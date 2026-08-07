use super::input_queue::TurnInput;
use super::session::Session;
use crate::codex_thread::TryStartTurnIfIdleRejectionReason;
use crate::context::push_xml_escaped_text;
use crate::unified_exec::MonitorArchiveBudget;
use crate::unified_exec::MonitorCaptureReceiver;
use crate::unified_exec::MonitorOutputChunk;
use crate::unified_exec::MonitorStopReason;
use crate::unified_exec::MonitorTaskStatus;
use crate::unified_exec::OutputHandles;
use crate::unified_exec::UnifiedExecProcess;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CommandMonitorInfo;
use codex_protocol::protocol::CommandMonitorTerminationReason;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecOutputStream;
use std::future::Future;
use std::future::pending;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_PENDING_UTF16_UNITS: usize = 1024 * 1024;
const MAX_LINE_UTF16_UNITS: usize = 500;
const MAX_BATCH_UTF16_UNITS: usize = 3000;
const BATCH_WINDOW: Duration = Duration::from_millis(200);
const RATE_LIMIT_CAPACITY: u32 = 10;
const RATE_LIMIT_REFILL_INTERVAL: Duration = Duration::from_secs(2);
const OVERLOAD_STOP_AFTER: Duration = Duration::from_secs(30);
const OVERLOAD_QUIET_RESET_AFTER: Duration = Duration::from_secs(6);
const TRUNCATION_SUFFIX: &str = "...(truncated)";
const ARCHIVE_DRAIN_CHUNKS_PER_PASS: usize = 64;
const MAX_ARCHIVE_BYTES: u64 = crate::unified_exec::MAX_MONITOR_ARCHIVE_BYTES;
const ARCHIVE_TRUNCATION_MARKER: &[u8] = b"\n[output truncated: exceeded 5GB disk cap]\n";
const ARCHIVE_CAPTURE_GAP_MARKER: &[u8] =
    b"\n[output omitted: monitor capture buffer was full while stopping]\n";

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

