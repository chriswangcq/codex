use super::*;
use crate::state::TaskKind;
use crate::tasks::MailboxParentProvenance;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use pretty_assertions::assert_eq;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::task::Context;
use std::task::Poll;
use tokio::io::AsyncWrite;

struct RunCountingTask {
    runs: Arc<AtomicUsize>,
}

#[test]
fn monitor_termination_reason_preserves_controlled_stop_semantics() {
    assert_eq!(
        command_monitor_termination_reason(MonitorTaskStatus::Killed, true, None),
        Some(CommandMonitorTerminationReason::TimedOut)
    );
    assert_eq!(
        command_monitor_termination_reason(
            MonitorTaskStatus::Killed,
            false,
            Some(MonitorStopReason::User),
        ),
        Some(CommandMonitorTerminationReason::UserStopped)
    );
    assert_eq!(
        command_monitor_termination_reason(
            MonitorTaskStatus::Killed,
            false,
            Some(MonitorStopReason::SessionShutdown),
        ),
        Some(CommandMonitorTerminationReason::SessionShutdown)
    );
    assert_eq!(
        command_monitor_termination_reason(
            MonitorTaskStatus::Killed,
            false,
            Some(MonitorStopReason::Capacity),
        ),
        Some(CommandMonitorTerminationReason::Capacity)
    );
    assert_eq!(
        command_monitor_termination_reason(MonitorTaskStatus::Killed, false, None),
        Some(CommandMonitorTerminationReason::Stopped)
    );
    assert_eq!(
        command_monitor_termination_reason(MonitorTaskStatus::Completed, false, None),
        None
    );
    assert_eq!(
        command_monitor_termination_reason(MonitorTaskStatus::Failed, false, None),
        None
    );
}

#[tokio::test]
async fn monitor_completion_guard_preserves_shutdown_reason_when_dropped() {
    let (stop_tx, stop_rx) = watch::channel(None);
    stop_tx
        .send(Some(MonitorStopReason::SessionShutdown))
        .expect("completion guard should still hold the stop receiver");
    let (done_tx, done_rx) = oneshot::channel();
    let worker_done = CancellationToken::new();

    let guard = MonitorWorkerCompletionGuard {
        done_tx: Some(done_tx),
        worker_done: worker_done.clone(),
        deadline_fired: Arc::new(AtomicBool::new(false)),
        stop_rx,
    };
    drop(guard);

    assert_eq!(
        done_rx.await.expect("completion reason should be sent"),
        Some(CommandMonitorTerminationReason::SessionShutdown)
    );
    assert!(worker_done.is_cancelled());
}

impl SessionTask for RunCountingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.monitor_shutdown_race"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<super::super::turn_context::TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

#[tokio::test]
async fn closing_session_drops_monitor_delivery_without_starting_a_turn() {
    let (session, _turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let history_len = session.clone_history().await.raw_items().len();
    session.begin_session_runtime_shutdown();

    deliver_monitor_item(
        &Arc::downgrade(&session),
        text_response_item("late monitor event".to_string()),
    )
    .await;

    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        history_len,
        session.clone_history().await.raw_items().len(),
        "late monitor delivery must not be persisted after shutdown starts"
    );
}

