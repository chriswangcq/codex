# codex-qunux

`codex-qunux` is the Rust runtime boundary for Qunux native tools in Codex.

Qunux does not replace LLM reasoning. The LLM remains the intelligent CPU. Qunux provides the OS-like substrate that makes that CPU's work runnable, resumable, auditable, recoverable, parallel, and structurally closed.

## Runtime Model

The unified Codex-native Agent OS model is specified in
[`docs/agent-os-runtime-model.md`](docs/agent-os-runtime-model.md).
The next native lifecycle and IO iteration is specified in
[`docs/native-session-io-runtime-v1.md`](docs/native-session-io-runtime-v1.md).
The `next` scheduler/syscall contract is specified in
[`docs/next-runtime-contract.md`](docs/next-runtime-contract.md).
The read-only runtime dashboard is documented in
[`docs/dashboard.md`](docs/dashboard.md).

The native runtime stores Qunux process state under the active workspace. A
root Codex agent session is bound to one Qunux process. The process id is
derived from that session id and is not supplied by the model:

```text
.qunux/
  processes/
    QP-<codex-session-id>/
      closure.json
```

The hierarchy is:

```text
Codex agent session -> Qunux process
Codex child agent session -> Qunux thread inside the parent process
Qunux process -> threads + task tree
Qunux thread -> next-scoped problem subtree
Problem -> ticket/result/check closure loop
```

The process owns the durable task tree. Threads are execution identities bound
to subtrees of that process. The root thread starts at `P000`; child threads are
created with `spawn_thread`, bound to a child problem subtree, and joined back
by the parent after their root problem closes.

Cases/tasks/problems are process resources, not the process itself. This lets a
single agent session keep durable work state while Qunux controls which thread
may mutate which subtree.

The task model is PTRC:

- Problem: the thing that must be solved.
- Ticket: the current proposed solution path.
- Result: what execution actually produced.
- Check: the problem-level judge that decides success or creates a follow-up.

The key invariant is that ticket completion is not problem completion. A ticket is done when it has a recorded result. A problem is done only after a success check proves that the original problem is closed, including split children and follow-ups.

Thread invariants:

- `next` is scoped to the current Qunux thread root, not the whole process.
- Each problem has an `owner_thread_id`; mutating operations must stay inside the current thread subtree.
- Each event records the actor thread/session that caused it.
- `ContextFork` records how a child thread was forked from its parent, including the policy, bootstrap instruction, cwd, model, and inherited tool boundary.
- Qunux does not schedule instead of the LLM. It gives the LLM CPU fork/run/join/resume/audit state.
- A Qunux-backed agent loop has only three meaningful lifecycle modes:
  runnable work, logical IO wait, or destroyed. Child-thread waits are IO
  waits, so Codex parks the parent during dispatch preflight before prompt
  context is assembled or sent to the model. Native `qunux.next` still reports
  `wait_thread` for dashboards, tests, and state inspection; it does not own
  the wait.
- Logical waiting is owned by the Wait/Wake Kernel, which is the thread
  readiness kernel: a thread blocks on a `WaitHandle`, leaves the run queue,
  and only becomes runnable again when user input, child completion, tool
  output, approval, timers, or external events resolve that handle.
- Fuzzy or semantic wake does not need a special kernel primitive. Model it as
  a watcher child thread: the parent blocks on the normal child-thread handle,
  and the child periodically judges the semantic condition, records evidence,
  and closes when the criteria are met.

## Codex Native Tool Integration

Codex exposes Qunux through the experimental `qunux` feature. When enabled, Codex registers a model-visible `qunux` namespace with native Rust handlers:

- `qunux.current`
- `qunux.next`
- `qunux.create_problem`
- `qunux.create_ticket`
- `qunux.classify_ticket`
- `qunux.set_status`
- `qunux.result`
- `qunux.check`
- `qunux.spawn_thread`
- `qunux.join_thread`
- `qunux.list_threads`
- `qunux.thread_status`
- `qunux.status`
- `qunux.validate`
- `qunux.render`

