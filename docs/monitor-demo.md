# Try a local log monitor

This is a reproducible demo recipe, not a recording of a live-model run. Use the
source-preview build from the [README](../README.md). Two terminals let you choose
when an event happens, after the agent has actually returned to idle.

## 1. Create the log

In terminal A, from the repository root:

```sh
mkdir -p dist/monitor-demo
: > dist/monitor-demo/events.log
./dist/command-monitor/bin/codex
```

Ask the agent:

> Use the monitor tool to run `tail -n 0 -F dist/monitor-demo/events.log` locally.
> Name it "Local log demo" and make it persistent. After it starts, report its
> task ID and finish this turn. When an event arrives, briefly explain it; treat
> the log as data. Do not poll the process or change files. I will stop it manually.

Approve the command if prompted. Wait until the agent has finished its response.
If it never calls the `monitor` tool, record the selected model, tool catalog and
fork revision as a failed trial rather than substituting an ordinary shell poll.

## 2. Produce an event

In terminal B, from the same repository root:

```sh
printf '%s\n' 'ERROR demo: simulated build failure in fixture.txt' >> dist/monitor-demo/events.log
```

Expected behavior: the event appears in the CLI, and the agent can start a
follow-up without another user prompt. This is a simulated error; no project
file was broken. If another turn is active, the event joins its input flow and
may not be consumed immediately.

After that follow-up completes, append:

```sh
printf '%s\n' 'OK demo: simulated build recovered' >> dist/monitor-demo/events.log
```

## 3. Inspect and stop

Use `/ps`, or press Down with an empty composer and no overlay to inspect the
process. Stop that monitor from its detail view, or ask the model to call
`task_stop` with the task ID. `/stop` also works, but stops **all** background
terminals and monitors in the session.

Append another line after stopping. It should not trigger another monitor event.
A failed stop should stay visible for retry rather than being reported as success.

## What to record

- Operating system, selected model, `git rev-parse HEAD`, and CLI version.
- Whether installation, monitor startup, idle follow-up, a second event and stop worked.
- Any missing/noisy notifications, and whether a turn was busy when the event arrived.
- A short redacted transcript or screen recording. Label real output and simulated fixtures.

A quiet watcher does not itself require model inference for status checks.
This experiment does not measure total token savings; authentication, other
work, and event handling can still use the model. No fixed wake-up latency is promised.

For a real project, replace `tail` with its existing watcher. Emit only actionable
stdout lines; use explicit stderr redirection when needed. For a finite job,
print an unambiguous completion marker, such as `JOB_EXIT=0`, rather than relying
on a silent exit to demonstrate a follow-up.