#[tokio::test]
async fn closing_session_drops_raw_monitor_output_delta() {
    let (session, _turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    session.begin_session_runtime_shutdown();
    let monitor = CommandMonitorInfo {
        task_id: "b1234abcd".to_string(),
        description: "late event".to_string(),
        timeout_ms: 1_000,
        persistent: false,
    };

    deliver_monitor_event(
        &Arc::downgrade(&session),
        &monitor,
        "call-late-monitor",
        "turn-late-monitor",
        "should not escape shutdown",
    )
    .await;

    assert!(
        events.try_recv().is_err(),
        "late monitor output delta must not be published after shutdown starts"
    );
}

#[tokio::test]
async fn raw_monitor_delta_admitted_before_shutdown_may_finish_after_boundary() {
    let (session, _turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let admitted = admit_monitor_output_delta(
        session.as_ref(),
        "turn-admitted-monitor",
        "call-admitted-monitor",
        "admitted before shutdown",
    )
    .expect("open runtime should admit the monitor delta");

    session.begin_session_runtime_shutdown();
    session.send_event_raw(admitted).await;

    let delivered = events
        .recv()
        .await
        .expect("an event admitted before shutdown should finish delivery");
    assert_eq!(delivered.id, "turn-admitted-monitor");
    let EventMsg::ExecCommandOutputDelta(delta) = delivered.msg else {
        panic!("expected monitor output delta");
    };
    assert_eq!(delta.call_id, "call-admitted-monitor");
    assert_eq!(delta.chunk, b"admitted before shutdown");
}

// The held lock is deliberate: it keeps the start future pending so the test
// can place shutdown exactly between admission and installation.
#[allow(clippy::await_holding_invalid_type)]
#[tokio::test]
async fn in_flight_monitor_idle_reservation_cannot_cross_shutdown_boundary() {
    let (session, _turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let start = {
        let active_turn_guard = session.active_turn.lock().await;
        let input = TurnInput::ResponseItem(text_response_item("monitor race".to_string()));
        let mut start = Box::pin(session.try_start_monitor_turn_if_idle(vec![input]));

        assert!(matches!(futures::poll!(start.as_mut()), Poll::Pending));
        session.begin_session_runtime_shutdown();
        drop(active_turn_guard);
        start
    };

    let error = start
        .await
        .expect_err("closing session must reject an in-flight idle reservation");
    assert_eq!(TryStartTurnIfIdleRejectionReason::Busy, error.reason());
    assert!(session.active_turn.lock().await.is_none());
}

// As above, the lock is the synchronization barrier under test rather than a
// production lock held across an asynchronous operation.
#[allow(clippy::await_holding_invalid_type)]
#[tokio::test]
async fn in_flight_task_start_cannot_install_or_run_after_shutdown_boundary() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let start = {
        let active_turn_guard = session.active_turn.lock().await;
        let mut start = Box::pin(session.start_task(
            turn_context,
            Vec::new(),
            RunCountingTask {
                runs: Arc::clone(&runs),
            },
            /*input_persisted*/ None,
            MailboxParentProvenance::Ignore,
        ));

        assert!(matches!(futures::poll!(start.as_mut()), Poll::Pending));
        session.begin_session_runtime_shutdown();
        drop(active_turn_guard);
        start
    };

    assert!(!start.await, "task installation must lose to shutdown");
    tokio::task::yield_now().await;
    assert_eq!(0, runs.load(Ordering::SeqCst));
    assert!(session.active_turn.lock().await.is_none());
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn deadline_reports_termination_failure_without_marking_monitor_killed() {
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ true,
        )
        .await,
    );
    let deadline_fired = Arc::new(AtomicBool::new(false));
    let deadline_stop_error = Arc::new(StdMutex::new(None));
    let deadline_notify = Arc::new(Notify::new());
    spawn_deadline_supervisor(
        1_000,
        Arc::clone(&process),
        CancellationToken::new(),
        Arc::clone(&deadline_fired),
        Arc::clone(&deadline_stop_error),
        Arc::clone(&deadline_notify),
        Arc::new(AtomicBool::new(false)),
    );

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(crate::unified_exec::TERMINATE_CONFIRMATION_TIMEOUT).await;
    tokio::task::yield_now().await;

    assert!(!deadline_fired.load(Ordering::Acquire));
    let error = deadline_stop_error
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("termination failure should be published");
    assert!(error.contains("timed out waiting for confirmed process termination"));
    assert!(!process.has_exited());
}

#[test]
fn natural_completion_requires_normal_output_completion() {
    assert!(monitor_worker_ready_to_finalize(
        true, true, true, true, false
    ));
    assert!(!monitor_worker_ready_to_finalize(
        true, true, true, false, false
    ));
    assert!(!monitor_worker_ready_to_finalize(
        true, true, false, false, false
    ));
    assert!(monitor_worker_ready_to_finalize(
        true, true, false, false, true
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn fatal_terminal_requires_an_atomic_manager_claim() {
    let (session, _turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let history_len = session.clone_history().await.raw_items().len();
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ false,
        )
        .await,
    );
    let monitor = CommandMonitorInfo {
        task_id: "bnotclaimed".to_string(),
        description: "claim arbitration".to_string(),
        timeout_ms: 1_000,
        persistent: false,
    };

    let status = claim_and_deliver_fatal_monitor(
        &Arc::downgrade(&session),
        &monitor,
        "call-not-claimed",
        &process,
        "fatal setup failure",
        &Arc::new(AtomicBool::new(false)),
    )
    .await;

    assert_eq!(status, MonitorTaskStatus::Killed);
    assert_eq!(history_len, session.clone_history().await.raw_items().len());

    let deadline_won = Arc::new(AtomicBool::new(true));
    let status = claim_and_deliver_fatal_monitor(
        &Arc::downgrade(&session),
        &monitor,
        "call-deadline-won",
        &process,
        "fatal setup failure",
        &deadline_won,
    )
    .await;
    assert_eq!(status, MonitorTaskStatus::Killed);
    assert_eq!(history_len, session.clone_history().await.raw_items().len());
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn failed_fatal_kill_keeps_worker_owner_until_an_external_stop() {
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ false,
        )
        .await,
    );
    let output_task_notify = Arc::new(Notify::new());
    let (stop_tx, stop_rx) = watch::channel(None);
    let deadline_fired = Arc::new(AtomicBool::new(false));
    let deadline_stop_error = Arc::new(StdMutex::new(None));
    let deadline_notify = Arc::new(Notify::new());
    let worker = tokio::spawn({
        let process = Arc::clone(&process);
        let deadline_fired = Arc::clone(&deadline_fired);
        let deadline_stop_error = Arc::clone(&deadline_stop_error);
        let deadline_notify = Arc::clone(&deadline_notify);
        async move {
            let monitor = CommandMonitorInfo {
                task_id: "bdegraded".to_string(),
                description: "degraded owner".to_string(),
                timeout_ms: 1_000,
                persistent: true,
            };
            run_fatal_monitor_recovery(
                &Weak::new(),
                &monitor,
                "call-degraded",
                "turn-degraded",
                &process,
                &output_task_notify,
                /*archive_rx*/ None,
                stop_rx,
                &deadline_fired,
                &deadline_stop_error,
                &deadline_notify,
                "fatal setup failure".to_string(),
            )
            .await
        }
    });

    tokio::task::yield_now().await;
    tokio::time::advance(crate::unified_exec::TERMINATE_CONFIRMATION_TIMEOUT).await;
    tokio::task::yield_now().await;
    assert!(
        !worker.is_finished(),
        "a failed kill must retain the worker that owns monitor lifecycle state"
    );

    stop_tx
        .send(Some(MonitorStopReason::User))
        .expect("degraded worker should retain its stop receiver");
    tokio::task::yield_now().await;
    assert_eq!(
        worker.await.expect("degraded worker should exit cleanly"),
        MonitorTaskStatus::Killed
    );
}

#[test]
fn physical_lines_are_trimmed_and_blank_lines_are_skipped() {
    let mut framer = PhysicalLineFramer::default();

    assert_eq!(framer.push(b"  first  \n\r\n sec"), vec!["first"]);
    assert_eq!(framer.push(b"ond \r\n"), vec!["second"]);
    assert_eq!(framer.finish(), None);
}

#[test]
fn whitespace_only_stdout_counts_as_produced_output_without_emitting_an_event() {
    let mut lines = PhysicalLineFramer::default();
    let mut batch = MonitorBatch::default();
    let mut produced_output = false;

    ingest_stdout(
        b"  \t\n",
        &mut lines,
        &mut batch,
        &mut produced_output,
        Instant::now(),
    );

    assert!(produced_output);
    assert_eq!(batch.take(), None);
    assert_eq!(
        monitor_completion_summary(false, produced_output, Some(0), None),
        " stream ended"
    );
}

#[test]
fn physical_line_trims_ecmascript_bom_whitespace() {
    let mut framer = PhysicalLineFramer::default();

    assert_eq!(
        framer.push("\u{FEFF}event\u{FEFF}\n".as_bytes()),
        vec!["event".to_string()]
    );
}

#[test]
fn physical_line_preserves_non_ecmascript_next_line_character() {
    let mut framer = PhysicalLineFramer::default();

    assert_eq!(
        framer.push("\u{0085}event\u{0085}\n".as_bytes()),
        vec!["\u{0085}event\u{0085}".to_string()]
    );
}

#[test]
fn physical_line_limit_counts_javascript_utf16_units() {
    let exactly_500_units = "😀".repeat(250);
    let over_500_units = "😀".repeat(251);

    assert_eq!(
        truncate_utf16(&exactly_500_units, MAX_LINE_UTF16_UNITS, false),
        exactly_500_units
    );
    assert_eq!(
        truncate_utf16(&over_500_units, MAX_LINE_UTF16_UNITS, false),
        format!("{}{}", "😀".repeat(250), TRUNCATION_SUFFIX)
    );
}

#[test]
fn pending_physical_line_is_bounded_and_marked_truncated() {
    let mut framer = PhysicalLineFramer::default();
    let mut bytes = vec![b'a'; MAX_PENDING_UTF16_UNITS + 100];
    bytes.push(b'\n');

    let lines = framer.push(&bytes);

    assert_eq!(framer.pending_utf16_units, 0);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        format!("{}{}", "a".repeat(MAX_LINE_UTF16_UNITS), TRUNCATION_SUFFIX)
    );
}

#[test]
fn pending_line_keeps_the_last_utf16_units_across_split_code_points() {
    let mut framer = PhysicalLineFramer::default();
    let mut bytes = format!("HEAD{}TAIL", "😀".repeat(MAX_PENDING_UTF16_UNITS / 2)).into_bytes();
    let split = bytes.len() - "TAIL".len() - 2;
    let tail = bytes.split_off(split);

    assert!(framer.push(&bytes).is_empty());
    let mut tail_with_newline = tail;
    tail_with_newline.push(b'\n');
    let lines = framer.push(&tail_with_newline);

    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with('😀'));
    assert!(lines[0].ends_with(TRUNCATION_SUFFIX));
}

