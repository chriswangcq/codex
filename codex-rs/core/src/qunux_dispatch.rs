use std::sync::Arc;

use crate::agent::status::is_final;
use crate::session::session::Session;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_qunux::CurrentState;
use codex_qunux::NextAction;
use codex_qunux::NextDisposition;
use codex_qunux::NextStep;
use codex_qunux::PassiveEventInput;
use codex_qunux::PassiveEventKind;
use codex_qunux::PassiveEventStatus;
use codex_qunux::QunuxRuntime;
use codex_qunux::ThreadStatus;
use tokio_util::sync::CancellationToken;

pub(crate) const QUNUX_USER_INPUT_DISPATCH_TURN_PREFIX: &str = "qunux-user-input-";

pub(crate) fn new_qunux_user_input_dispatch_turn_id() -> String {
    format!(
        "{QUNUX_USER_INPUT_DISPATCH_TURN_PREFIX}{}",
        uuid::Uuid::new_v4()
    )
}

pub(crate) fn is_qunux_user_input_dispatch_turn_id(turn_id: &str) -> bool {
    turn_id.starts_with(QUNUX_USER_INPUT_DISPATCH_TURN_PREFIX)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QunuxAutoDispatchWake {
    pub(crate) key: String,
    pub(crate) input: ResponseInputItem,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QunuxUserInputEvent {
    pub(crate) summary: String,
    pub(crate) payload_ref: Option<String>,
    pub(crate) dedupe_key: Option<String>,
    pub(crate) condition: Option<String>,
    pub(crate) source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QunuxUserInputOffer {
    Unavailable,
    Inboxed {
        event_id: String,
        inbox_item_id: Option<String>,
    },
    Matched {
        event_id: String,
        runnable_thread_ids: Vec<String>,
    },
    Duplicate {
        event_id: String,
    },
}

pub(crate) fn qunux_auto_dispatch_wake(
    session: &Session,
) -> CodexResult<Option<QunuxAutoDispatchWake>> {
    let Some(context) = session.services.qunux_runtime_context.clone() else {
        return Ok(None);
    };

    let runtime = QunuxRuntime::load(context).map_err(qunux_fatal)?;
    Ok(qunux_auto_dispatch_wake_from_current(&runtime.current()))
}

pub(crate) fn offer_user_input_to_qunux(
    session: &Session,
    event: QunuxUserInputEvent,
) -> CodexResult<QunuxUserInputOffer> {
    let Some(context) = session.services.qunux_runtime_context.clone() else {
        return Ok(QunuxUserInputOffer::Unavailable);
    };

    let mut runtime = QunuxRuntime::load(context).map_err(qunux_fatal)?;
    let target_thread_id =
        runtime.target_thread_id_for_passive_event_kind(PassiveEventKind::UserInput);
    let receipt = runtime
        .receive_passive_event(PassiveEventInput {
            kind: PassiveEventKind::UserInput,
            event_key: None,
            target_thread_id: Some(target_thread_id),
            condition: event.condition,
            source: event.source,
            summary: event.summary,
            payload_ref: event.payload_ref,
            dedupe_key: event.dedupe_key,
        })
        .map_err(qunux_fatal)?;

    let offer = match receipt.status {
        PassiveEventStatus::Matched => QunuxUserInputOffer::Matched {
            event_id: receipt.event_id,
            runnable_thread_ids: receipt
                .wake_decision
                .runnable_threads
                .into_iter()
                .map(|thread| thread.thread_id)
                .collect(),
        },
        PassiveEventStatus::Inboxed => QunuxUserInputOffer::Inboxed {
            event_id: receipt.event_id,
            inbox_item_id: receipt.inbox_item_id,
        },
        PassiveEventStatus::Duplicate => QunuxUserInputOffer::Duplicate {
            event_id: receipt.event_id,
        },
        PassiveEventStatus::Handled => QunuxUserInputOffer::Duplicate {
            event_id: receipt.event_id,
        },
    };
    Ok(offer)
}

pub(crate) fn qunux_auto_dispatch_wake_from_current(
    current: &CurrentState,
) -> Option<QunuxAutoDispatchWake> {
    if current.next.disposition != NextDisposition::Runnable {
        return None;
    }
    if is_bare_lifecycle_root_ticket_frontier(current) {
        return None;
    }

    let key = qunux_auto_dispatch_key(current);
    let text = qunux_auto_dispatch_text(current);
    Some(QunuxAutoDispatchWake {
        key,
        input: ResponseInputItem::Message {
            role: "developer".to_string(),
            content: vec![ContentItem::InputText { text }],
            phase: None,
        },
    })
}

fn is_bare_lifecycle_root_ticket_frontier(current: &CurrentState) -> bool {
    current.next.action == NextAction::CreateSolutionTicket
        && current.next.problem_id.as_deref() == Some(current.status.root_problem_id.as_str())
        && current.status.root_problem_id == current.status.thread_root_problem_id
        && current.status.total_problems == 1
        && current.status.total_tickets == 0
        && current.status.total_results == 0
        && current.status.total_checks == 0
        && current.status.total_handles == 0
        && current.status.passive_events == 0
        && current.status.inbox_items == 0
}

pub(crate) fn qunux_auto_dispatch_key(current: &CurrentState) -> String {
    let next = &current.next;
    format!(
        "{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        current.context.process_id,
        current.context.thread_id,
        current.status.root_problem_id,
        current.status.thread_root_problem_id,
        current.status.thread_status as u8,
        next_action_label(next.action),
        next.problem_id.as_deref().unwrap_or("-"),
        next.ticket_id.as_deref().unwrap_or("-"),
        next.target_thread_id.as_deref().unwrap_or("-"),
        next.reason
    )
}

fn qunux_auto_dispatch_text(current: &CurrentState) -> String {
    let next = &current.next;
    format!(
        "Qunux detected runnable work and is waking the Codex agent loop. This wake is a headless runtime scheduler frontier, not TUI/chat behavior.\n\n\
         Current Qunux context:\n\
         - process_id: {process_id}\n\
         - current_thread_id: {thread_id}\n\
         - root_problem_id: {root_problem_id}\n\
         - thread_root_problem_id: {thread_root_problem_id}\n\
         - next_action: {next_action}\n\
         - next_problem_id: {problem_id}\n\
         - next_ticket_id: {ticket_id}\n\
         - target_thread_id: {target_thread_id}\n\
         - reason: {reason}\n\n\
         Current Qunux next instruction:\n\
         {next_instruction}\n\n\
         Agent loop contract:\n\
         1. Start by calling `qunux.current`, then `qunux.next`, to confirm the live state.\n\
         2. Execute exactly the current Qunux next_action.\n\
         3. Do not invent completion outside Qunux result/check state.\n\
         4. For pure small talk, narrow meta questions, acknowledgements, or idle instructions, answer visibly in the current turn and wait if appropriate; do not create a routing child problem or spawn a child thread merely to handle conversation plumbing.\n\
         5. Do not choose one_go lightly; prefer child problems when the work is broad or risky.\n\
         6. If next_action is handle_inbox, triage the inbox input. For actionable work, prefer `qunux.scaffold_user_task` with the inbox id, child problem title/body, default ticket title/body, and handling note; then follow the next Qunux frontier. Use `qunux.ingest_user_task` only when the ticket must be authored separately. For pure small talk, acknowledgements, narrow meta questions, clarification, or idle/wait instructions, emit any required user-visible assistant reply in this turn, then call `qunux.ack_inbox` after it is handled. `qunux.ack_inbox` is state-only and is not visible to the user; do not claim a user-visible reply in the ack note unless you actually emitted one.\n\
         7. Keep visible output separate from state mutation: assistant messages are visible output; Qunux tools update runtime state.\n\
         8. If the right next step is user input or passive IO, call `qunux.wait` and park the agent loop.",
        process_id = current.context.process_id,
        thread_id = current.context.thread_id,
        root_problem_id = current.status.root_problem_id,
        thread_root_problem_id = current.status.thread_root_problem_id,
        next_action = next_action_label(next.action),
        problem_id = next.problem_id.as_deref().unwrap_or("-"),
        ticket_id = next.ticket_id.as_deref().unwrap_or("-"),
        target_thread_id = next.target_thread_id.as_deref().unwrap_or("-"),
        reason = next.reason,
        next_instruction = next.instruction
    )
}

fn next_action_label(action: NextAction) -> &'static str {
    match action {
        NextAction::CreateSolutionTicket => "create_solution_ticket",
        NextAction::DefineTicket => "define_ticket",
        NextAction::ClassifyTicket => "classify_ticket",
        NextAction::ExecuteTicket => "execute_ticket",
        NextAction::SplitTicket => "split_ticket",
        NextAction::SpawnThread => "spawn_thread",
        NextAction::WaitThread => "wait_thread",
        NextAction::WaitIo => "wait_io",
        NextAction::HandleInbox => "handle_inbox",
        NextAction::JoinThread => "join_thread",
        NextAction::RecoverThread => "recover_thread",
        NextAction::RecordResult => "record_result",
        NextAction::CheckSuccess => "check_success",
        NextAction::None => "none",
    }
}

pub(crate) async fn park_qunux_io_wait_before_dispatch(
    session: &Arc<Session>,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let Some(context) = session.services.qunux_runtime_context.clone() else {
        return Ok(());
    };

    let mut runtime = QunuxRuntime::load(context.clone()).map_err(qunux_fatal)?;
    loop {
        let next = runtime.next();
        if next.disposition != NextDisposition::IoWait {
            return Ok(());
        }
        if next.action == NextAction::WaitIo {
            return Err(CodexErr::TurnAborted);
        }
        if next.action != NextAction::WaitThread {
            return Err(CodexErr::Fatal(format!(
                "Qunux next returned unsupported IO-wait action {:?}; cannot park the agent loop",
                next.action
            )));
        }

        let target_thread_id = next.target_thread_id.clone().ok_or_else(|| {
            CodexErr::Fatal(
                "Qunux next returned wait_thread without target_thread_id; cannot park the agent loop"
                    .to_string(),
            )
        })?;
        let target_thread = runtime
            .state()
            .threads
            .get(&target_thread_id)
            .cloned()
            .ok_or_else(|| {
                CodexErr::Fatal(format!(
                    "Qunux next returned wait_thread for missing thread {target_thread_id}; cannot park the agent loop"
                ))
            })?;

        if !matches!(
            target_thread.status,
            ThreadStatus::Running | ThreadStatus::WaitingChildren | ThreadStatus::WaitingIo
        ) {
            return Ok(());
        }

        let actor_session_id = target_thread
            .codex_thread_id
            .clone()
            .or(target_thread.actor_session_id.clone())
            .ok_or_else(|| {
                CodexErr::Fatal(format!(
                    "Qunux thread {target_thread_id} is waiting but has no bound Codex actor; cannot park the agent loop"
                ))
            })?;
        let actor_thread_id = ThreadId::try_from(actor_session_id.as_str()).map_err(|err| {
            CodexErr::Fatal(format!(
                "Qunux thread {target_thread_id} has invalid Codex actor id {actor_session_id}: {err}; cannot park the agent loop"
            ))
        })?;

        wait_for_agent_final_status(
            session,
            cancellation_token,
            actor_thread_id,
            &target_thread_id,
        )
        .await?;

        runtime = QunuxRuntime::load(context.clone()).map_err(qunux_fatal)?;
        let reloaded_next = runtime.next();
        if same_wait_thread_target_is_child_passive_io_wait(
            &reloaded_next,
            runtime
                .state()
                .threads
                .get(&target_thread_id)
                .map(|thread| thread.status),
            &target_thread_id,
        ) {
            return Err(CodexErr::TurnAborted);
        }
        if reloaded_next.action == NextAction::WaitThread
            && reloaded_next.target_thread_id.as_deref() == Some(target_thread_id.as_str())
        {
            return Err(CodexErr::Fatal(format!(
                "Codex child actor for Qunux thread {target_thread_id} reached a final status, but Qunux still reports the same wait_thread state. The completion hook did not advance the wait; inspect qunux.status/thread_status instead of polling next."
            )));
        }
    }
}

fn same_wait_thread_target_is_child_passive_io_wait(
    next: &NextStep,
    target_thread_status: Option<ThreadStatus>,
    target_thread_id: &str,
) -> bool {
    next.action == NextAction::WaitThread
        && next.target_thread_id.as_deref() == Some(target_thread_id)
        && target_thread_status == Some(ThreadStatus::WaitingIo)
}

async fn wait_for_agent_final_status(
    session: &Arc<Session>,
    cancellation_token: &CancellationToken,
    actor_thread_id: ThreadId,
    target_thread_id: &str,
) -> CodexResult<()> {
    let mut status_rx = session
        .services
        .agent_control
        .subscribe_status(actor_thread_id)
        .await
        .map_err(|err| {
            CodexErr::Fatal(format!(
                "cannot subscribe to Codex actor {actor_thread_id} for Qunux thread {target_thread_id}: {err}; cannot park the agent loop"
            ))
        })?;

    let mut status = status_rx.borrow().clone();
    while !is_final(&status) {
        tokio::select! {
            changed = status_rx.changed() => {
                if changed.is_err() {
                    status = session
                        .services
                        .agent_control
                        .get_status(actor_thread_id)
                        .await;
                    break;
                }
                status = status_rx.borrow().clone();
            }
            () = cancellation_token.cancelled() => {
                return Err(CodexErr::TurnAborted);
            }
        }
    }

    if !is_final(&status) {
        return Err(CodexErr::Fatal(format!(
            "Qunux wait for child thread {target_thread_id} ended without a final Codex actor status: {status:?}"
        )));
    }

    Ok(())
}

fn qunux_fatal(error: codex_qunux::QunuxError) -> CodexErr {
    CodexErr::Fatal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context;
    use codex_qunux::CheckStatus;
    use codex_qunux::ContextForkPolicy;
    use codex_qunux::EntityKind;
    use codex_qunux::NextAction;
    use codex_qunux::PassiveEventInput;
    use codex_qunux::PassiveEventKind;
    use codex_qunux::RuntimeContext;
    use codex_qunux::TicketClassification;
    use core_test_support::TempDirExt;

    #[tokio::test]
    async fn preflight_is_noop_without_qunux_context() {
        let (session, _turn_context) = make_session_and_context().await;
        let session = Arc::new(session);

        park_qunux_io_wait_before_dispatch(&session, &CancellationToken::new())
            .await
            .expect("no qunux context should be a no-op");
    }

    #[tokio::test]
    async fn preflight_rejects_wait_without_bound_actor() {
        let (mut session, _turn_context) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("root context");
        let mut runtime =
            QunuxRuntime::load_or_init(context.clone(), "Root", "# Root").expect("runtime");

        let ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent")
            .expect("ticket");
        runtime
            .classify_ticket(&ticket, TicketClassification::Split, "needs child")
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &ticket, "splitting")
            .expect("splitting");
        let child_problem = runtime
            .create_problem_from_ticket("P000", &ticket, "Child", "# Child")
            .expect("child problem");
        runtime
            .spawn_thread(
                &child_problem,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("logical child");

        session.services.qunux_runtime_context = Some(context);
        let session = Arc::new(session);

        let err = park_qunux_io_wait_before_dispatch(&session, &CancellationToken::new())
            .await
            .expect_err("preflight must reject unparkable wait");
        assert!(
            err.to_string().contains("has no bound Codex actor"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn preflight_parks_passive_io_wait_without_fatal_error() {
        let (mut session, _turn_context) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("root context");
        let mut runtime =
            QunuxRuntime::load_or_init(context.clone(), "Root", "# Root").expect("runtime");
        runtime
            .wait_for_user_input(
                Some("reply".to_string()),
                Some("chat".to_string()),
                None,
                "need user reply",
            )
            .expect("user input wait");
        assert_eq!(runtime.next().action, NextAction::WaitIo);

        session.services.qunux_runtime_context = Some(context);
        let session = Arc::new(session);

        let err = park_qunux_io_wait_before_dispatch(&session, &CancellationToken::new())
            .await
            .expect_err("preflight should park the turn");
        assert!(matches!(err, CodexErr::TurnAborted));
    }

    #[test]
    fn same_wait_thread_helper_only_accepts_same_target_waiting_io() {
        let next = NextStep {
            action: NextAction::WaitThread,
            disposition: NextDisposition::IoWait,
            thread_id: "QT000".to_string(),
            target_thread_id: Some("QT001".to_string()),
            problem_id: Some("P001".to_string()),
            ticket_id: None,
            instruction: "wait for child".to_string(),
            reason: "child thread is still running".to_string(),
        };

        assert!(same_wait_thread_target_is_child_passive_io_wait(
            &next,
            Some(ThreadStatus::WaitingIo),
            "QT001"
        ));
        assert!(!same_wait_thread_target_is_child_passive_io_wait(
            &next,
            Some(ThreadStatus::Running),
            "QT001"
        ));
        assert!(!same_wait_thread_target_is_child_passive_io_wait(
            &next,
            Some(ThreadStatus::WaitingIo),
            "QT002"
        ));

        let mut runnable_next = next.clone();
        runnable_next.action = NextAction::CreateSolutionTicket;
        assert!(!same_wait_thread_target_is_child_passive_io_wait(
            &runnable_next,
            Some(ThreadStatus::WaitingIo),
            "QT001"
        ));
    }

    #[tokio::test]
    async fn auto_dispatch_wake_skips_bare_lifecycle_root_create_ticket() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context = RuntimeContext::for_session(temp_dir.abs(), "agent-1").expect("context");
        let runtime =
            QunuxRuntime::load_or_init(context.clone(), "Root", "# Root").expect("runtime");
        let current = runtime.current();
        assert_eq!(current.next.action, NextAction::CreateSolutionTicket);
        assert_eq!(current.next.disposition, NextDisposition::Runnable);

        let wake = qunux_auto_dispatch_wake_from_current(&current);
        assert!(
            wake.is_none(),
            "process birth alone should not spend a model turn creating the lifecycle root ticket"
        );
    }

    #[tokio::test]
    async fn auto_dispatch_wake_includes_handle_inbox_instruction_payload() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context = RuntimeContext::for_session(temp_dir.abs(), "agent-1").expect("context");
        let mut runtime = QunuxRuntime::load_or_init(context, "Root", "# Root").expect("runtime");
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some("QT000".to_string()),
                condition: Some("user-message".to_string()),
                source: Some("chat".to_string()),
                summary: "user input: text=please analyze OpenClaw".to_string(),
                payload_ref: Some("turn:42".to_string()),
                dedupe_key: Some("message-42".to_string()),
            })
            .expect("inbox event");
        let current = runtime.current();
        assert_eq!(current.next.action, NextAction::HandleInbox);

        let wake =
            qunux_auto_dispatch_wake_from_current(&current).expect("handle_inbox should wake");

        let ResponseInputItem::Message { content, .. } = wake.input else {
            panic!("wake should be a developer message");
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("wake should contain exactly one text item");
        };
        assert!(text.contains("next_action: handle_inbox"));
        assert!(text.contains("Current Qunux next instruction:"));
        assert!(text.contains("Handle pending inbox item IN000"));
        assert!(text.contains("user input: text=please analyze OpenClaw"));
        assert!(text.contains("qunux.ack_inbox"));
        assert!(text.contains("qunux.scaffold_user_task"));
        assert!(text.contains("qunux.ingest_user_task"));
        assert!(text.contains("ticket must be authored separately"));
        assert!(text.contains("For actionable work"));
        assert!(text.contains("For pure small talk"));
        assert!(text.contains("state-only"));
        assert!(text.contains("not visible to the user"));
        assert!(text.contains("emit any required user-visible assistant reply"));
        assert!(text.contains("do not create a routing child problem"));
        assert!(text.contains("headless runtime scheduler frontier"));
        assert!(text.contains("assistant messages are visible output"));
    }

    #[tokio::test]
    async fn auto_dispatch_wake_skips_io_wait_state() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context = RuntimeContext::for_session(temp_dir.abs(), "agent-1").expect("context");
        let mut runtime = QunuxRuntime::load_or_init(context, "Root", "# Root").expect("runtime");
        runtime
            .wait_for_user_input(
                Some("reply".to_string()),
                Some("chat".to_string()),
                None,
                "need user reply",
            )
            .expect("wait");
        let current = runtime.current();
        assert_eq!(current.next.action, NextAction::WaitIo);
        assert_eq!(current.next.disposition, NextDisposition::IoWait);

        assert!(qunux_auto_dispatch_wake_from_current(&current).is_none());
    }

    #[tokio::test]
    async fn auto_dispatch_wake_skips_terminal_state() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context = RuntimeContext::for_session(temp_dir.abs(), "agent-1").expect("context");
        let mut runtime = QunuxRuntime::load_or_init(context, "Root", "# Root").expect("runtime");
        let ticket = runtime
            .create_ticket("P000", "Root ticket", "# Ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&ticket, TicketClassification::OneGo, "small")
            .expect("classify");
        runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect("problem doing");
        runtime
            .set_status(EntityKind::Ticket, &ticket, "executing")
            .expect("ticket executing");
        let result = runtime
            .record_result(&ticket, "Root result", "# Result")
            .expect("result");
        runtime
            .check(
                "P000",
                CheckStatus::Success,
                vec![result],
                "Root check",
                "# Check",
                None,
            )
            .expect("check");
        let current = runtime.current();
        assert_eq!(current.next.action, NextAction::None);
        assert_eq!(current.next.disposition, NextDisposition::Terminal);

        assert!(qunux_auto_dispatch_wake_from_current(&current).is_none());
    }

    #[tokio::test]
    async fn offer_user_input_is_unavailable_without_qunux_context() {
        let (session, _turn_context) = make_session_and_context().await;

        let offer = offer_user_input_to_qunux(
            &session,
            QunuxUserInputEvent {
                summary: "user said hello".to_string(),
                payload_ref: Some("turn:1".to_string()),
                dedupe_key: Some("turn:1".to_string()),
                condition: None,
                source: None,
            },
        )
        .expect("offer should not fail without context");

        assert_eq!(offer, QunuxUserInputOffer::Unavailable);
    }

    #[tokio::test]
    async fn offer_user_input_inboxes_when_no_wait_matches() {
        let (mut session, _turn_context) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("context");
        QunuxRuntime::load_or_init(context.clone(), "Root", "# Root").expect("runtime");
        session.services.qunux_runtime_context = Some(context.clone());

        let offer = offer_user_input_to_qunux(
            &session,
            QunuxUserInputEvent {
                summary: "user said hello".to_string(),
                payload_ref: Some("turn:2".to_string()),
                dedupe_key: Some("turn:2".to_string()),
                condition: None,
                source: None,
            },
        )
        .expect("offer should be recorded");

        let QunuxUserInputOffer::Inboxed {
            event_id,
            inbox_item_id,
        } = offer
        else {
            panic!("expected user input to be inboxed without a matching wait");
        };
        assert_eq!(event_id, "PE000");
        assert_eq!(inbox_item_id.as_deref(), Some("IN000"));

        let runtime = QunuxRuntime::load(context).expect("runtime reload");
        assert_eq!(runtime.status().passive_events, 1);
        assert_eq!(runtime.status().pending_inbox_items, 1);
        let current = runtime.current();
        assert_eq!(current.next.action, NextAction::HandleInbox);
        assert!(qunux_auto_dispatch_wake_from_current(&current).is_some());
        runtime.validate().expect("valid runtime");
    }

    #[tokio::test]
    async fn offer_user_input_wakes_matching_wait_and_enables_dispatch() {
        let (mut session, _turn_context) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("context");
        let mut runtime =
            QunuxRuntime::load_or_init(context.clone(), "Root", "# Root").expect("runtime");
        runtime
            .wait_for_user_input(None, None, None, "need any user reply")
            .expect("park on user input");
        assert_eq!(runtime.next().action, NextAction::WaitIo);
        session.services.qunux_runtime_context = Some(context.clone());

        let offer = offer_user_input_to_qunux(
            &session,
            QunuxUserInputEvent {
                summary: "user answered".to_string(),
                payload_ref: Some("turn:3".to_string()),
                dedupe_key: Some("turn:3".to_string()),
                condition: None,
                source: None,
            },
        )
        .expect("offer should wake wait");

        let QunuxUserInputOffer::Matched {
            event_id,
            runnable_thread_ids,
        } = offer
        else {
            panic!("expected user input to match the parked wait");
        };
        assert_eq!(event_id, "PE000");
        assert_eq!(runnable_thread_ids, vec!["QT000".to_string()]);

        let runtime = QunuxRuntime::load(context).expect("runtime reload");
        let current = runtime.current();
        assert_eq!(current.status.thread_status, ThreadStatus::Running);
        assert_ne!(current.next.action, NextAction::WaitIo);
        assert!(qunux_auto_dispatch_wake_from_current(&current).is_some());
        runtime.validate().expect("valid runtime");
    }

    #[tokio::test]
    async fn matched_user_input_can_queue_qunux_dispatch_wake() {
        let (mut session, _turn_context) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("context");
        let mut runtime =
            QunuxRuntime::load_or_init(context.clone(), "Root", "# Root").expect("runtime");
        runtime
            .wait_for_user_input(None, None, None, "need any user reply")
            .expect("park on user input");
        assert_eq!(runtime.next().action, NextAction::WaitIo);
        session.services.qunux_runtime_context = Some(context);
        let session = Arc::new(session);

        let offer = offer_user_input_to_qunux(
            session.as_ref(),
            QunuxUserInputEvent {
                summary: "user answered".to_string(),
                payload_ref: Some("turn:4".to_string()),
                dedupe_key: Some("turn:4".to_string()),
                condition: None,
                source: None,
            },
        )
        .expect("offer should wake wait");

        assert!(matches!(
            offer,
            QunuxUserInputOffer::Matched {
                runnable_thread_ids,
                ..
            } if runnable_thread_ids == vec!["QT000".to_string()]
        ));
        assert!(session.queue_qunux_auto_dispatch_if_runnable().await);

        let pending = session.take_queued_response_items_for_next_turn().await;
        assert_eq!(pending.len(), 1);
        let ResponseInputItem::Message { role, content, .. } = &pending[0] else {
            panic!("queued dispatch should be a developer message");
        };
        assert_eq!(role, "developer");
        assert!(matches!(
            content.as_slice(),
            [ContentItem::InputText { text }] if text.contains("Qunux detected runnable work")
                && text.contains("next_action: create_solution_ticket")
        ));
    }

    #[tokio::test]
    async fn queue_qunux_auto_dispatch_suppresses_duplicate_key() {
        let (mut session, _turn_context) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("context");
        let mut runtime =
            QunuxRuntime::load_or_init(context.clone(), "Root", "# Root").expect("runtime");
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some("QT000".to_string()),
                condition: None,
                source: None,
                summary: "user asked for work".to_string(),
                payload_ref: Some("turn:duplicate".to_string()),
                dedupe_key: Some("turn:duplicate".to_string()),
            })
            .expect("inbox user work");
        session.services.qunux_runtime_context = Some(context);
        let session = Arc::new(session);

        assert!(session.queue_qunux_auto_dispatch_if_runnable().await);
        assert!(!session.queue_qunux_auto_dispatch_if_runnable().await);

        let pending = session.take_queued_response_items_for_next_turn().await;
        assert_eq!(pending.len(), 1);
    }

    #[tokio::test]
    async fn explicit_pending_input_resets_qunux_dispatch_key_without_queuing_wake() {
        let (mut session, _turn_context) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("context");
        QunuxRuntime::load_or_init(context.clone(), "Root", "# Root").expect("runtime");
        session.services.qunux_runtime_context = Some(context);
        let session = Arc::new(session);
        *session.qunux_auto_dispatch_key.lock().await = Some("stale-key".to_string());
        session
            .queue_response_items_for_next_turn(vec![ResponseInputItem::Message {
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "real user input".to_string(),
                }],
                phase: None,
            }])
            .await;

        assert!(session.prepare_pending_work_for_idle_turn().await);
        assert_eq!(*session.qunux_auto_dispatch_key.lock().await, None);
        let pending = session.take_queued_response_items_for_next_turn().await;
        assert_eq!(pending.len(), 1);
        let ResponseInputItem::Message { role, content, .. } = &pending[0] else {
            panic!("pending item should remain the explicit message");
        };
        assert_eq!(role, "user");
        assert!(matches!(
            content.as_slice(),
            [ContentItem::InputText { text }] if text == "real user input"
        ));
    }
}