#[allow(clippy::too_many_arguments)]
async fn run_monitor_worker(
    session: &Weak<Session>,
    tool_use_id: &str,
    event_turn_id: &str,
    monitor: &CommandMonitorInfo,
    output: &OutputHandles,
    output_file: &Path,
    mut stop_rx: watch::Receiver<Option<MonitorStopReason>>,
    process: &Arc<UnifiedExecProcess>,
    deadline_fired: &Arc<AtomicBool>,
    deadline_stop_error: &Arc<StdMutex<Option<String>>>,
    deadline_notify: &Arc<Notify>,
    deadline_cancel: &CancellationToken,
    archive_budget: Arc<MonitorArchiveBudget>,
) -> MonitorTaskStatus {
    let Some(mut archive_rx) = output.take_monitor_capture() else {
        return run_fatal_monitor_recovery(
            session,
            monitor,
            tool_use_id,
            event_turn_id,
            process,
            &output.output_closed_notify,
            /*archive_rx*/ None,
            stop_rx,
            deadline_fired,
            deadline_stop_error,
            deadline_notify,
            "monitor output capture was not initialized".to_string(),
        )
        .await;
    };
    let mut file = match create_output_file(output_file).await {
        Ok(file) => MonitorArchiveWriter::new(file, archive_budget),
        Err(err) => {
            if deadline_fired.load(Ordering::Acquire) {
                deliver_monitor_timeout(session, monitor, tool_use_id, event_turn_id).await;
                return MonitorTaskStatus::Killed;
            }
            return run_fatal_monitor_recovery(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                process,
                &output.output_closed_notify,
                Some(archive_rx),
                stop_rx,
                deadline_fired,
                deadline_stop_error,
                deadline_notify,
                format!("failed to create monitor output file: {err}"),
            )
            .await;
        }
    };

    let mut lines = PhysicalLineFramer::default();
    let mut batch = MonitorBatch::default();
    let mut limiter = MonitorRateLimiter::new(Instant::now());
    let mut produced_output = false;
    let mut stdout_finished = false;
    let mut archive_finished = false;
    let mut stop_kind = None;
    let mut stop_flushed = false;
    let mut stop_rx_open = true;
    let mut process_exit_observed = false;

    loop {
        let output_closed_notified = output.output_closed_notify.notified();
        let deadline_notified = deadline_notify.notified();
        tokio::pin!(output_closed_notified);
        tokio::pin!(deadline_notified);
        output_closed_notified.as_mut().enable();
        deadline_notified.as_mut().enable();

        let deadline_error = deadline_stop_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(err) = deadline_error {
            deliver_monitor_event(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                &format!(
                    "[Monitor timeout reached, but the process could not be stopped: {err}. The monitor is still running; use TaskStop to retry.]"
                ),
            )
            .await;
        }

        match drain_archive_available(
            &mut archive_rx,
            &mut file,
            &mut lines,
            &mut batch,
            &mut produced_output,
            stop_kind.is_none() && !stdout_finished,
        )
        .await
        {
            Ok(closed) => archive_finished |= closed,
            Err(err) => {
                if deadline_fired.load(Ordering::Acquire) || stop_kind.is_some() {
                    return MonitorTaskStatus::Killed;
                }
                return run_fatal_monitor_recovery(
                    session,
                    monitor,
                    tool_use_id,
                    event_turn_id,
                    process,
                    &output.output_closed_notify,
                    Some(archive_rx),
                    stop_rx,
                    deadline_fired,
                    deadline_stop_error,
                    deadline_notify,
                    format!("failed to write monitor output file: {err}"),
                )
                .await;
            }
        }

        if deadline_fired.load(Ordering::Acquire) && stop_kind.is_none() {
            stop_kind = Some(MonitorWorkerStop::Deadline);
        }
        if stop_kind.is_none()
            && let Some(reason) = *stop_rx.borrow()
        {
            stop_kind = Some(MonitorWorkerStop::External(reason));
        }
        if archive_finished
            && process.output_task_finished()
            && !process.output_completed_normally()
            && stop_kind.is_none()
        {
            let message = process.failure_message().map_or_else(
                || "monitor output capture ended before output completed normally".to_string(),
                |failure| format!("monitor output task failed: {failure}"),
            );
            return run_fatal_monitor_recovery(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                process,
                &output.output_closed_notify,
                Some(archive_rx),
                stop_rx,
                deadline_fired,
                deadline_stop_error,
                deadline_notify,
                message,
            )
            .await;
        }
        if archive_finished && !stdout_finished && stop_kind.is_none() {
            finish_physical_lines(&mut lines, &mut batch, Instant::now());
            stdout_finished = true;
        }

        if let Some(kind) = stop_kind
            && !stop_flushed
        {
            if matches!(
                kind,
                MonitorWorkerStop::Flood
                    | MonitorWorkerStop::External(
                        MonitorStopReason::SessionShutdown | MonitorStopReason::Capacity
                    )
            ) {
                let _ = lines.finish();
                let _ = batch.take();
            } else {
                finish_physical_lines(&mut lines, &mut batch, Instant::now());
                if let Some(text) = batch.take() {
                    let _ = dispatch_batch(
                        session,
                        monitor,
                        tool_use_id,
                        event_turn_id,
                        &mut limiter,
                        text,
                        Instant::now(),
                        /*stop_on_overload*/ false,
                    )
                    .await;
                }
                if let Some(text) = limiter.take_suppression_notice() {
                    deliver_monitor_event(session, monitor, tool_use_id, event_turn_id, &text)
                        .await;
                }
            }
            if matches!(kind, MonitorWorkerStop::Deadline) {
                deliver_monitor_timeout(session, monitor, tool_use_id, event_turn_id).await;
            }
            if matches!(kind, MonitorWorkerStop::External(MonitorStopReason::User)) {
                deliver_monitor_event(
                    session,
                    monitor,
                    tool_use_id,
                    event_turn_id,
                    "[Monitor stopped]",
                )
                .await;
            }
            stdout_finished = true;
            stop_flushed = true;
        }

        let now = Instant::now();
        if stop_kind.is_none()
            && batch.deadline().is_some_and(|deadline| deadline <= now)
            && let Some(text) = batch.take()
            && let Some(stop_message) = dispatch_batch(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                &mut limiter,
                text,
                now,
                /*stop_on_overload*/ true,
            )
            .await
        {
            match stop_process_for_flood(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                process,
                &mut archive_rx,
                &mut file,
                &stop_message,
            )
            .await
            {
                Ok(()) => {
                    stop_kind = Some(MonitorWorkerStop::Flood);
                    continue;
                }
                Err(FloodStopError::Termination(_)) => {
                    return run_degraded_monitor(
                        session,
                        monitor,
                        tool_use_id,
                        event_turn_id,
                        process,
                        &output.output_closed_notify,
                        Some(archive_rx),
                        Some(file),
                        stop_rx,
                        deadline_fired,
                        deadline_stop_error,
                        deadline_notify,
                        DegradedMonitor::Flood { stop_message },
                    )
                    .await;
                }
                Err(FloodStopError::Archive(err)) => {
                    return run_fatal_monitor_recovery(
                        session,
                        monitor,
                        tool_use_id,
                        event_turn_id,
                        process,
                        &output.output_closed_notify,
                        Some(archive_rx),
                        stop_rx,
                        deadline_fired,
                        deadline_stop_error,
                        deadline_notify,
                        format!("failed to write monitor output file: {err}"),
                    )
                    .await;
                }
            }
        }
        if monitor_worker_ready_to_finalize(
            stdout_finished,
            archive_finished,
            process.has_exited(),
            process.output_completed_normally(),
            stop_kind.is_some(),
        ) {
            if stop_kind.is_none() {
                deadline_cancel.cancel();
                process.wait_for_monitor_stop_resolution().await;
                if let Some(reason) = *stop_rx.borrow() {
                    stop_kind = Some(MonitorWorkerStop::External(reason));
                    continue;
                }
            }
            if let Err(err) = file.flush().await
                && stop_kind.is_none()
                && !deadline_fired.load(Ordering::Acquire)
            {
                return run_fatal_monitor_recovery(
                    session,
                    monitor,
                    tool_use_id,
                    event_turn_id,
                    process,
                    &output.output_closed_notify,
                    Some(archive_rx),
                    stop_rx,
                    deadline_fired,
                    deadline_stop_error,
                    deadline_notify,
                    format!("failed to flush monitor output file: {err}"),
                )
                .await;
            }
            if stop_kind.is_some() || deadline_fired.load(Ordering::Acquire) {
                return MonitorTaskStatus::Killed;
            }

            if let Some(text) = batch.take()
                && let Some(stop_message) = dispatch_batch(
                    session,
                    monitor,
                    tool_use_id,
                    event_turn_id,
                    &mut limiter,
                    text,
                    Instant::now(),
                    /*stop_on_overload*/ true,
                )
                .await
            {
                return match stop_process_for_flood(
                    session,
                    monitor,
                    tool_use_id,
                    event_turn_id,
                    process,
                    &mut archive_rx,
                    &mut file,
                    &stop_message,
                )
                .await
                {
                    Ok(()) => MonitorTaskStatus::Killed,
                    Err(FloodStopError::Termination(_)) => {
                        return run_degraded_monitor(
                            session,
                            monitor,
                            tool_use_id,
                            event_turn_id,
                            process,
                            &output.output_closed_notify,
                            Some(archive_rx),
                            Some(file),
                            stop_rx,
                            deadline_fired,
                            deadline_stop_error,
                            deadline_notify,
                            DegradedMonitor::Flood { stop_message },
                        )
                        .await;
                    }
                    Err(FloodStopError::Archive(err)) => {
                        return run_fatal_monitor_recovery(
                            session,
                            monitor,
                            tool_use_id,
                            event_turn_id,
                            process,
                            &output.output_closed_notify,
                            Some(archive_rx),
                            stop_rx,
                            deadline_fired,
                            deadline_stop_error,
                            deadline_notify,
                            format!("failed to write monitor output file: {err}"),
                        )
                        .await;
                    }
                };
            }
            if let Some(text) = limiter.take_suppression_notice() {
                deliver_monitor_event(session, monitor, tool_use_id, event_turn_id, &text).await;
            }
            if deadline_fired.load(Ordering::Acquire) || stop_rx.borrow().is_some() {
                return MonitorTaskStatus::Killed;
            }

            let exit_code = process.exit_code();
            let failure_message = process.failure_message();
            let failed = failure_message.is_some() || exit_code.is_some_and(|code| code != 0);
            let status = if failed {
                MonitorTaskStatus::Failed
            } else {
                MonitorTaskStatus::Completed
            };
            let Some(active_session) = session.upgrade() else {
                return status;
            };
            let claim = active_session
                .services
                .unified_exec_manager
                .claim_monitor_completion(&monitor.task_id, process, status)
                .await;
            if claim.is_stop_pending() {
                process.wait_for_monitor_stop_resolution().await;
                continue;
            }
            if !claim.is_claimed() {
                return MonitorTaskStatus::Killed;
            }
            deliver_monitor_completion(
                session,
                monitor,
                tool_use_id,
                output_file,
                exit_code,
                failure_message,
                produced_output,
            )
            .await;
            return status;
        }

        let process_exited = process.cancellation_token().cancelled_owned();
        tokio::pin!(process_exited);
        tokio::select! {
            _ = &mut output_closed_notified, if !process.output_task_finished() => {}
            _ = &mut deadline_notified, if stop_kind.is_none() => {}
            chunk = archive_rx.recv(), if !archive_finished => {
                match chunk {
                    Some(chunk) => {
                        if let Err(err) = handle_capture_chunk(
                            chunk,
                            &mut file,
                            &mut lines,
                            &mut batch,
                            &mut produced_output,
                            stop_kind.is_none() && !stdout_finished,
                        ).await {
                            if deadline_fired.load(Ordering::Acquire) || stop_kind.is_some() {
                                return MonitorTaskStatus::Killed;
                            }
                            return run_fatal_monitor_recovery(
                                session,
                                monitor,
                                tool_use_id,
                                event_turn_id,
                                process,
                                &output.output_closed_notify,
                                Some(archive_rx),
                                stop_rx,
                                deadline_fired,
                                deadline_stop_error,
                                deadline_notify,
                                format!("failed to write monitor output file: {err}"),
                            ).await;
                        }
                    }
                    None => {
                        archive_finished = true;
                    }
                }
            }
            _ = wait_until(batch.deadline()), if stop_kind.is_none() => {}
            changed = stop_rx.changed(), if stop_kind.is_none() && stop_rx_open => {
                if changed.is_err() {
                    stop_rx_open = false;
                }
            }
            _ = &mut process_exited, if stop_kind.is_none() && !process_exit_observed => {
                process_exit_observed = true;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum MonitorWorkerStop {
    Deadline,
    External(MonitorStopReason),
    Flood,
}

fn monitor_worker_ready_to_finalize(
    stdout_finished: bool,
    archive_finished: bool,
    process_exited: bool,
    output_completed_normally: bool,
    stopped: bool,
) -> bool {
    stdout_finished
        && archive_finished
        && (stopped || (process_exited && output_completed_normally))
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending::<()>().await,
    }
}

fn ingest_stdout(
    bytes: &[u8],
    lines: &mut PhysicalLineFramer,
    batch: &mut MonitorBatch,
    produced_output: &mut bool,
    now: Instant,
) {
    if !bytes.is_empty() {
        *produced_output = true;
    }
    for line in lines.push(bytes) {
        batch.push_line(&line, now);
    }
}

fn finish_physical_lines(lines: &mut PhysicalLineFramer, batch: &mut MonitorBatch, now: Instant) {
    if let Some(line) = lines.finish() {
        batch.push_line(&line, now);
    }
}

async fn drain_archive_available(
    archive_rx: &mut MonitorCaptureReceiver,
    file: &mut MonitorArchiveWriter,
    lines: &mut PhysicalLineFramer,
    batch: &mut MonitorBatch,
    produced_output: &mut bool,
    frame_stdout: bool,
) -> io::Result<bool> {
    for _ in 0..ARCHIVE_DRAIN_CHUNKS_PER_PASS {
        match archive_rx.try_recv() {
            Ok(chunk) => {
                handle_capture_chunk(chunk, file, lines, batch, produced_output, frame_stdout)
                    .await?
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(false),
            Err(mpsc::error::TryRecvError::Disconnected) => return Ok(true),
        }
    }
    Ok(false)
}

async fn handle_capture_chunk(
    chunk: MonitorOutputChunk,
    file: &mut MonitorArchiveWriter,
    lines: &mut PhysicalLineFramer,
    batch: &mut MonitorBatch,
    produced_output: &mut bool,
    frame_stdout: bool,
) -> io::Result<()> {
    match chunk {
        MonitorOutputChunk::StdoutClosed => {
            if !frame_stdout {
                return Ok(());
            }
            let now = Instant::now();
            finish_physical_lines(lines, batch, now);
            batch.request_flush(now);
            Ok(())
        }
        MonitorOutputChunk::ArchiveGap => file.write_capture_gap_marker().await,
        MonitorOutputChunk::Output { bytes, is_stdout } => {
            file.write_chunk(&bytes).await?;
            if frame_stdout && is_stdout {
                ingest_stdout(&bytes, lines, batch, produced_output, Instant::now());
            }
            Ok(())
        }
    }
}

struct MonitorArchiveWriter<W = tokio::fs::File> {
    file: W,
    bytes_written: u64,
    cap: u64,
    truncated: bool,
    capture_gap_written: bool,
    archive_budget: Arc<MonitorArchiveBudget>,
}

impl MonitorArchiveWriter<tokio::fs::File> {
    fn new(file: tokio::fs::File, archive_budget: Arc<MonitorArchiveBudget>) -> Self {
        Self::with_cap_and_budget(file, MAX_ARCHIVE_BYTES, archive_budget)
    }
}

impl<W> MonitorArchiveWriter<W>
where
    W: AsyncWrite + Unpin,
{
    fn with_cap_and_budget(file: W, cap: u64, archive_budget: Arc<MonitorArchiveBudget>) -> Self {
        Self {
            file,
            bytes_written: 0,
            cap,
            truncated: false,
            capture_gap_written: false,
            archive_budget,
        }
    }

    async fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        if self.truncated {
            return Ok(());
        }
        let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        let Some(mut reservation) = self.archive_budget.reserve(chunk_len) else {
            self.file.write_all(ARCHIVE_TRUNCATION_MARKER).await?;
            self.truncated = true;
            return Ok(());
        };
        if self.bytes_written.saturating_add(chunk_len) > self.cap {
            drop(reservation);
            self.file.write_all(ARCHIVE_TRUNCATION_MARKER).await?;
            self.truncated = true;
            return Ok(());
        }
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let written = self.file.write(remaining).await?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write command monitor archive chunk",
                ));
            }
            let written_u64 = u64::try_from(written).unwrap_or(u64::MAX);
            reservation.commit_written(written_u64);
            self.bytes_written = self.bytes_written.saturating_add(written_u64);
            remaining = &remaining[written..];
        }
        Ok(())
    }

    async fn write_capture_gap_marker(&mut self) -> io::Result<()> {
        if !self.capture_gap_written {
            self.file.write_all(ARCHIVE_CAPTURE_GAP_MARKER).await?;
            self.capture_gap_written = true;
        }
        Ok(())
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.file.flush().await
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_batch(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
    limiter: &mut MonitorRateLimiter,
    batch: String,
    now: Instant,
    stop_on_overload: bool,
) -> Option<String> {
    let dispatch = limiter.submit(batch, now, stop_on_overload);
    for text in dispatch.deliveries {
        deliver_monitor_event(session, monitor, tool_use_id, event_turn_id, &text).await;
    }
    dispatch.stop_message
}

#[allow(clippy::too_many_arguments)]
async fn stop_process_for_flood(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
    process: &Arc<UnifiedExecProcess>,
    archive_rx: &mut MonitorCaptureReceiver,
    archive_file: &mut MonitorArchiveWriter,
    stop_message: &str,
) -> Result<(), FloodStopError> {
    let monitor_stop_guard = process.begin_monitor_stop();
    let result = if process.has_exited() && process.output_completed_normally() {
        Ok(())
    } else {
        await_while_archiving_capture(process.terminate_confirmed(), archive_rx, archive_file).await
    };
    drop(monitor_stop_guard);
    match result {
        Ok(()) => {
            deliver_monitor_event(session, monitor, tool_use_id, event_turn_id, stop_message).await;
            Ok(())
        }
        Err(FloodStopError::Termination(err)) => {
            deliver_monitor_event(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                &format!(
                    "failed to stop monitor after excessive output; task remains running: {err}"
                ),
            )
            .await;
            Err(FloodStopError::Termination(err))
        }
        Err(err @ FloodStopError::Archive(_)) => Err(err),
    }
}

#[derive(Debug)]
enum FloodStopError {
    Termination(String),
    Archive(io::Error),
}

enum DegradedMonitor {
    Fatal { message: String },
    Flood { stop_message: String },
}

#[allow(clippy::too_many_arguments)]
async fn run_fatal_monitor_recovery(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
    process: &Arc<UnifiedExecProcess>,
    output_task_notify: &Arc<Notify>,
    mut archive_rx: Option<MonitorCaptureReceiver>,
    stop_rx: watch::Receiver<Option<MonitorStopReason>>,
    deadline_fired: &Arc<AtomicBool>,
    deadline_stop_error: &Arc<StdMutex<Option<String>>>,
    deadline_notify: &Arc<Notify>,
    message: String,
) -> MonitorTaskStatus {
    if deadline_fired.load(Ordering::Acquire) {
        deliver_monitor_timeout(session, monitor, tool_use_id, event_turn_id).await;
        return MonitorTaskStatus::Killed;
    }
    if stop_rx.borrow().is_some() {
        return MonitorTaskStatus::Killed;
    }

    match confirm_fatal_monitor_stop(process, archive_rx.as_mut()).await {
        Ok(()) => {
            return claim_and_deliver_fatal_monitor(
                session,
                monitor,
                tool_use_id,
                process,
                &message,
                deadline_fired,
            )
            .await;
        }
        Err(err) => {
            deliver_monitor_event(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                &format!(
                    "[Monitor error: {message}. The process could not be stopped ({err}); the task remains running. Use TaskStop to retry.]"
                ),
            )
            .await;
        }
    }

    run_degraded_monitor(
        session,
        monitor,
        tool_use_id,
        event_turn_id,
        process,
        output_task_notify,
        archive_rx,
        /*archive_file*/ None,
        stop_rx,
        deadline_fired,
        deadline_stop_error,
        deadline_notify,
        DegradedMonitor::Fatal { message },
    )
    .await
}

async fn confirm_fatal_monitor_stop(
    process: &Arc<UnifiedExecProcess>,
    archive_rx: Option<&mut MonitorCaptureReceiver>,
) -> Result<(), crate::unified_exec::UnifiedExecError> {
    let monitor_stop_guard = process.begin_monitor_stop();
    let result = if process.has_exited() && process.output_completed_normally() {
        Ok(())
    } else {
        await_while_draining_capture(process.terminate_confirmed(), archive_rx).await
    };
    // Fatal completion is claimed only after this task's stop guard is gone.
    // A concurrent TaskStop, deadline, or teardown guard therefore wins the
    // manager's atomic claim instead of receiving contradictory terminal XML.
    drop(monitor_stop_guard);
    result
}

async fn claim_and_deliver_fatal_monitor(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    process: &Arc<UnifiedExecProcess>,
    message: &str,
    deadline_fired: &Arc<AtomicBool>,
) -> MonitorTaskStatus {
    loop {
        if deadline_fired.load(Ordering::Acquire) {
            return MonitorTaskStatus::Killed;
        }
        let Some(active_session) = session.upgrade() else {
            return MonitorTaskStatus::Failed;
        };
        let claim = active_session
            .services
            .unified_exec_manager
            .claim_monitor_completion(&monitor.task_id, process, MonitorTaskStatus::Failed)
            .await;
        if claim.is_stop_pending() {
            process.wait_for_monitor_stop_resolution().await;
            continue;
        }
        if !claim.is_claimed() {
            return MonitorTaskStatus::Killed;
        }
        break;
    }
    deliver_monitor_failure(session, monitor, tool_use_id, message).await;
    MonitorTaskStatus::Failed
}

#[allow(clippy::too_many_arguments)]
async fn run_degraded_monitor(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
    process: &Arc<UnifiedExecProcess>,
    output_task_notify: &Arc<Notify>,
    mut archive_rx: Option<MonitorCaptureReceiver>,
    mut archive_file: Option<MonitorArchiveWriter>,
    mut stop_rx: watch::Receiver<Option<MonitorStopReason>>,
    deadline_fired: &Arc<AtomicBool>,
    deadline_stop_error: &Arc<StdMutex<Option<String>>>,
    deadline_notify: &Arc<Notify>,
    mut degraded: DegradedMonitor,
) -> MonitorTaskStatus {
    let mut capture_finished = archive_rx.is_none();
    let mut stop_kind = None;
    let mut stop_rx_open = true;
    let mut process_exit_observed = false;
    let mut retried_abnormal_terminal_state = false;
    let mut flood_stop_confirmed = false;

    loop {
        let output_task_notified = output_task_notify.notified();
        let deadline_notified = deadline_notify.notified();
        tokio::pin!(output_task_notified);
        tokio::pin!(deadline_notified);
        output_task_notified.as_mut().enable();
        deadline_notified.as_mut().enable();

        let deadline_error = deadline_stop_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(err) = deadline_error {
            deliver_monitor_event(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                &format!(
                    "[Monitor timeout reached, but the process could not be stopped: {err}. The monitor is still running; use TaskStop to retry.]"
                ),
            )
            .await;
        }

        if !capture_finished {
            match drain_degraded_capture(&mut archive_rx, &mut archive_file).await {
                Ok(closed) => capture_finished |= closed,
                Err(err) => {
                    let message = format!("failed to write monitor output file: {err}");
                    archive_file = None;
                    match confirm_fatal_monitor_stop(process, archive_rx.as_mut()).await {
                        Ok(()) => {
                            return claim_and_deliver_fatal_monitor(
                                session,
                                monitor,
                                tool_use_id,
                                process,
                                &message,
                                deadline_fired,
                            )
                            .await;
                        }
                        Err(stop_err) => {
                            deliver_monitor_event(
                                session,
                                monitor,
                                tool_use_id,
                                event_turn_id,
                                &format!(
                                    "[Monitor error: {message}. The process could not be stopped ({stop_err}); the task remains running. Use TaskStop to retry.]"
                                ),
                            )
                            .await;
                            degraded = DegradedMonitor::Fatal { message };
                            retried_abnormal_terminal_state = true;
                        }
                    }
                }
            }
        }

        if deadline_fired.load(Ordering::Acquire) && stop_kind.is_none() {
            stop_kind = Some(MonitorWorkerStop::Deadline);
        }
        if stop_kind.is_none()
            && let Some(reason) = *stop_rx.borrow()
        {
            stop_kind = Some(MonitorWorkerStop::External(reason));
        }

        if stop_kind.is_some() && capture_finished {
            if let Some(file) = archive_file.as_mut() {
                let _ = file.flush().await;
            }
            if matches!(stop_kind, Some(MonitorWorkerStop::Deadline)) {
                deliver_monitor_timeout(session, monitor, tool_use_id, event_turn_id).await;
            }
            return MonitorTaskStatus::Killed;
        }

        if flood_stop_confirmed && capture_finished {
            if let Some(file) = archive_file.as_mut()
                && let Err(err) = file.flush().await
            {
                let message = format!("failed to flush monitor output file: {err}");
                return claim_and_deliver_fatal_monitor(
                    session,
                    monitor,
                    tool_use_id,
                    process,
                    &message,
                    deadline_fired,
                )
                .await;
            }
            return claim_and_deliver_flood_stop(
                session,
                monitor,
                tool_use_id,
                event_turn_id,
                process,
                &degraded,
                deadline_fired,
            )
            .await;
        }

        if process.has_exited() && process.output_completed_normally() && capture_finished {
            if let Some(file) = archive_file.as_mut()
                && let Err(err) = file.flush().await
            {
                let message = format!("failed to flush monitor output file: {err}");
                return claim_and_deliver_fatal_monitor(
                    session,
                    monitor,
                    tool_use_id,
                    process,
                    &message,
                    deadline_fired,
                )
                .await;
            }
            return match &degraded {
                DegradedMonitor::Fatal { message } => {
                    claim_and_deliver_fatal_monitor(
                        session,
                        monitor,
                        tool_use_id,
                        process,
                        message,
                        deadline_fired,
                    )
                    .await
                }
                DegradedMonitor::Flood { .. } => {
                    claim_and_deliver_flood_stop(
                        session,
                        monitor,
                        tool_use_id,
                        event_turn_id,
                        process,
                        &degraded,
                        deadline_fired,
                    )
                    .await
                }
            };
        }

        if !retried_abnormal_terminal_state
            && process.has_exited()
            && process.output_task_finished()
            && !process.output_completed_normally()
        {
            retried_abnormal_terminal_state = true;
            match &degraded {
                DegradedMonitor::Fatal { message } => {
                    match confirm_fatal_monitor_stop(process, archive_rx.as_mut()).await {
                        Ok(()) => {
                            return claim_and_deliver_fatal_monitor(
                                session,
                                monitor,
                                tool_use_id,
                                process,
                                message,
                                deadline_fired,
                            )
                            .await;
                        }
                        Err(err) => {
                            deliver_monitor_event(
                                session,
                                monitor,
                                tool_use_id,
                                event_turn_id,
                                &format!(
                                    "[Monitor process exited, but cleanup could not be confirmed: {err}. Use TaskStop to retry.]"
                                ),
                            )
                            .await;
                        }
                    }
                }
                DegradedMonitor::Flood { .. } => {
                    let Some(file) = archive_file.as_mut() else {
                        continue;
                    };
                    match attempt_flood_stop(process, archive_rx.as_mut(), file).await {
                        Ok(()) => flood_stop_confirmed = true,
                        Err(FloodStopError::Termination(err)) => {
                            deliver_monitor_event(
                                session,
                                monitor,
                                tool_use_id,
                                event_turn_id,
                                &format!(
                                    "failed to stop monitor after excessive output; task remains running: {err}"
                                ),
                            )
                            .await;
                        }
                        Err(FloodStopError::Archive(err)) => {
                            let message = format!("failed to write monitor output file: {err}");
                            archive_file = None;
                            match confirm_fatal_monitor_stop(process, archive_rx.as_mut()).await {
                                Ok(()) => {
                                    return claim_and_deliver_fatal_monitor(
                                        session,
                                        monitor,
                                        tool_use_id,
                                        process,
                                        &message,
                                        deadline_fired,
                                    )
                                    .await;
                                }
                                Err(stop_err) => {
                                    deliver_monitor_event(
                                        session,
                                        monitor,
                                        tool_use_id,
                                        event_turn_id,
                                        &format!(
                                            "[Monitor error: {message}. The process could not be stopped ({stop_err}); the task remains running. Use TaskStop to retry.]"
                                        ),
                                    )
                                    .await;
                                    degraded = DegradedMonitor::Fatal { message };
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }

        let process_exited = process.cancellation_token().cancelled_owned();
        tokio::pin!(process_exited);
        tokio::select! {
            _ = &mut output_task_notified => {}
            _ = &mut deadline_notified, if stop_kind.is_none() => {}
            chunk = receive_degraded_capture(&mut archive_rx), if !capture_finished => {
                match chunk {
                    Some(chunk) => {
                        if let Some(file) = archive_file.as_mut()
                            && let Err(err) = archive_capture_chunk(chunk, file).await
                        {
                            let message = format!("failed to write monitor output file: {err}");
                            archive_file = None;
                            match confirm_fatal_monitor_stop(process, archive_rx.as_mut()).await {
                                Ok(()) => {
                                    return claim_and_deliver_fatal_monitor(
                                        session,
                                        monitor,
                                        tool_use_id,
                                        process,
                                        &message,
                                        deadline_fired,
                                    )
                                    .await;
                                }
                                Err(stop_err) => {
                                    deliver_monitor_event(
                                        session,
                                        monitor,
                                        tool_use_id,
                                        event_turn_id,
                                        &format!(
                                            "[Monitor error: {message}. The process could not be stopped ({stop_err}); the task remains running. Use TaskStop to retry.]"
                                        ),
                                    )
                                    .await;
                                    degraded = DegradedMonitor::Fatal { message };
                                    retried_abnormal_terminal_state = true;
                                }
                            }
                        }
                    }
                    None => capture_finished = true,
                }
            }
            changed = stop_rx.changed(), if stop_kind.is_none() && stop_rx_open => {
                if changed.is_err() {
                    stop_rx_open = false;
                }
            }
            _ = &mut process_exited, if !process_exit_observed => {
                process_exit_observed = true;
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

async fn claim_and_deliver_flood_stop(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
    process: &Arc<UnifiedExecProcess>,
    degraded: &DegradedMonitor,
    deadline_fired: &Arc<AtomicBool>,
) -> MonitorTaskStatus {
    let DegradedMonitor::Flood { stop_message, .. } = degraded else {
        return MonitorTaskStatus::Failed;
    };
    loop {
        if deadline_fired.load(Ordering::Acquire) {
            return MonitorTaskStatus::Killed;
        }
        let Some(active_session) = session.upgrade() else {
            return MonitorTaskStatus::Killed;
        };
        let claim = active_session
            .services
            .unified_exec_manager
            .claim_monitor_completion(&monitor.task_id, process, MonitorTaskStatus::Killed)
            .await;
        if claim.is_stop_pending() {
            process.wait_for_monitor_stop_resolution().await;
            continue;
        }
        if !claim.is_claimed() {
            return MonitorTaskStatus::Killed;
        }
        break;
    }
    deliver_monitor_event(session, monitor, tool_use_id, event_turn_id, stop_message).await;
    MonitorTaskStatus::Killed
}

async fn attempt_flood_stop(
    process: &Arc<UnifiedExecProcess>,
    archive_rx: Option<&mut MonitorCaptureReceiver>,
    archive_file: &mut MonitorArchiveWriter,
) -> Result<(), FloodStopError> {
    let monitor_stop_guard = process.begin_monitor_stop();
    let result = if process.has_exited() && process.output_completed_normally() {
        Ok(())
    } else if let Some(archive_rx) = archive_rx {
        await_while_archiving_capture(process.terminate_confirmed(), archive_rx, archive_file).await
    } else {
        process
            .terminate_confirmed()
            .await
            .map_err(|err| FloodStopError::Termination(err.to_string()))
    };
    drop(monitor_stop_guard);
    result
}

async fn drain_degraded_capture(
    archive_rx: &mut Option<MonitorCaptureReceiver>,
    archive_file: &mut Option<MonitorArchiveWriter>,
) -> io::Result<bool> {
    let Some(archive_rx) = archive_rx.as_mut() else {
        return Ok(true);
    };
    for _ in 0..ARCHIVE_DRAIN_CHUNKS_PER_PASS {
        match archive_rx.try_recv() {
            Ok(chunk) => {
                if let Some(file) = archive_file.as_mut() {
                    archive_capture_chunk(chunk, file).await?;
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(false),
            Err(mpsc::error::TryRecvError::Disconnected) => return Ok(true),
        }
    }
    Ok(false)
}

async fn receive_degraded_capture(
    archive_rx: &mut Option<MonitorCaptureReceiver>,
) -> Option<MonitorOutputChunk> {
    match archive_rx.as_mut() {
        Some(archive_rx) => archive_rx.recv().await,
        None => pending::<Option<MonitorOutputChunk>>().await,
    }
}

async fn await_while_draining_capture<F>(
    termination: F,
    archive_rx: Option<&mut MonitorCaptureReceiver>,
) -> Result<(), crate::unified_exec::UnifiedExecError>
where
    F: Future<Output = Result<(), crate::unified_exec::UnifiedExecError>>,
{
    let Some(archive_rx) = archive_rx else {
        return termination.await;
    };
    tokio::pin!(termination);
    loop {
        tokio::select! {
            result = termination.as_mut() => return result,
            chunk = archive_rx.recv() => {
                if chunk.is_none() {
                    return termination.as_mut().await;
                }
            }
        }
    }
}

async fn await_while_archiving_capture<F>(
    termination: F,
    archive_rx: &mut MonitorCaptureReceiver,
    archive_file: &mut MonitorArchiveWriter,
) -> Result<(), FloodStopError>
where
    F: Future<Output = Result<(), crate::unified_exec::UnifiedExecError>>,
{
    tokio::pin!(termination);
    loop {
        tokio::select! {
            result = termination.as_mut() => {
                return result.map_err(|err| FloodStopError::Termination(err.to_string()));
            }
            chunk = archive_rx.recv() => {
                match chunk {
                    Some(chunk) => {
                        archive_capture_chunk(chunk, archive_file)
                            .await
                            .map_err(FloodStopError::Archive)?;
                    }
                    None => {
                        return termination
                            .as_mut()
                            .await
                            .map_err(|err| FloodStopError::Termination(err.to_string()));
                    }
                }
            }
        }
    }
}

async fn archive_capture_chunk(
    chunk: MonitorOutputChunk,
    archive_file: &mut MonitorArchiveWriter,
) -> io::Result<()> {
    match chunk {
        MonitorOutputChunk::Output { bytes, .. } => archive_file.write_chunk(&bytes).await,
        MonitorOutputChunk::StdoutClosed => Ok(()),
        MonitorOutputChunk::ArchiveGap => archive_file.write_capture_gap_marker().await,
    }
}

async fn create_output_file(path: &Path) -> io::Result<tokio::fs::File> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "output file has no parent directory",
        )
    })?;
    tokio::fs::create_dir_all(parent).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).await
}

#[derive(Default)]
struct PhysicalLineFramer {
    pending: String,
    pending_start: usize,
    pending_utf16_units: usize,
    undecoded: Vec<u8>,
    truncated_head: bool,
}

impl PhysicalLineFramer {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            if byte == b'\n' {
                self.append_encoded(&bytes[start..index]);
                self.finish_decoder();
                if let Some(line) = self.take_line() {
                    lines.push(line);
                }
                start = index + 1;
            }
        }
        self.append_encoded(&bytes[start..]);
        lines
    }

    fn finish(&mut self) -> Option<String> {
        self.finish_decoder();
        if self.pending_start == self.pending.len() && !self.truncated_head {
            return None;
        }
        self.take_line()
    }

    fn take_line(&mut self) -> Option<String> {
        let trimmed = self.pending[self.pending_start..].trim_matches(is_ecmascript_whitespace);
        let line = (!trimmed.is_empty())
            .then(|| truncate_utf16(trimmed, MAX_LINE_UTF16_UNITS, self.truncated_head));
        self.pending.clear();
        self.pending_start = 0;
        self.pending_utf16_units = 0;
        self.truncated_head = false;
        line
    }

    fn append_encoded(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut encoded = std::mem::take(&mut self.undecoded);
        encoded.extend_from_slice(bytes);
        let mut offset = 0;
        while offset < encoded.len() {
            match std::str::from_utf8(&encoded[offset..]) {
                Ok(text) => {
                    self.append_text(text);
                    offset = encoded.len();
                }
                Err(err) => {
                    let valid_end = offset + err.valid_up_to();
                    if valid_end > offset
                        && let Ok(valid) = std::str::from_utf8(&encoded[offset..valid_end])
                    {
                        self.append_text(valid);
                    }
                    match err.error_len() {
                        Some(error_len) => {
                            self.append_text("�");
                            offset = valid_end + error_len;
                        }
                        None => {
                            self.undecoded.extend_from_slice(&encoded[valid_end..]);
                            offset = encoded.len();
                        }
                    }
                }
            }
        }
    }

    fn finish_decoder(&mut self) {
        if self.undecoded.is_empty() {
            return;
        }
        let undecoded = std::mem::take(&mut self.undecoded);
        self.append_text(&String::from_utf8_lossy(&undecoded));
    }

    fn append_text(&mut self, text: &str) {
        self.pending.push_str(text);
        self.pending_utf16_units = self.pending_utf16_units.saturating_add(utf16_units(text));
        if self.pending_utf16_units <= MAX_PENDING_UTF16_UNITS {
            return;
        }

        let units_to_drop = self.pending_utf16_units - MAX_PENDING_UTF16_UNITS;
        let mut dropped_units = 0;
        let mut new_start = self.pending_start;
        for (index, ch) in self.pending[self.pending_start..].char_indices() {
            dropped_units += ch.len_utf16();
            new_start = self.pending_start + index + ch.len_utf8();
            if dropped_units >= units_to_drop {
                break;
            }
        }
        self.pending_start = new_start;
        self.pending_utf16_units -= dropped_units;
        self.truncated_head = true;
        if self.pending_start >= 64 * 1024 && self.pending_start * 2 >= self.pending.len() {
            self.pending.drain(..self.pending_start);
            self.pending_start = 0;
        }
    }
}

fn is_ecmascript_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

#[derive(Default)]
struct MonitorBatch {
    text: String,
    utf16_units: usize,
    truncated: bool,
    started_at: Option<Instant>,
}

impl MonitorBatch {
    fn push_line(&mut self, line: &str, now: Instant) {
        self.started_at.get_or_insert(now);
        if self.truncated {
            return;
        }

        let separator_units = usize::from(!self.text.is_empty());
        let line_units = utf16_units(line);
        if self.utf16_units + separator_units + line_units <= MAX_BATCH_UTF16_UNITS {
            if separator_units == 1 {
                self.text.push('\n');
            }
            self.text.push_str(line);
            self.utf16_units += separator_units + line_units;
            return;
        }

        let mut remaining = MAX_BATCH_UTF16_UNITS.saturating_sub(self.utf16_units);
        if separator_units == 1 && remaining > 0 {
            self.text.push('\n');
            self.utf16_units += 1;
            remaining -= 1;
        }
        let prefix = utf16_prefix(line, remaining);
        self.text.push_str(prefix);
        self.utf16_units += utf16_units(prefix);
        self.truncated = true;
    }

    fn deadline(&self) -> Option<Instant> {
        self.started_at.map(|started| started + BATCH_WINDOW)
    }

    fn request_flush(&mut self, now: Instant) {
        if self.started_at.is_some() {
            self.started_at = Some(now.checked_sub(BATCH_WINDOW).unwrap_or(now));
        }
    }

    fn take(&mut self) -> Option<String> {
        self.started_at?;
        let mut text = std::mem::take(&mut self.text);
        if self.truncated {
            text.push('\n');
            text.push_str(TRUNCATION_SUFFIX);
        }
        *self = Self::default();
        Some(text)
    }
}

struct MonitorRateLimiter {
    tokens: u32,
    last_refill: Instant,
    suppressed_since_notice: u64,
    overload_started: Option<Instant>,
    last_event_at: Option<Instant>,
}

struct RateDispatch {
    deliveries: Vec<String>,
    stop_message: Option<String>,
}

impl MonitorRateLimiter {
    fn new(now: Instant) -> Self {
        Self {
            tokens: RATE_LIMIT_CAPACITY,
            last_refill: now,
            suppressed_since_notice: 0,
            overload_started: None,
            last_event_at: None,
        }
    }

    fn submit(&mut self, batch: String, now: Instant, stop_on_overload: bool) -> RateDispatch {
        self.refill(now);
        if self
            .last_event_at
            .is_some_and(|last| now.saturating_duration_since(last) > OVERLOAD_QUIET_RESET_AFTER)
        {
            self.reset_overload_tracking();
        }
        self.last_event_at = Some(now);
        let mut deliveries = Vec::new();
        if self.suppressed_since_notice > 0 && self.take_token() {
            deliveries.push(suppression_message(self.suppressed_since_notice));
            self.suppressed_since_notice = 0;
        }
        if self.take_token() {
            deliveries.push(batch);
            return RateDispatch {
                deliveries,
                stop_message: None,
            };
        }

        self.suppressed_since_notice = self.suppressed_since_notice.saturating_add(1);
        self.overload_started.get_or_insert(now);
        let stop_message = self.overload_started.and_then(|started| {
            let duration = now.saturating_duration_since(started);
            (stop_on_overload && duration > OVERLOAD_STOP_AFTER)
                .then(|| overload_stop_message(self.suppressed_since_notice, duration))
        });
        if stop_message.is_some() {
            deliveries.clear();
            self.suppressed_since_notice = 0;
        }
        RateDispatch {
            deliveries,
            stop_message,
        }
    }

    fn take_suppression_notice(&mut self) -> Option<String> {
        if self.suppressed_since_notice == 0 {
            return None;
        }
        let message = suppression_message(self.suppressed_since_notice);
        self.suppressed_since_notice = 0;
        self.reset_overload_tracking();
        Some(message)
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        let intervals = elapsed.as_nanos() / RATE_LIMIT_REFILL_INTERVAL.as_nanos();
        if intervals == 0 {
            return;
        }
        let intervals = u32::try_from(intervals).unwrap_or(u32::MAX);
        self.tokens = self
            .tokens
            .saturating_add(intervals)
            .min(RATE_LIMIT_CAPACITY);
        self.last_refill += RATE_LIMIT_REFILL_INTERVAL * intervals;
    }

    fn take_token(&mut self) -> bool {
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }

    fn reset_overload_tracking(&mut self) {
        self.suppressed_since_notice = 0;
        self.overload_started = None;
    }
}

fn utf16_units(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_prefix(text: &str, max_units: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let units = ch.len_utf16();
        if used + units > max_units {
            break;
        }
        used += units;
        end = index + ch.len_utf8();
    }
    &text[..end]
}

fn truncate_utf16(text: &str, max_units: usize, force_truncated: bool) -> String {
    if !force_truncated && utf16_units(text) <= max_units {
        return text.to_string();
    }
    let mut truncated = utf16_prefix(text, max_units).to_string();
    truncated.push_str(TRUNCATION_SUFFIX);
    truncated
}

fn suppression_message(count: u64) -> String {
    format!(
        "[{count} events suppressed — output rate too high. Consider using TaskStop to restart this monitor with a more selective filter.]"
    )
}

fn overload_stop_message(count: u64, duration: Duration) -> String {
    format!(
        "[Monitor stopped — too much output ({count} events suppressed over {}s). Restart with a more selective source.]",
        duration.as_secs().max(OVERLOAD_STOP_AFTER.as_secs())
    )
}

async fn deliver_monitor_timeout(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
) {
    deliver_monitor_event(
        session,
        monitor,
        tool_use_id,
        event_turn_id,
        "[Monitor timed out — re-arm if needed.]",
    )
    .await;
}

async fn deliver_monitor_event(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
    event: &str,
) {
    let Some(active_session) = session.upgrade() else {
        return;
    };
    let Some(delta) =
        admit_monitor_output_delta(active_session.as_ref(), event_turn_id, tool_use_id, event)
    else {
        return;
    };
    // Admission is linearized under the runtime gate, but persistence and
    // delivery remain async and never hold the gate's synchronous mutex.
    active_session.send_event_raw(delta).await;
    if active_session.is_session_runtime_closing() {
        return;
    }

    let mut text = String::from("<task-notification>\n<task-id>");
    push_xml_escaped_text(&mut text, &monitor.task_id);
    text.push_str("</task-id>\n<summary>Monitor event: \"");
    push_xml_escaped_text(&mut text, &monitor.description);
    text.push_str("\"</summary>\n<event>");
    push_xml_escaped_text(&mut text, event);
    text.push_str("</event>\n</task-notification>");
    deliver_monitor_item(session, text_response_item(text)).await;
}

fn admit_monitor_output_delta(
    session: &Session,
    event_turn_id: &str,
    tool_use_id: &str,
    event: &str,
) -> Option<Event> {
    session.try_run_while_session_runtime_open(|| {
        monitor_output_delta_event(event_turn_id, tool_use_id, event)
    })
}

fn monitor_output_delta_event(event_turn_id: &str, tool_use_id: &str, event: &str) -> Event {
    Event {
        id: event_turn_id.to_string(),
        msg: EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
            call_id: tool_use_id.to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: event.as_bytes().to_vec(),
        }),
    }
}

async fn deliver_monitor_failure(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    message: &str,
) {
    let mut text = String::from("<task-notification>\n<task-id>");
    push_xml_escaped_text(&mut text, &monitor.task_id);
    text.push_str("</task-id>\n<tool-use-id>");
    push_xml_escaped_text(&mut text, tool_use_id);
    text.push_str("</tool-use-id>\n<status>failed</status>\n<summary>Monitor \"");
    push_xml_escaped_text(&mut text, &monitor.description);
    text.push_str("\" failed: ");
    push_xml_escaped_text(&mut text, message);
    text.push_str("</summary>\n</task-notification>");
    deliver_monitor_item(session, text_response_item(text)).await;
}

#[allow(clippy::too_many_arguments)]
async fn deliver_monitor_completion(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    output_file: &Path,
    exit_code: Option<i32>,
    failure_message: Option<String>,
    produced_output: bool,
) {
    let failed = failure_message.is_some() || exit_code.is_some_and(|code| code != 0);
    let status = if failed { "failed" } else { "completed" };
    let mut text = String::from("<task-notification>\n<task-id>");
    push_xml_escaped_text(&mut text, &monitor.task_id);
    text.push_str("</task-id>\n<tool-use-id>");
    push_xml_escaped_text(&mut text, tool_use_id);
    text.push_str("</tool-use-id>\n<output-file>");
    push_xml_escaped_text(&mut text, &output_file.display().to_string());
    text.push_str("</output-file>\n<status>");
    text.push_str(status);
    text.push_str("</status>\n<summary>Monitor \"");
    push_xml_escaped_text(&mut text, &monitor.description);
    text.push('"');
    let summary = monitor_completion_summary(
        failed,
        produced_output,
        exit_code,
        failure_message.as_deref(),
    );
    push_xml_escaped_text(&mut text, &summary);
    text.push_str("</summary>\n</task-notification>");
    deliver_monitor_item(session, text_response_item(text)).await;
}

fn monitor_completion_summary(
    failed: bool,
    produced_output: bool,
    exit_code: Option<i32>,
    failure_message: Option<&str>,
) -> String {
    let mut summary = if failed {
        " script failed".to_string()
    } else if produced_output {
        " stream ended".to_string()
    } else {
        " ended without producing output".to_string()
    };
    if let Some(exit_code) = exit_code.filter(|_| failed || !produced_output) {
        summary.push_str(&format!(" (exit {exit_code})"));
    }
    if let Some(message) = failure_message {
        summary.push_str(": ");
        summary.push_str(message);
    }
    summary
}

async fn deliver_monitor_item(session: &Weak<Session>, item: ResponseItem) {
    let Some(session) = session.upgrade() else {
        return;
    };
    if session.is_session_runtime_closing() {
        return;
    }
    let Err(items) = session.inject_if_running(vec![item]).await else {
        return;
    };
    if session.is_session_runtime_closing() {
        return;
    }
    let turn_input = items.into_iter().map(TurnInput::ResponseItem).collect();
    match session.try_start_monitor_turn_if_idle(turn_input).await {
        Ok(()) => {}
        Err(err)
            if matches!(
                err.reason(),
                TryStartTurnIfIdleRejectionReason::Busy
                    | TryStartTurnIfIdleRejectionReason::PendingTriggerTurn
            ) =>
        {
            enqueue_monitor_items(&session, err.into_input()).await;
        }
        Err(err) => enqueue_monitor_items(&session, err.into_input()).await,
    }
}

async fn enqueue_monitor_items(session: &Arc<Session>, input: Vec<TurnInput>) {
    if session.is_session_runtime_closing() {
        return;
    }
    let items = input
        .into_iter()
        .filter_map(|input| match input {
            TurnInput::ResponseItem(item) => Some(item),
            TurnInput::UserInput { .. } | TurnInput::InterAgentCommunication(_) => None,
        })
        .collect::<Vec<_>>();
    session
        .inject_no_new_turn(items, /*current_turn_context*/ None)
        .await;
}

fn text_response_item(text: String) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[cfg(test)]
#[path = "command_monitor_tests.rs"]
mod tests;