#[test]
fn batch_limit_counts_utf16_units_and_uses_batch_suffix() {
    let now = Instant::now();
    let mut batch = MonitorBatch::default();
    for _ in 0..7 {
        batch.push_line(&"x".repeat(MAX_LINE_UTF16_UNITS), now);
    }

    let framed = batch.take().expect("batch should contain output");
    let prefix = framed
        .strip_suffix(&format!("\n{TRUNCATION_SUFFIX}"))
        .expect("batch should carry its truncation suffix");
    assert_eq!(utf16_units(prefix), MAX_BATCH_UTF16_UNITS);
}

#[test]
fn batch_window_is_two_hundred_milliseconds() {
    let now = Instant::now();
    let mut batch = MonitorBatch::default();
    batch.push_line("event", now);

    assert_eq!(batch.deadline(), Some(now + Duration::from_millis(200)));
}

#[test]
fn token_bucket_starts_with_ten_tokens_and_refills_one_every_two_seconds() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let dispatch = limiter.submit(format!("event-{index}"), start, true);
        assert_eq!(dispatch.deliveries, vec![format!("event-{index}")]);
        assert_eq!(dispatch.stop_message, None);
    }

    let suppressed = limiter.submit("event-10".to_string(), start, true);
    assert_eq!(suppressed.deliveries, Vec::<String>::new());
    assert_eq!(suppressed.stop_message, None);

    let after_refill = limiter.submit(
        "event-11".to_string(),
        start + RATE_LIMIT_REFILL_INTERVAL,
        true,
    );
    assert_eq!(after_refill.deliveries, vec![suppression_message(1)]);
    assert_eq!(after_refill.stop_message, None);
}

