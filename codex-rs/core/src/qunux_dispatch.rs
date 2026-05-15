use std::sync::Arc;

use crate::agent::status::is_final;
use crate::session::session::Session;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_qunux::NextAction;
use codex_qunux::NextDisposition;
use codex_qunux::QunuxRuntime;
use codex_qunux::ThreadStatus;
use tokio_util::sync::CancellationToken;

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
        if reloaded_next.action == NextAction::WaitThread
            && reloaded_next.target_thread_id.as_deref() == Some(target_thread_id.as_str())
        {
            return Err(CodexErr::Fatal(format!(
                "Codex child actor for Qunux thread {target_thread_id} reached a final status, but Qunux still reports the same wait_thread state. The completion hook did not advance the wait; inspect qunux.status/thread_status instead of polling next."
            )));
        }
    }
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
    use codex_qunux::ContextForkPolicy;
    use codex_qunux::EntityKind;
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
}
