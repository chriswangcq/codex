# codex-qunux

`codex-qunux` is the Rust runtime boundary for Qunux native tools in Codex.

Qunux does not replace LLM reasoning. The LLM remains the intelligent CPU. Qunux provides the OS-like substrate that makes that CPU's work runnable, resumable, auditable, recoverable, parallel, and structurally closed.

## Runtime Model

The unified Codex-native Agent OS model is specified in
[`docs/agent-os-runtime-model.md`](docs/agent-os-runtime-model.md).
The fuzzy-wake watcher child-thread recipe is specified in
[`docs/watcher-thread-pattern.md`](docs/watcher-thread-pattern.md).
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

At process birth, Qunux creates exactly one ordinary root problem, `P000`. That
root problem is the process mission: serve the user well in this Codex session.
Qunux does not pre-create a root ticket, pre-classify `one_go`/`split`, create
children, record results/checks, or decide whether the thread should wait. The
LLM agent reads the root problem, the current event, and `next`, then chooses
the next legal PTRC move.

The root is a long-lived lifecycle mission, not an initialization task. If the
agent is idle and has no concrete user demand, the correct move is to park with
`qunux.wait` for user input or another wake event, not to close `P000`.
Unmatched user input is preserved in the passive inbox and surfaced as a
`handle_inbox` runnable action. Ordinary actionable inbox items are converted
into canonical PTRC child packages with `qunux.scaffold_user_task`, which
creates the child problem and its first solution ticket together. Non-durable
conversation is answered visibly by the assistant and then marked handled with
the state-only `qunux.ack_inbox`.

Qunux is headless-first. The TUI cockpit and chat transcript are important
human-facing surfaces, but they are not the process body. User input is one
passive world event source alongside shell output, file changes, tool output,
timers, webhooks, and external signals. Assistant messages are visible output;
Qunux tool calls mutate and inspect runtime state.

## TUI / OS Data Protocol

The native TUI is an attachable console for the Qunux process, not the OS
itself. The runtime owns durable process memory under `.qunux/processes/*` and
publishes snapshots; the TUI renders those snapshots and sends user input back
as passive world events.

The protocol has five separate lanes:

- Runtime state: problems, tickets, results, checks, threads, waits, handles,
  inbox items, passive events, and IO events in Qunux durable state.
- Snapshot lane: app-server/runtime pushes `qunux/snapshot` notifications for
  cockpit rendering. Snapshots are observation, not mutation.
- Input lane: fresh TUI/chat input is offered to Qunux as a `user_input`
  passive event. If it matches a wait, the waiting thread wakes; otherwise it
  becomes an inbox item.
- Visible output lane: assistant messages are the user-visible stdout of the
  LLM CPU. `qunux.ack_inbox` and other tools mutate state but do not answer the
  user by themselves.
- Recovery lane: failed child-thread handles are reported as `recover_thread`
  runnable frontier. Recovery returns the unfinished subtree to the parent so
  the parent can retry, spawn a replacement child, or choose another legal PTRC
  move.

Small-talk, acknowledgements, idle instructions, and narrow meta questions
should normally stay in the current thread: answer visibly, acknowledge any
inbox item, and park with `qunux.wait` when appropriate. Do not create a routing
child problem or spawn a child thread merely to move conversation plumbing
around.

Cases/tasks/problems are process resources, not the process itself. This lets a
single agent session keep durable work state while Qunux controls which thread
may mutate which subtree.

The task model is PTRC:

- Problem: the thing that must be solved.
- Ticket: the current proposed solution path.
- Result: what execution actually produced.
- Check: the problem-level judge that decides success or creates a follow-up.

The key invariant is that ticket completion is not problem completion. A ticket is done when it has a recorded result. A problem is done only after a success check proves that the original problem is closed, including split children and follow-ups.

Child creation has explicit provenance:

- `mode=split` creates plan-time children from a `split` ticket in `splitting`.
- `mode=spawn` creates run-time child problems from an executing `one_go`
  ticket when execution discovers a blocking subprogram.
