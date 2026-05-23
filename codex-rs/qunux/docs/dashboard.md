# Qunux Runtime Dashboard

The Qunux runtime dashboard is a static, read-only inspector for runtime process
state. It turns `.qunux/processes/*/closure.json` into a self-contained HTML
file that can be opened locally when you need to understand a Qunux process
without invoking the model or mutating state.

This page is not the product UI and does not replace the Codex CLI. The product
UI includes the native Codex TUI cockpit as an attachable runtime view alongside
the transcript and composer. It is fed by runtime-pushed `qunux/snapshot`
notifications. The HTML dashboard is an offline debug artifact for inspecting
persisted snapshots.

## Generate

From the Codex repository root:

```bash
node codex-rs/qunux/scripts/render-dashboard.mjs --workspace /path/to/workspace
```

Default behavior:

- Reads every process state at `.qunux/processes/*/closure.json` in the selected
  workspace.
- Writes `.qunux/dashboard.html` in the selected workspace.
- Embeds the process states directly into the generated HTML.
- Requires Node only; no package install or web server is required.

Options:

```bash
node codex-rs/qunux/scripts/render-dashboard.mjs \
  --workspace /path/to/workspace \
  --process QP-example \
  --output /tmp/qunux-dashboard.html
```

- `--workspace`: workspace that contains `.qunux/`.
- `--process`: optional process id filter.
- `--output`: optional HTML output path.
- `--help`: print usage.

## Boundary

The dashboard is diagnostic UI, not a runtime controller and not the native TUI
cockpit.

It does not:

- create problems
- create or classify tickets
- record results
- run checks
- spawn or join threads
- edit `.qunux` state

All state changes must still happen through the Qunux native tools or the
runtime APIs. This boundary is deliberate: the dashboard is safe to open while
debugging a live process because it only reads an already persisted snapshot.
For live TUI rendering, app-server/runtime pushes the current process snapshot
to the thread that owns the Qunux actor.

## Panels

The top cards summarize the selected process, selected thread frontier, thread
counts, and open problem counts.

The problem tree shows the durable PTRC closure structure. Problems can contain
children from split tickets and follow-ups from failed checks.

The thread tree shows execution identity. A thread owns a problem subtree; child
threads are created by `spawn_thread` and joined back after their root problem
closes.

The detail panel shows the selected problem, thread, ticket, result, check,
handle, wait, or event. Body fields are rendered as preformatted Markdown text
so the dashboard preserves the ledger evidence exactly as stored.

The scheduler panel computes an offline diagnostic approximation of the next
frontier for the selected thread.

The lower tabs expose:

- threads and their next dispositions
- handles created for child execution
- waits that park parent threads
- state and IO events
- checks and follow-up links
- client-side reference diagnostics

## Next Dispositions

`runnable` means the LLM CPU can do useful work for the selected thread. The
dashboard should show the next action, target problem, and target ticket when
available.

`io_wait` means the selected thread is parked on a logical wait, usually a child
thread. The parent should not spend tokens polling; Codex dispatch preflight and
Qunux completion hooks are responsible for parking and waking.

`terminal` means the selected thread subtree has no remaining work.

These labels are for inspection. The source of truth remains the Rust runtime;
native `qunux.next` reports the same frontier without performing the wait.
