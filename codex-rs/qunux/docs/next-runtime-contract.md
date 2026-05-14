# Qunux Next Runtime Contract

`next` is the Qunux runnable-frontier scheduler boundary.

It is not a time-slice scheduler. It does not preempt the LLM, choose between
multiple live CPUs, or replace reasoning. The LLM remains the intelligent CPU.
Qunux decides whether the current Qunux thread has legal work to run, must park
on logical IO, or has reached terminal closure.

The contract is:

```text
next = pick_next_runnable_or_park(thread)
```

## Dispositions

Every `NextStep` has a scheduler disposition:

- `runnable`: the current thread has exactly one legal frontier action.
- `io_wait`: the current thread is blocked on logical IO and should not spend
  LLM tokens polling.
- `terminal`: the current thread has no open work in its scoped subtree.

The current mapping is:

| disposition | actions |
| --- | --- |
| `runnable` | `create_solution_ticket`, `define_ticket`, `classify_ticket`, `execute_ticket`, `split_ticket`, `spawn_thread`, `join_thread`, `recover_thread`, `record_result`, `check_success` |
| `io_wait` | `wait_thread` |
| `terminal` | `none` |

This field is part of the runtime protocol so tools and dashboards do not need
to infer scheduler state from action names.

## Pure Runtime Surface

`QunuxRuntime::next()` is deterministic state inspection. It does not mutate
state and it does not block.

It returns the current scoped frontier for the calling Qunux thread:

- a runnable action when the thread can make progress,
- an IO-wait action when progress depends on a handle/event,
- `none` when the scoped subtree is closed.

This pure surface is useful for tests, renderers, dashboards, and debugging.
It may return `io_wait` directly because those consumers need to see why a
thread is not runnable.

## Native Codex Tool Surface

Native Codex `qunux.next` is an agent-loop system call.

When pure `next()` returns `runnable` or `terminal`, the native tool returns the
step directly. When pure `next()` returns `io_wait`, the native tool should park
the current agent loop on the relevant logical IO source when it knows how to
do so.

Today the supported IO wait is a child-thread wait:

1. resolve the target Qunux child thread,
2. require a bound Codex actor id,
3. subscribe to the child actor status,
4. await final actor status,
5. reload Qunux state,
6. return the fresh next step.

If native `qunux.next` cannot park an IO wait safely, it must fail explicitly.
It must not hand the parent LLM an ordinary instruction that invites polling.

## Invariants

- `next` is scoped to the current Qunux thread root.
- `next` returns one frontier, not a plan.
- `next` never asks a thread to mutate outside its owned subtree.
- `next` never marks a problem successful; checks do that.
- `next` never replaces LLM judgment; it constrains the legal state frontier.
- `io_wait` is not a task. It is a runtime park state.
- Native `qunux.next` may block asynchronously on IO; pure `QunuxRuntime::next`
  never blocks.

## Future Extensions

The current scheduler is cooperative and single-frontier per thread. Future
runtime work can add policy without changing the core contract:

- priority across cases,
- deadlines,
- timer handles,
- file or external-job handles,
- fair queueing across runnable threads,
- starvation prevention,
- explicit cancellation and timeout recovery.

Those features should extend the frontier selection policy and IO-handle set,
not turn Qunux into a replacement for the LLM CPU.
