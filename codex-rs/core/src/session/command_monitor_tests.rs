use super::delivery::admit_monitor_output_delta;
use super::delivery::deliver_monitor_event;
use super::delivery::deliver_monitor_item;
use super::delivery::text_response_item;
use super::recovery::await_while_draining_capture;
use super::recovery::claim_and_deliver_fatal_monitor;
use super::recovery::run_fatal_monitor_recovery;
use super::worker::monitor_worker_ready_to_finalize;
use super::worker::run_monitor_worker;
use super::*;
use crate::session::input_queue::TurnInput;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskResult;
use crate::unified_exec::MonitorOutputChunk;
use codex_protocol::protocol::EventMsg;
use codex_protocol::turn_input::TurnInputSubmission;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;
use std::task::Poll;

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
        let input = text_response_item("monitor race".to_string());
        let mut start = Box::pin(session.try_start_monitor_turn_if_idle(input));

        assert!(matches!(futures::poll!(start.as_mut()), Poll::Pending));
        session.begin_session_runtime_shutdown();
        drop(active_turn_guard);
        start
    };

    assert_eq!(
        start.await.expect("monitor admission result"),
        TurnInputSubmission::NotSubmitted {
            reason: codex_protocol::turn_input::NotSubmittedReason::NotIdle,
        },
    );
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