#[test]
fn sustained_overload_stops_after_thirty_seconds() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let _ = limiter.submit(format!("initial-{index}"), start, true);
    }

    let mut stopped = None;
    for tick in 0..=151 {
        let now = start + Duration::from_millis(tick * 200);
        let dispatch = limiter.submit(format!("overload-{tick}"), now, true);
        if dispatch.stop_message.is_some() {
            stopped = dispatch.stop_message;
            break;
        }
    }

    let stopped = stopped.expect("continuous overload should stop the monitor");
    assert!(stopped.starts_with("[Monitor stopped — too much output ("));
    assert!(stopped.contains(" events suppressed over 30s)"));
    assert!(stopped.ends_with("Restart with a more selective source.]"));
}

#[test]
fn sustained_overload_does_not_stop_at_exactly_thirty_seconds() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let _ = limiter.submit(format!("initial-{index}"), start, true);
    }
    let _ = limiter.submit("suppressed".to_string(), start, true);

    let dispatch = limiter.submit(
        "exact-boundary".to_string(),
        start + OVERLOAD_STOP_AFTER,
        true,
    );

    assert_eq!(dispatch.stop_message, None);
}

#[test]
fn successful_refill_delivery_does_not_reset_continuous_overload() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let _ = limiter.submit(format!("initial-{index}"), start, true);
    }
    let _ = limiter.submit("suppressed".to_string(), start, true);

    let dispatch = limiter.submit(
        "delivered-after-refill".to_string(),
        start + Duration::from_secs(4),
        true,
    );

    assert_eq!(dispatch.deliveries.len(), 2);
    assert_eq!(limiter.overload_started, Some(start));
}