- check-time follow-ups are created only by a `not_success` check.

The parent ticket cannot record its result while any child problem created from
that ticket remains open.

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
- `qunux.ingest_user_task`
- `qunux.scaffold_user_task`
- `qunux.ack_inbox`
- `qunux.create_problem`
- `qunux.create_ticket`
- `qunux.classify_ticket`
- `qunux.set_status`
- `qunux.result`
- `qunux.check`
- `qunux.spawn_thread`
- `qunux.wait`
- `qunux.join_thread`
- `qunux.recover_thread`
- `qunux.list_threads`
- `qunux.thread_status`
- `qunux.status`
- `qunux.validate`
- `qunux.render`

Handlers are registered through Codex core's normal tool registry path, not through shell commands, MCP, or app-server dynamic tools. The handlers bind state to the current turn workspace and call this crate directly.

`qunux.scaffold_user_task` converts an actionable pending inbox item, such as a
request to implement, investigate, run, fix, design, or review something, into
an ordinary child problem under the long-lived root mission and creates that
child's default solution ticket in the same syscall. It does not create a
separate task model; it creates normal PTRC problem/ticket records with
passive-event provenance. Use it for ordinary durable user work when the LLM
can author both the problem body and first ticket body immediately.

`qunux.ingest_user_task` is the lower-level fallback that converts a pending
inbox item into only the child problem. Use it when the first ticket must be
authored separately or the agent intentionally wants a problem-only scaffold.

`qunux.ack_inbox` only marks a passive inbox item handled in Qunux state. It is
not a visible assistant reply. If the inbox item requires an answer, the LLM CPU
must emit that chat response separately before or alongside the acknowledgement.

`qunux.wait` is the single model-facing Wait/Wake syscall. It only parks the
current thread on typed wait specs. Do not add separate public tools for user
input, timers, external signals, event keys, wake, consume, cancel, or inspect.
Those are runtime or host responsibilities.

The public model is `WaitHandle + EventKey + qunux.wait(...)`. `qunux.next`
remains a pure frontier query: it can report that a thread is waiting, but it
does not create, consume, or resolve waits.

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
thread identity. The caller identity is still injected by Codex. `join_thread`
is compatibility sugar for consuming a completed child-thread wait. Generic
handle consumption is a runtime/host operation, not a normal agent-facing
`qunux.wait` call.

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
state while preserving `qunux.next` as a pure frontier query.

## Native TUI Cockpit

The product UI for Qunux includes a native Codex TUI cockpit. When
`Feature::Qunux` is enabled, the chat transcript and composer remain the user IO
channel, while the cockpit renders runtime-pushed `qunux/snapshot`
notifications for the current Codex thread. Those snapshots make processes,
threads, tasks, waits, handles, events, and the current runnable frontier visible
in the terminal without asking the model to poll state files.

The composer and session machinery remain available so the user can still drive
the agent, but the shell is no longer only a transcript. It also carries a
runtime view of the LLM CPU's process/thread/task closure state.

The UI has two complementary channels:

- Status channel: the cockpit renders `qunux/snapshot` state. It shows the
  process, threads, agent-loop state, waits, handles, passive inbox, passive
  events, and next runnable frontier without asking the model to summarize the
  runtime.
- Narrative IO channel: chat remains stdin/stdout for the user and the LLM CPU.
  Qunux tools may also return concise runtime messages such as "spawned thread",
  "waiting for user input", "passive event woke thread", or "child is ready to
  join". These messages help the human follow the OS without replacing the
  structured process state.

This split is intentional: the cockpit is the process monitor; chat is the human
IO and runtime event narration surface.

The cockpit renders the current frontier as `runnable`, `io_wait`, or
`terminal`. A parked IO thread is shown as `parked_io` with its wait and handle
rows, so user-input waits, child-thread waits, and passive event routing are
inspectable without turning `qunux.next` into a polling loop.

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
