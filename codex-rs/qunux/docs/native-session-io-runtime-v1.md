# Qunux Native Session And IO Runtime V1

## Purpose

Qunux is the runtime substrate for agent work. It does not replace LLM
reasoning. The LLM remains the intelligent CPU; Qunux makes that CPU's work
runnable, resumable, auditable, recoverable, parallel, and parkable on IO.

This document defines the next native Codex integration iteration:

- Bind Codex root sessions to Qunux processes at session lifecycle time.
- Bind Codex child agent sessions to Qunux threads before child execution.
- Keep ordinary Codex `spawn_agent` outside the Qunux process tree.
- Model synchronous and asynchronous agent IO through handles, waits, and
  events.
- Wake parent threads through Qunux join/auto-join state when child work
  completes.

The design keeps the core rule simple: Qunux does not schedule instead of the
LLM. Qunux records process/thread/IO state so the LLM CPU can decide the next
action from a durable, legal, recoverable frontier.

The runtime lifecycle rule is stricter than ordinary prompt guidance: a
Qunux-backed agent loop should either run runnable work, be destroyed by the
session lifecycle, or park on logical IO. It should not keep sampling around a
known non-runnable wait state. Child-thread waits are logical IO waits.

## Current State

The current Qunux prototype already has the important lower layers:

- `codex-rs/qunux/src/lib.rs` stores process state, threads, problems,
  tickets, results, checks, and events.
- `Thread` includes `ContextFork`, `actor_session_id`, `codex_thread_id`, and
  subtree ownership.
- `spawn_thread` creates a logical child thread, transfers the target problem
  subtree, and marks the parent as `WaitingChildren`.
- `join_thread` joins a done child thread back into the current parent thread.
- `codex-rs/core/src/tools/handlers/qunux.rs` exposes native Qunux tools and
  injects identity from `ToolInvocation`.
- `qunux.spawn_thread` already calls Codex `AgentControl` with
  `SpawnAgentForkMode::FullHistory`.

The remaining gap is that this is still mostly tool-call driven. Qunux identity
is resolved lazily when a model calls a Qunux tool. The target state is native
session lifecycle binding: a Codex session has a Qunux identity before the
model asks for it.

## Non-Goals

This iteration does not:

- Intercept or replace ordinary Codex `spawn_agent`.
- Turn Qunux into a mechanical scheduler that chooses instead of the LLM.
- Implement complex retry, rollback, or failure recovery policies.
- Require the model to pass current `process_id` or current `thread_id`.
- Rely on prompts as the source of runtime identity.

Ordinary `spawn_agent` remains a Codex collaboration primitive. A Qunux OS
thread is created only through `qunux.spawn_thread`. When the Qunux feature is
enabled, ordinary model-visible `spawn_agent` tools are suppressed so agent
parallelism has one runtime entrypoint and cannot accidentally bypass the
Qunux process/thread tree.

## Runtime Vocabulary

### Process

A Qunux process is the durable state space for one root Codex agent session.
It owns:

- the problem tree
- tickets, results, checks
- Qunux threads
- IO handles and waits
- event log

The process id is derived from the root Codex session identity and is never
provided by the model.

### Thread

A Qunux thread is an execution identity bound to a problem subtree. A root
Codex session owns the main Qunux thread. A Qunux child thread is created by
`qunux.spawn_thread` and may have a child Codex agent session bound to it.

Only the owning thread may mutate its subtree.

### Handle

A handle represents an outstanding asynchronous object. The first required
handle type is `child_thread`; later extensions can add tool calls, user input,
timers, file watches, or external jobs.

### Wait

A wait records that a thread is blocked on one or more handles. A synchronous
operation such as `join_thread` is represented as waiting on a handle until
it is ready.

### Event

An event is a durable completion fact. Examples:

- `child_thread_spawned`
- `child_thread_done`
- `child_thread_joined`
- `actor_completed_without_thread_done`
- `handle_ready`

Events wake waits. `next()` should return runnable work only after wait/event
state permits it.

## Target Architecture

