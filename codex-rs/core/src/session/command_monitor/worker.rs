//! Output collection and the normal monitor lifecycle.

use super::MonitorWorkerStop;

use super::archive::MonitorArchiveWriter;
use super::archive::create_output_file;
use super::delivery::deliver_monitor_completion;
use super::delivery::deliver_monitor_event;
use super::delivery::deliver_monitor_timeout;
use super::output::MonitorBatch;
use super::output::MonitorRateLimiter;
use super::output::PhysicalLineFramer;
use super::output::RateDispatch;
use super::recovery::DegradedMonitor;
use super::recovery::FloodStopError;
use super::recovery::run_degraded_monitor;
use super::recovery::run_fatal_monitor_recovery;
use super::recovery::stop_process_for_flood;
use crate::session::session::Session;
use crate::unified_exec::MonitorArchiveBudget;
use crate::unified_exec::MonitorCaptureReceiver;
use crate::unified_exec::MonitorOutputChunk;
use crate::unified_exec::MonitorStopReason;
use crate::unified_exec::MonitorTaskStatus;
use crate::unified_exec::OutputHandles;
use crate::unified_exec::UnifiedExecProcess;
use codex_protocol::protocol::CommandMonitorInfo;
use std::future::pending;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const ARCHIVE_DRAIN_CHUNKS_PER_PASS: usize = 64;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_monitor_worker(
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
                        limiter.submit(text, Instant::now(), /*stop_on_overload*/ false),
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
                limiter.submit(text, now, /*stop_on_overload*/ true),
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
                if deadline_fired.load(Ordering::Acquire) {
                    stop_kind = Some(MonitorWorkerStop::Deadline);
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
                    limiter.submit(text, Instant::now(), /*stop_on_overload*/ true),
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

pub(super) fn monitor_worker_ready_to_finalize(
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

pub(super) fn ingest_stdout(
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

pub(super) async fn handle_capture_chunk(
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

async fn dispatch_batch(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
    dispatch: RateDispatch,
) -> Option<String> {
    for text in dispatch.deliveries {
        deliver_monitor_event(session, monitor, tool_use_id, event_turn_id, &text).await;
    }
    dispatch.stop_message
}