#[test]
fn quiet_gap_resets_overload_tracking_without_emitting_a_notice() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let _ = limiter.submit(format!("initial-{index}"), start, true);
    }
    let _ = limiter.submit("suppressed".to_string(), start, true);

    let dispatch = limiter.submit(
        "after-quiet".to_string(),
        start + OVERLOAD_QUIET_RESET_AFTER + Duration::from_nanos(1),
        true,
    );

    assert_eq!(dispatch.deliveries, vec!["after-quiet"]);
    assert_eq!(dispatch.stop_message, None);
    assert_eq!(limiter.overload_started, None);
    assert_eq!(limiter.suppressed_since_notice, 0);
}

#[test]
fn suppression_and_overload_messages_match_the_monitor_contract() {
    assert_eq!(
        suppression_message(7),
        "[7 events suppressed — output rate too high. Consider using TaskStop to restart this monitor with a more selective filter.]"
    );
    assert_eq!(
        overload_stop_message(42, Duration::from_secs(31)),
        "[Monitor stopped — too much output (42 events suppressed over 31s). Restart with a more selective source.]"
    );
}

#[test]
fn natural_no_output_summary_includes_zero_exit_code() {
    assert_eq!(
        monitor_completion_summary(false, false, Some(0), None),
        " ended without producing output (exit 0)"
    );
}

#[test]
fn natural_output_summary_omits_zero_exit_code() {
    assert_eq!(
        monitor_completion_summary(false, true, Some(0), None),
        " stream ended"
    );
    assert_eq!(
        monitor_completion_summary(true, true, Some(7), None),
        " script failed (exit 7)"
    );
}

#[test]
fn output_delta_uses_turn_id_and_keeps_tool_use_id_in_payload() {
    let event = monitor_output_delta_event("turn-123", "call-456", "framed");

    assert_eq!(event.id, "turn-123");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected output delta");
    };
    assert_eq!(delta.call_id, "call-456");
    assert_eq!(delta.stream, ExecOutputStream::Stdout);
    assert_eq!(delta.chunk, b"framed");
}