```text
Codex root session
  -> Qunux process
     -> main Qunux thread
        -> problem subtree P000

qunux.spawn_thread(P123)
  -> creates Qunux thread QT001
  -> creates child_thread handle H001
  -> transfers P123 subtree ownership to QT001
  -> passes Qunux binding metadata into Codex child spawn
  -> child Codex session starts already bound to QT001

child completes P123
  -> Qunux thread QT001 becomes done
  -> completion hook records child_thread_done / handle_ready
  -> auto-join marks H001 joined when legal
  -> parent thread becomes runnable
```

The important distinction is between execution identity and reasoning:

- Codex/LLM decides what to do.
- Qunux decides whether the requested state transition is legal.
- Qunux stores why a thread is runnable, waiting, done, or blocked.

## Session Lifecycle Binding

### Root Session

At `Session::new` time, Codex should initialize the Qunux process binding for
root sessions.

Target behavior:

1. Compute root actor id from the Codex session/thread id.
2. Derive Qunux process id from that actor id.
3. Load or create the Qunux process state.
4. Bind the Codex root actor id to the main Qunux thread.
5. Store enough session-local metadata so Qunux handlers can open the runtime
   without recomputing policy from scratch.

The handler can keep a recovery path for older or partially initialized
sessions, but the normal path should not be "first Qunux tool call creates the
process".

### Root Problem Initialization

Session binding may happen before the user has stated the real Qunux root
problem. To avoid default placeholder tasks becoming permanent state, the
runtime should support one of these:

- `init_root(title, body)` that replaces the placeholder root while it is still
  empty.
- `current(title, body)` continues to initialize root content, but only when
  the root is unmodified.

The invariant is that process binding and problem definition are separate:
process identity exists at session start; task content can be supplied by the
first Qunux action.

### Child Session

A child Codex session created by `qunux.spawn_thread` should receive Qunux
binding metadata before its first turn runs:

- `qunux_process_id`
- `qunux_thread_id`
- `qunux_root_problem_id`
- `qunux_parent_thread_id`

This metadata is runtime identity, not prompt content. The bootstrap prompt may
still tell the child what to do, but the child should be bound even if the
prompt is ignored.

## Spawn Pre-Binding

`qunux.spawn_thread` is the only operation that creates a Qunux OS thread.

Target flow:

1. Parent Qunux runtime validates the target problem is writable by the parent
   thread and is not the parent thread root.
2. Runtime creates a child Qunux thread and child-thread handle.
3. Runtime transfers subtree ownership to the child thread.
4. Qunux native handler calls Codex `AgentControl` with full-history fork.
5. Spawn options or session source metadata carries Qunux binding metadata.
6. Child `Session::new` binds to the existing Qunux thread before the first
   model turn.
7. The child bootstrap instruction says to call `qunux.current` then
   `qunux.next`, but this is guidance, not identity.

If Codex child spawn fails after logical thread creation, Qunux records an
explicit child spawn failure event and moves the child thread/handle to failed
terminal state so the parent can see a recovery action instead of waiting
forever. Full rollback/retry policy is deferred.

## IO Runtime Model

Qunux should treat all blocking operations as IO.

### Synchronous Surface

The agent-facing synchronous operation can remain simple:

```text
join_thread(QT001)
```

If the child is done, join completes. If not, the operation should either
return a clear not-ready error or represent the current thread as waiting on
the child handle.

### Asynchronous Substrate

The runtime stores:

```text
Handle {
  id
  kind
  owner_thread_id
  target_thread_id?
  status: pending | ready | consumed | failed | cancelled
}

Wait {
  id
  thread_id
  handle_ids
  mode: all | any
  status: waiting | ready | consumed
}

IoEvent {
  id
  handle_id?
  thread_id?
  kind
  status
  message
}
```

For this iteration, the only required handle kind is `child_thread`.

### Runnable Frontier

`next()` should explain waiting rather than leaking invalid work:

- A running thread with a runnable problem returns the next action.
- A waiting thread returns `WaitThread` or `JoinThread` when a child handle is
  ready.
- A parent waiting on children does not receive normal subtree work until its
  waits are ready or joined.
- A done child thread is joinable.

This preserves the LLM-as-CPU model. The LLM still chooses to call the next
tool, but Qunux describes the legal frontier precisely.

