//! Confirmed termination and notification arbitration after monitor failures.

use super::MonitorWorkerStop;
use super::archive::MonitorArchiveWriter;
use super::archive::archive_capture_chunk;
use super::delivery::deliver_monitor_event;
use super::delivery::deliver_monitor_failure;
use super::delivery::deliver_monitor_timeout;
use crate::session::session::Session;
use crate::unified_exec::MonitorCaptureReceiver;
use crate::unified_exec::MonitorOutputChunk;
use crate::unified_exec::MonitorStopReason;
use crate::unified_exec::MonitorTaskStatus;
use crate::unified_exec::UnifiedExecProcess;
use codex_protocol::protocol::CommandMonitorInfo;
use std::future::Future;
use std::future::pending;
use std::io;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::watch;

const ARCHIVE_DRAIN_CHUNKS_PER_PASS: usize = 64;

#[allow(clippy::too_many_arguments)]
pub(super) async fn stop_process_for_flood(
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
pub(super) enum FloodStopError {
    Termination(String),
    Archive(io::Error),
}

pub(super) enum DegradedMonitor {
    Fatal { message: String },
    Flood { stop_message: String },
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_fatal_monitor_recovery(
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

pub(super) async fn confirm_fatal_monitor_stop(
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

pub(super) async fn claim_and_deliver_fatal_monitor(
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
pub(super) async fn run_degraded_monitor(
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

        if capture_finished
            && (flood_stop_confirmed
                || (process.has_exited() && process.output_completed_normally()))
        {
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
                DegradedMonitor::Fatal { message } if !flood_stop_confirmed => {
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
                _ => {
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

pub(super) async fn await_while_draining_capture<F>(
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