#[tokio::test]
async fn output_file_creation_never_truncates_an_existing_task_file() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000000.output");
    let mut file = create_output_file(&path)
        .await
        .expect("first creation should succeed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(temp_dir.path())
                .await
                .expect("archive directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            file.metadata()
                .await
                .expect("archive metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    file.write_all(b"preserve")
        .await
        .expect("fixture should be written");
    file.flush().await.expect("fixture should be flushed");

    let err = create_output_file(&path)
        .await
        .expect_err("second creation should fail");

    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        tokio::fs::read(&path)
            .await
            .expect("existing file should remain readable"),
        b"preserve"
    );
}

#[tokio::test]
async fn archive_keeps_both_streams_but_framer_consumes_only_stdout() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000001.output");
    let file = create_output_file(&path)
        .await
        .expect("output file should be created");
    let mut file = MonitorArchiveWriter::new(file, MonitorArchiveBudget::with_cap(u64::MAX));
    let mut lines = PhysicalLineFramer::default();
    let mut batch = MonitorBatch::default();
    let mut produced_output = false;

    handle_capture_chunk(
        MonitorOutputChunk::Output {
            bytes: b"stderr-only\n".to_vec(),
            is_stdout: false,
        },
        &mut file,
        &mut lines,
        &mut batch,
        &mut produced_output,
        true,
    )
    .await
    .expect("stderr should be archived");
    handle_capture_chunk(
        MonitorOutputChunk::Output {
            bytes: b"stdout-only\n".to_vec(),
            is_stdout: true,
        },
        &mut file,
        &mut lines,
        &mut batch,
        &mut produced_output,
        true,
    )
    .await
    .expect("stdout should be archived and framed");
    file.flush().await.expect("archive should flush");

    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("archive should be readable"),
        b"stderr-only\nstdout-only\n"
    );
    assert_eq!(batch.take(), Some("stdout-only".to_string()));
    assert!(produced_output);
}

#[tokio::test]
async fn stdout_close_flushes_a_partial_line_while_stderr_stays_open() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000002.output");
    let file = create_output_file(&path)
        .await
        .expect("output file should be created");
    let mut file = MonitorArchiveWriter::new(file, MonitorArchiveBudget::with_cap(u64::MAX));
    let mut lines = PhysicalLineFramer::default();
    let mut batch = MonitorBatch::default();
    let mut produced_output = false;

    handle_capture_chunk(
        MonitorOutputChunk::Output {
            bytes: b"partial".to_vec(),
            is_stdout: true,
        },
        &mut file,
        &mut lines,
        &mut batch,
        &mut produced_output,
        true,
    )
    .await
    .expect("stdout should be archived");
    handle_capture_chunk(
        MonitorOutputChunk::StdoutClosed,
        &mut file,
        &mut lines,
        &mut batch,
        &mut produced_output,
        true,
    )
    .await
    .expect("stdout close should flush the framer");

    assert_eq!(batch.take().as_deref(), Some("partial"));
    assert!(produced_output);
    file.flush().await.expect("archive should flush");
    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("archive should be readable"),
        b"partial"
    );
}

#[tokio::test]
async fn archive_gap_event_writes_one_bounded_marker() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000003.output");
    let file = create_output_file(&path)
        .await
        .expect("output file should be created");
    let mut writer = MonitorArchiveWriter::new(file, MonitorArchiveBudget::with_cap(u64::MAX));

    archive_capture_chunk(MonitorOutputChunk::ArchiveGap, &mut writer)
        .await
        .expect("the archive gap marker should be writable");
    archive_capture_chunk(MonitorOutputChunk::ArchiveGap, &mut writer)
        .await
        .expect("a duplicate archive gap should be harmless");
    writer.flush().await.expect("archive should flush");

    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("archive should be readable"),
        ARCHIVE_CAPTURE_GAP_MARKER,
        "one monitor writes at most one fixed-size gap marker"
    );
}

#[tokio::test]
async fn archive_discards_the_crossing_chunk_and_writes_one_disk_cap_marker() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000002.output");
    let file = create_output_file(&path)
        .await
        .expect("output file should be created");
    let mut writer = MonitorArchiveWriter::with_cap_and_budget(
        file,
        5,
        MonitorArchiveBudget::with_cap(u64::MAX),
    );

    writer
        .write_chunk(b"1234")
        .await
        .expect("chunk within cap should be written");
    writer
        .write_chunk(b"56")
        .await
        .expect("crossing chunk should be replaced by marker");
    writer
        .write_chunk(b"ignored")
        .await
        .expect("later chunks should be discarded");
    writer.flush().await.expect("archive should flush");

    let mut expected = b"1234".to_vec();
    expected.extend_from_slice(ARCHIVE_TRUNCATION_MARKER);
    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("archive should be readable"),
        expected
    );
}