There are two surfaces:

- Pure `QunuxRuntime::next()` is deterministic state inspection. It may return
  `WaitThread` so renderers, tests, and diagnostics can show why a thread is
  not runnable.
- Native Codex dispatch preflight is the parking surface. Before prompt context
  is assembled for a Qunux-backed turn, Codex checks the same pure frontier. If
  the current thread is waiting on a running child-thread wait, preflight
  asynchronously awaits the child actor status notification, reloads Qunux
  state, and only then lets the parent continue to model dispatch. If the wait
  cannot be parked because identity or status subscription is missing, preflight
  fails explicitly rather than invite LLM polling.
- Native Codex `qunux.next` is intentionally a pure tool surface over
  `QunuxRuntime::next()`. It reports `WaitThread` / `io_wait` for state
  inspection, diagnostics, and UI, but it does not await child completion.

## Completion And Auto-Join

Codex already has child completion hooks for spawned agents. Qunux should use
that signal as an IO event source.

When a child Codex agent reaches a final status:

1. Resolve the Qunux process/thread bound to that Codex child.
2. If the Qunux child thread root problem is done, record:
   - `child_thread_done`
   - `handle_ready`
   - `child_thread_joined` when parent join is legal
   - consumed handle/wait state
3. If the Codex child completed but the Qunux thread root is not done, record:
   - `actor_completed_without_thread_done`
   - `child_thread_failed`
   - the child agent status
   - the child Qunux thread id
4. Do not force a join for incomplete Qunux threads.

This creates a clean boundary: normal success auto-joins and wakes the parent;
incomplete agent exit becomes failed recovery state for the parent LLM to
handle.

## Tool Surface

Existing Qunux tools stay valid:

- `current`
- `next`
- `create_problem`
- `create_ticket`
- `classify_ticket`
- `set_status`
- `result`
- `check`
- `spawn_thread`
- `join_thread`
- `list_threads`
- `thread_status`
- `status`
- `validate`
- `render`

Potential additions after the base model lands:

- `list_handles`
- `handle_status`
- `list_events`
- `await_handle`

For v1, explicit public IO tools are optional if `spawn_thread`, `join_thread`,
`next`, and `status` expose enough wait/handle/event information.

## Testing Plan

### Runtime Tests

Runtime tests should cover:

- session-bound process creation
- child-thread handle creation during `spawn_thread`
- parent wait state after child spawn
- child done event readiness
- legal join consuming a ready child handle
- incomplete child completion event does not force join
- `next()` remains scoped to the current thread root
- subtree ownership enforcement remains intact

### Core Handler Tests

Core tests should cover:

- root handler/session identity opens the bound Qunux process
- child handler/session identity opens the parent process and child thread
- model-supplied current identity fields remain rejected
- `qunux.spawn_thread` passes Qunux binding metadata into the spawn path
- completion hook calls Qunux auto-join or records incomplete child event

### End-To-End Style Test

The closest practical test should prove:

1. parent session has Qunux process
2. parent creates split child problem
3. parent calls `qunux.spawn_thread`
4. child session starts bound to Qunux thread
5. child `current/next` sees its subtree
6. child completes result/check
7. completion hook wakes or auto-joins parent

If the full production loop is too expensive for a unit test, use the existing
thread manager and handler test harnesses to cover the same transitions.

## Rollout Order

1. Land this design document.
2. Add runtime IO handle/event/wait structs and runtime tests.
3. Add session binding metadata types and handler resolution changes.
4. Add Qunux spawn pre-binding through Codex spawn options/session source.
5. Add completion auto-join hook.
6. Add end-to-end style test.
7. Update README and tool descriptions.
8. Run final verification matrix.

## Open Boundaries

- Whether Qunux binding metadata should live in `SpawnAgentOptions`,
  `SessionSource::SubAgent(ThreadSpawn)`, or session extension data should be
  decided during implementation. The selection should minimize prompt
  coupling and avoid exposing runtime identity to the model.
- If root session binding needs placeholder root task content, runtime should
  make that state explicit and replaceable exactly once.
- Complex child failure recovery is deferred. The required behavior is clear
  event/state reporting, not automatic repair.
