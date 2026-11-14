//! Session-scoped shell monitors. Unified exec owns every process and terminal outcome.

mod archive;
mod delivery;
mod output;
mod recovery;
mod worker;

use self::worker::run_monitor_worker;
use super::session::Session;
use crate::unified_exec::MonitorArchiveBudget;
use crate::unified_exec::MonitorStopReason;
use crate::unified_exec::MonitorTaskStatus;
use crate::unified_exec::OutputHandles;
use crate::unified_exec::UnifiedExecProcess;
use codex_protocol::protocol::CommandMonitorInfo;
use codex_protocol::protocol::CommandMonitorTerminationReason;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum MonitorWorkerStop {
    Deadline,
    External(MonitorStopReason),
    Flood,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_command_monitor(
        self: &Arc<Self>,
        tool_use_id: String,
        event_turn_id: String,
        monitor: CommandMonitorInfo,
        output: OutputHandles,
        output_file: PathBuf,
        stop_rx: watch::Receiver<Option<MonitorStopReason>>,
        process: Arc<UnifiedExecProcess>,
        done_tx: oneshot::Sender<Option<CommandMonitorTerminationReason>>,
        worker_done: CancellationToken,
        archive_budget: Arc<MonitorArchiveBudget>,
    ) -> tokio::task::AbortHandle {
        let session = Arc::downgrade(self);
        let deadline_fired = Arc::new(AtomicBool::new(false));
        // Construct the completion guard before spawning so aborting an
        // unpolled task still reports why it stopped and signals that the
        // worker has been fully dropped.
        let completion_guard = MonitorWorkerCompletionGuard {
            done_tx: Some(done_tx),
            worker_done,
            deadline_fired: Arc::clone(&deadline_fired),
            stop_rx: stop_rx.clone(),
        };
        let worker = tokio::spawn(async move {
            let mut completion_guard = completion_guard;
            let termination_reason = run_command_monitor(
                session,
                tool_use_id,
                event_turn_id,
                monitor,
                output,
                output_file,
                stop_rx,
                process,
                deadline_fired,
                archive_budget,
            )
            .await;
            completion_guard.complete(termination_reason);
        });
        worker.abort_handle()
    }
}

struct MonitorWorkerCompletionGuard {
    done_tx: Option<oneshot::Sender<Option<CommandMonitorTerminationReason>>>,
    worker_done: CancellationToken,
    deadline_fired: Arc<AtomicBool>,
    stop_rx: watch::Receiver<Option<MonitorStopReason>>,
}

impl MonitorWorkerCompletionGuard {
    fn complete(&mut self, reason: Option<CommandMonitorTerminationReason>) {
        if let Some(done_tx) = self.done_tx.take() {
            let _ = done_tx.send(reason);
        }
    }
}

impl Drop for MonitorWorkerCompletionGuard {
    fn drop(&mut self) {
        if let Some(done_tx) = self.done_tx.take() {
            let reason = command_monitor_termination_reason(
                MonitorTaskStatus::Killed,
                self.deadline_fired.load(Ordering::Acquire),
                *self.stop_rx.borrow(),
            );
            let _ = done_tx.send(reason);
        }
        self.worker_done.cancel();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_command_monitor(
    session: Weak<Session>,
    tool_use_id: String,
    event_turn_id: String,
    monitor: CommandMonitorInfo,
    output: OutputHandles,
    output_file: PathBuf,
    stop_rx: watch::Receiver<Option<MonitorStopReason>>,
    process: Arc<UnifiedExecProcess>,
    deadline_fired: Arc<AtomicBool>,
    archive_budget: Arc<MonitorArchiveBudget>,
) -> Option<CommandMonitorTerminationReason> {
    let deadline_cancel = CancellationToken::new();
    let deadline_stop_error = Arc::new(StdMutex::new(None));
    let deadline_notify = Arc::new(Notify::new());
    if !monitor.persistent {
        spawn_deadline_supervisor(
            monitor.timeout_ms,
            Arc::clone(&process),
            deadline_cancel.clone(),
            Arc::clone(&deadline_fired),
            Arc::clone(&deadline_stop_error),
            Arc::clone(&deadline_notify),
            Arc::clone(&output.output_closed),
        );
    }

    let status_stop_rx = stop_rx.clone();
    let mut status = run_monitor_worker(
        &session,
        &tool_use_id,
        &event_turn_id,
        &monitor,
        &output,
        &output_file,
        stop_rx,
        &process,
        &deadline_fired,
        &deadline_stop_error,
        &deadline_notify,
        &deadline_cancel,
        archive_budget,
    )
    .await;
    deadline_cancel.cancel();
    if deadline_fired.load(Ordering::Acquire) {
        status = MonitorTaskStatus::Killed;
    }
    if (process.has_exited() || process.failure_message().is_some())
        && let Some(session) = session.upgrade()
    {
        session
            .services
            .unified_exec_manager
            .reap_monitor(&monitor.task_id, &process, status)
            .await;
    }
    command_monitor_termination_reason(
        status,
        deadline_fired.load(Ordering::Acquire),
        *status_stop_rx.borrow(),
    )
}

fn command_monitor_termination_reason(
    status: MonitorTaskStatus,
    deadline_fired: bool,
    stop_reason: Option<MonitorStopReason>,
) -> Option<CommandMonitorTerminationReason> {
    if deadline_fired {
        return Some(CommandMonitorTerminationReason::TimedOut);
    }
    match stop_reason {
        Some(MonitorStopReason::User) => Some(CommandMonitorTerminationReason::UserStopped),
        Some(MonitorStopReason::SessionShutdown) => {
            Some(CommandMonitorTerminationReason::SessionShutdown)
        }
        Some(MonitorStopReason::Capacity) => Some(CommandMonitorTerminationReason::Capacity),
        None if status == MonitorTaskStatus::Killed => {
            Some(CommandMonitorTerminationReason::Stopped)
        }
        None => None,
    }
}

fn spawn_deadline_supervisor(
    timeout_ms: u64,
    process: Arc<UnifiedExecProcess>,
    cancel: CancellationToken,
    deadline_fired: Arc<AtomicBool>,
    deadline_stop_error: Arc<StdMutex<Option<String>>>,
    deadline_notify: Arc<Notify>,
    output_closed: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {}
        }
        let monitor_stop_guard = process.begin_monitor_stop();
        if process.has_exited() && output_closed.load(Ordering::Acquire) {
            return;
        }

        match process.terminate_confirmed().await {
            Ok(()) => deadline_fired.store(true, Ordering::Release),
            Err(err) => {
                *deadline_stop_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(err.to_string());
            }
        }
        deadline_notify.notify_waiters();
        drop(monitor_stop_guard);
    });
}

#[cfg(test)]
#[path = "command_monitor_tests.rs"]
mod tests;