#[tokio::test]
async fn multiple_archive_writers_share_one_session_content_budget() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let first_path = temp_dir.path().join("b00000003.output");
    let second_path = temp_dir.path().join("b00000004.output");
    let first_file = create_output_file(&first_path)
        .await
        .expect("first output file should be created");
    let second_file = create_output_file(&second_path)
        .await
        .expect("second output file should be created");
    let budget = MonitorArchiveBudget::with_cap(5);
    let mut first = MonitorArchiveWriter::with_cap_and_budget(first_file, 5, Arc::clone(&budget));
    let mut second = MonitorArchiveWriter::with_cap_and_budget(second_file, 5, Arc::clone(&budget));

    let (first_result, second_result) =
        tokio::join!(first.write_chunk(b"1234"), second.write_chunk(b"5678"),);
    first_result.expect("one writer should reserve content bytes");
    second_result.expect("the other writer should write only its cap marker");
    first.flush().await.expect("first archive should flush");
    second.flush().await.expect("second archive should flush");

    assert_eq!(budget.content_bytes(), 4);
    let contents = [
        tokio::fs::read(first_path)
            .await
            .expect("first archive should be readable"),
        tokio::fs::read(second_path)
            .await
            .expect("second archive should be readable"),
    ];
    assert_eq!(
        contents
            .iter()
            .filter(|content| content.as_slice() == b"1234" || content.as_slice() == b"5678")
            .count(),
        1
    );
    assert_eq!(
        contents
            .iter()
            .filter(|content| content.as_slice() == ARCHIVE_TRUNCATION_MARKER)
            .count(),
        1
    );
}

struct FailAfterWriter {
    written: usize,
    fail_after: usize,
}

impl AsyncWrite for FailAfterWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.written >= self.fail_after {
            return Poll::Ready(Err(io::Error::other("injected archive write failure")));
        }
        let written = bytes.len().min(self.fail_after - self.written);
        self.written += written;
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn archive_write_failure_releases_only_unwritten_reservation() {
    let budget = MonitorArchiveBudget::with_cap(10);
    let mut failing = MonitorArchiveWriter::with_cap_and_budget(
        FailAfterWriter {
            written: 0,
            fail_after: 2,
        },
        10,
        Arc::clone(&budget),
    );

    failing
        .write_chunk(b"12345")
        .await
        .expect_err("fixture should fail after a partial write");

    assert_eq!(budget.content_bytes(), 2);
    let mut succeeding =
        MonitorArchiveWriter::with_cap_and_budget(tokio::io::sink(), 10, Arc::clone(&budget));
    succeeding
        .write_chunk(b"12345678")
        .await
        .expect("rolled-back bytes should remain available to another writer");
    assert_eq!(budget.content_bytes(), 10);
}

#[tokio::test]
async fn fatal_termination_drains_more_than_capture_channel_capacity() {
    const CAPACITY: usize = 128;
    let (capture_tx, mut capture_rx) = crate::unified_exec::monitor_capture_channel(CAPACITY);
    let producer = tokio::spawn(async move {
        for _ in 0..(CAPACITY * 2) {
            capture_tx
                .send(MonitorOutputChunk::Output {
                    bytes: b"flood".to_vec(),
                    is_stdout: true,
                })
                .await
                .expect("capture receiver should stay open while terminating");
        }
    });
    let termination = async move {
        producer
            .await
            .expect("output producer should unblock while capture is drained");
        Ok::<(), crate::unified_exec::UnifiedExecError>(())
    };

    await_while_draining_capture(termination, Some(&mut capture_rx))
        .await
        .expect("termination should not deadlock behind the bounded capture channel");
}