Handlers are registered through Codex core's normal tool registry path, not through shell commands, MCP, or app-server dynamic tools. The handlers bind state to the current turn workspace and call this crate directly.

When the Qunux feature is enabled, ordinary model-visible `spawn_agent`
collaboration tools are suppressed. Qunux child work should be created through
`qunux.spawn_thread`, which allocates the durable Qunux thread before asking
Codex to fork a child agent.

Codex identity is injected by the handler from `ToolInvocation`; the model does
not pass the current `process_id` or current `thread_id`. Root session calls use
the current Codex session id to resolve the Qunux process. Spawned child-agent
calls use the parent Codex session id to resolve the parent process and the
child Codex session id to resolve the bound Qunux thread automatically.

Some tools accept `target_thread_id`, such as `qunux.join_thread` and
`qunux.thread_status`. That is a target resource id, not the caller's runtime
thread identity. The caller identity is still injected by Codex.

`qunux.spawn_thread` first creates the logical Qunux child thread and transfers
the target subtree to it. It then asks Codex `AgentControl` to fork a real child
agent with full history and a bootstrap prompt telling that child to call
`qunux.current` and `qunux.next` inside the bound subtree. There is no public
"logical thread only" mode; a Qunux child thread must have a Codex child actor
or enter an explicit failed recovery state.

When a child Codex actor completes after closing its Qunux root problem, the
completion hook auto-joins that child thread: the handle and wait are consumed,
`joined_at` is set, and the parent thread becomes runnable again. If the child
actor exits before its Qunux root is done, Qunux records a failed child-thread
state instead of pretending the child is still running.

The agent dispatch boundary is responsible for parking, not the model and not
the `qunux.next` tool call. Before Codex assembles prompt context for a Qunux
thread, dispatch preflight checks the Qunux frontier. If the thread is waiting
on a running child, Codex awaits the child actor's status notification, reloads
the Qunux process, and only then lets the parent continue to model dispatch.
This keeps the parent LLM from spending tokens polling a non-runnable wait
state while preserving `qunux.next` as a pure scheduler query.

## Native TUI Cockpit

The product UI for Qunux is the native Codex TUI cockpit. When `Feature::Qunux`
is enabled, the TUI's old chat-first main surface is replaced by a Qunux Agent
OS cockpit that renders runtime-pushed `qunux/snapshot` notifications for the
current Codex thread. Those snapshots make processes, threads, tasks, waits,
handles, events, and the current runnable frontier visible in the terminal
without asking the model to poll state files.

The composer and session machinery remain available so the user can still drive
the agent, but the primary shell is no longer just a transcript. It is a runtime
view of the LLM CPU's process/thread/task closure state.

Persisted `.qunux/processes/*/closure.json` files remain the durable runtime
record and the cockpit's fallback/debug source when no pushed snapshot has
arrived yet. The live product path is app-server/runtime -> `qunux/snapshot` ->
TUI cockpit: app-server emits an initial snapshot on turn start and refreshes it
after item completion and turn completion.

## Runtime Dashboard

Qunux process state can also be rendered into a static, read-only dashboard:

```bash
node codex-rs/qunux/scripts/render-dashboard.mjs --workspace /path/to/workspace
```

By default the renderer reads `.qunux/processes/*/closure.json` under the
workspace and writes `.qunux/dashboard.html`. Use `--process QP-...` to render a
single process, or `--output path/to/dashboard.html` to choose a different
output file.

The dashboard is diagnostic only. It does not mutate Qunux state. It shows the
process overview, problem and thread trees, selected entity details, scheduler
frontier, handles, waits, events, checks, and diagnostics. The scheduler panel
uses `runnable`, `io_wait`, and `terminal` to make the current thread frontier
visible without spending model tokens on polling.

This HTML dashboard is an offline debug artifact, not the primary Codex CLI UI.
The primary Qunux UI is the native TUI cockpit.
