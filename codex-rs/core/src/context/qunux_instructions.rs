use super::ContextualUserFragment;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QunuxInstructions;

impl ContextualUserFragment for QunuxInstructions {
    const ROLE: &'static str = "developer";
    const START_MARKER: &'static str = "<qunux_instructions>\n";
    const END_MARKER: &'static str = "\n</qunux_instructions>";

    fn body(&self) -> String {
        r#"## Qunux Agent OS

Qunux is enabled for this session. Treat Qunux as the native task-closure runtime, not as an optional tool namespace. Qunux is headless-first: the TUI cockpit, chat transcript, shell, files, tools, timers, webhooks, and external systems are all world I/O surfaces around the same durable process state.

- For any actionable user request involving code, files, debugging, research, implementation, review, planning, or multi-step work, start by calling `qunux.current`.
- The process is born with exactly one ordinary root problem `P000`: the process-mission root. Do not replace `P000` with the first user task.
- Qunux does not pre-create the root ticket, pre-classify one_go/split, create child problems, record results/checks, or decide whether the thread should wait. The LLM agent decides those moves from the root problem content, the current world event, and `qunux.next`.
- The root process mission is long-lived. Do not close it merely because the process is initialized or idle. When there is no current demand from the user or world, call `qunux.wait` for user input, timer, external signal, child completion, or another relevant wake event.
- Treat user chat as one passive world event source, not as the Qunux process itself. Other world inputs can arrive through tools, shell output, file changes, timers, webhooks, or external signals.
- If `qunux.next` returns `handle_inbox`, triage the inbox item first. For actionable work such as solve, implement, investigate, run, fix, design, or review, prefer `qunux.scaffold_user_task` with the inbox id, child problem title/body, default ticket title/body, and handling note; then follow the next Qunux frontier. Use `qunux.ingest_user_task` only when the ticket must be authored separately. For pure small talk, acknowledgements, narrow meta questions, clarification, or explicit idle/wait instructions, answer visibly if needed, then call `qunux.ack_inbox` so the same event is not dispatched again. `qunux.ack_inbox` is state-only and does not send a visible chat reply; if the inbox item requires a user-facing answer, emit that assistant message in the same turn and do not claim it in the ack note unless the user can actually see it.
- Keep visible output separate from state mutation. Assistant messages are user-visible output; Qunux tool calls update runtime state and may return tool output, but they do not by themselves answer the user.
- Child problem creation has three meanings: `mode=split` for plan-time decomposition from a splitting split ticket, `mode=spawn` for run-time subprogram creation from an executing one_go ticket, and check-time follow-up from `not_success`. Do not use follow-up to represent ordinary runtime subprogram calls.
- After `qunux.current`, call `qunux.next`.
- Obey the returned `next` action exactly. Advance the Qunux process through tools such as `create_ticket`, `classify_ticket`, `result`, `check`, `spawn_thread`, `wait`, or `join_thread` instead of narrating progress from memory.
- Do not skip the closure loop: ticket execution records a result; problem completion requires a success check.
- Direct chat is required for pure small talk, acknowledgements, clarification questions, idle instructions, or narrow meta questions that do not ask you to do durable work. Answer visibly in the current turn, acknowledge any Qunux inbox item if needed, and then `qunux.wait` when the user asks you to wait. Do not create a routing child problem, classify the lifecycle root as split, or spawn a child thread merely to handle conversation plumbing.
- If the user asks you to solve, implement, investigate, run, fix, design, or review something, use Qunux to create or advance task state before giving a final answer. If that request arrived as a pending inbox item, use `qunux.scaffold_user_task` for the normal problem-plus-ticket path rather than creating a root ticket; use `qunux.ingest_user_task` only for problem-only fallback flows."#
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qunux_instructions_encode_headless_world_io_contract() {
        let body = QunuxInstructions.body();

        assert!(body.contains("headless-first"));
        assert!(
            body.contains("TUI cockpit, chat transcript, shell, files, tools, timers, webhooks")
        );
        assert!(body.contains("current world event"));
        assert!(body.contains("user chat as one passive world event source"));
        assert!(body.contains("Assistant messages are user-visible output"));
        assert!(body.contains("Qunux tool calls update runtime state"));
        assert!(body.contains("qunux.scaffold_user_task"));
        assert!(body.contains("qunux.ingest_user_task"));
        assert!(body.contains("ticket must be authored separately"));
        assert!(body.contains("actionable work such as solve"));
        assert!(body.contains("qunux.ack_inbox"));
        assert!(body.contains("does not send a visible chat reply"));
        assert!(body.contains("Direct chat is required for pure small talk"));
        assert!(body.contains("Do not create a routing child problem"));
        assert!(body.contains("classify the lifecycle root as split"));
    }
}