#[cfg(unix)]
#[tokio::test]
async fn deadline_published_during_finalization_delivers_once_and_preserves_external_stop() {
    for stop_reason in [
        None,
        Some(MonitorStopReason::User),
        Some(MonitorStopReason::SessionShutdown),
    ] {
        let (session, _turn_context, events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        // A reservation receives the notification without starting a model task.
        *session.active_turn.lock().await = Some(crate::state::ActiveTurn::default());
        let (process, output, termination_calls) =
            crate::unified_exec::completed_monitor_process_for_test().await;
        let stop_guard = process.begin_monitor_stop();
        let (stop_tx, stop_rx) = watch::channel(None);
        let deadline_fired = Arc::new(AtomicBool::new(false));
        let deadline_stop_error = Arc::new(StdMutex::new(None));
        let deadline_notify = Arc::new(Notify::new());
        let deadline_cancel = CancellationToken::new();
        let monitor = CommandMonitorInfo {
            task_id: "bdeadline-finalization".to_string(),
            description: "finalization ordering".to_string(),
            timeout_ms: 1_000,
            persistent: false,
        };
        let temp_dir = tempfile::tempdir().expect("temporary archive directory");
        let output_file = temp_dir.path().join("monitor.output");
        let session_ref = Arc::downgrade(&session);
        let worker = run_monitor_worker(
            &session_ref,
            "call-finalizing-monitor",
            "turn-finalizing-monitor",
            &monitor,
            &output,
            &output_file,
            stop_rx,
            &process,
            &deadline_fired,
            &deadline_stop_error,
            &deadline_notify,
            &deadline_cancel,
            MonitorArchiveBudget::with_cap(u64::MAX),
        );
        tokio::pin!(worker);
        tokio::select! {
            _ = deadline_cancel.cancelled() => {}
            status = &mut worker => panic!("worker crossed the held stop guard: {status:?}"),
        }
        assert!(!deadline_fired.load(Ordering::Acquire));
        // The supervisor publishes its result only after root/output closure
        // lets the worker enter finalization, then releases the stop guard.
        deadline_fired.store(true, Ordering::Release);
        if let Some(reason) = stop_reason {
            stop_tx
                .send(Some(reason))
                .expect("worker still owns stop receiver");
        }
        drop(stop_guard);
        assert_eq!(worker.await, MonitorTaskStatus::Killed);
        assert_eq!(termination_calls.load(Ordering::SeqCst), 0);

        let mut deltas = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let EventMsg::ExecCommandOutputDelta(delta) = event.msg {
                assert_eq!(delta.call_id, "call-finalizing-monitor");
                deltas.push(String::from_utf8(delta.chunk).expect("monitor UTF-8 event"));
            }
        }
        let expected = match stop_reason {
            None => vec!["[Monitor timed out — re-arm if needed.]"],
            Some(MonitorStopReason::User) => vec!["[Monitor stopped]"],
            Some(MonitorStopReason::SessionShutdown) => Vec::new(),
            Some(MonitorStopReason::Capacity) => unreachable!(),
        };
        assert_eq!(deltas, expected);
    }
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

#[tokio::test]
async fn monitor_idle_notification_wakes_plan_mode() {
    let (session, _turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut collaboration_mode = session.collaboration_mode().await;
    collaboration_mode.mode = codex_protocol::config_types::ModeKind::Plan;
    session
        .update_settings(super::super::session::SessionSettingsUpdate {
            step_settings: super::super::step_settings::StepSettingsUpdate {
                collaboration_mode: Some(collaboration_mode),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("apply Plan mode");
    assert!(matches!(
        session
            .try_start_monitor_turn_if_idle(text_response_item("monitor event".to_string()))
            .await,
        Ok(TurnInputSubmission::Started { .. }),
    ));
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
}

// Hold the active-turn lock to replace the reservation before installation resumes.
#[allow(clippy::await_holding_invalid_type)]
#[tokio::test]
async fn replaced_monitor_reservation_cannot_start_over_user_work() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let mut active = session.active_turn.lock().await;
    let reserved = crate::state::ActiveTurn::default();
    let reserved_state = Arc::clone(&reserved.turn_state);
    *active = Some(reserved);
    let mut start = Box::pin(session.start_task_with_reservation(
        turn_context,
        Vec::new(),
        RunCountingTask {
            runs: Arc::clone(&runs),
        },
        Some(&reserved_state),
    ));
    assert!(matches!(futures::poll!(start.as_mut()), Poll::Pending));
    let replacement = crate::state::ActiveTurn::default();
    let replacement_state = Arc::clone(&replacement.turn_state);
    *active = Some(replacement);
    drop(active);
    assert!(!start.await);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let active = session.active_turn.lock().await;
    assert!(active.as_ref().is_some_and(|turn| {
        turn.task.is_none() && Arc::ptr_eq(&turn.turn_state, &replacement_state)
    }));
}

#[tokio::test]
async fn monitor_delivery_waits_for_finishing_then_respects_shutdown() {
    for closing in [false, true] {
        let (session, _turn_context, _events) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let finishing = CancellationToken::new();
        *session.active_turn.lock().await = Some(crate::state::ActiveTurn {
            finishing: Some(finishing.clone()),
            ..Default::default()
        });
        let weak = Arc::downgrade(&session);
        let mut delivery = Box::pin(deliver_monitor_item(
            &weak,
            text_response_item("monitor after finishing".to_string()),
        ));
        assert!(matches!(futures::poll!(delivery.as_mut()), Poll::Pending));
        if closing {
            // Shutdown must release the delivery even if transcript recording is still blocked.
            session.begin_session_runtime_shutdown();
        } else {
            finishing.cancel();
            *session.active_turn.lock().await = None;
            session.active_turn_changed.notify_waiters();
        }
        delivery.await;

        assert!(session.active_turn.lock().await.is_some());
        if closing {
            assert!(!finishing.is_cancelled());
        }
        session
            .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
            .await;
    }
}

#[tokio::test]
async fn monitor_delivery_waits_without_spinning_after_transcript_recording() {
    struct WakeCounter(AtomicUsize);
    impl futures::task::ArcWake for WakeCounter {
        fn wake_by_ref(counter: &Arc<Self>) {
            counter.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let (session, _turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let finishing = CancellationToken::new();
    finishing.cancel();
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn {
        finishing: Some(finishing),
        ..Default::default()
    });
    let weak = Arc::downgrade(&session);
    let mut delivery = Box::pin(deliver_monitor_item(
        &weak,
        text_response_item("monitor after transcript recording".to_string()),
    ));
    // Start the manual poll with a fresh cooperative budget.
    tokio::task::yield_now().await;
    let wakes = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = futures::task::waker_ref(&wakes);
    let mut context = std::task::Context::from_waker(&waker);
    assert!(matches!(
        std::future::Future::poll(delivery.as_mut(), &mut context),
        Poll::Pending
    ));
    // Rechecking an already-cancelled transcript token would consume the cooperative
    // budget and wake itself. The lifecycle wait remains asleep until admission changes.
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);

    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    delivery.await;
    assert!(session.active_turn.lock().await.is_some());
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
}

#[tokio::test]
async fn monitor_delivery_keeps_running_turn_and_queues_an_internal_item() {
    let (session, _turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let active = crate::state::ActiveTurn::default();
    let turn_state = Arc::clone(&active.turn_state);
    *session.active_turn.lock().await = Some(active);
    let item = text_response_item("monitor event".to_string());

    deliver_monitor_item(&Arc::downgrade(&session), item.clone()).await;

    assert!(
        session
            .active_turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|active| { Arc::ptr_eq(&active.turn_state, &turn_state) })
    );
    assert_eq!(
        session
            .input_queue
            .take_pending_input_for_turn_state(&turn_state)
            .await,
        vec![TurnInput::ResponseItem(item.into())],
    );
}
