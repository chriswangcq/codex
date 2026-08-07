# Codex Command Monitor Fork Notes

This document is the maintenance contract for the local command-monitor fork of Codex CLI. It
describes the frozen upstream baseline, the behavior this fork intends to preserve, what has and
has not been validated, and the boundaries that matter if upstream synchronization is considered
later.

This is a local fork. It is not an upstream proposal or a public compatibility promise. The
artifact recorded below is the stable local P0/P1 build for the validated host only.

## Status Snapshot

- Fork branch: `codex/command-monitor`
- Upstream repository: `https://github.com/openai/codex.git`
- Frozen upstream commit: `57f42a81131ccf5933e7ec5dc659c381eeb5d72b`
- Upstream commit date: 2026-08-06
- Upstream commit subject: `Avoid cloning immutable metadata on tool search cache hits (#37279)`
- Claude Code behavioral reference: 2.1.223, inspected locally on 2026-08-07
- Current status: stable local P0/P1 release recorded against code commit `4c5e9ff572`; P2 is
  intentionally paused
- Synchronization policy: do not merge or rebase upstream unless a separate synchronization
  decision is made
- P2 status: paused; additional platforms, remote monitor support, and WebSocket monitor support
  are not on the current implementation plan

The frozen SHA, not a moving upstream branch, is the authority for the base of this fork.

## What This Fork Adds

The fork adds a local command-monitor lifecycle to the existing unified-exec architecture:

- a model-visible `monitor` tool for starting a non-interactive local shell command;
- a model-visible `task_stop` tool for stopping a running monitor by task ID;
- bounded and persistent lifetimes;
- stdout physical-line events that can wake Codex without polling;
- combined stdout/stderr output capture for local process inspection;
- monitor-aware command lifecycle events and history replay;
- TUI history cells, a running-process footer, `/ps`, `/stop`, and a Down-arrow process manager;
- an experimental app-server endpoint for listing background terminals and monitor snapshots; and
- an optional monitor termination reason so timeout, user stop, session shutdown, and capacity
  termination are not reduced to an unexplained `exit 137` during replay.

The monitor is implemented as an extension of unified exec, not as a second process subsystem.
Unified exec remains the process-lifecycle authority.

## Local Release Bundle

The source-built local release requires two adjacent executables:

- `codex`, the CLI/TUI executable; and
- `codex-code-mode-host`, the Code Mode execution sidecar.

`InstallContext` resolves the sidecar next to the running CLI when no packaged resource layout is
present. Building or copying only `codex` leaves ordinary chat available but breaks Code Mode and
any tool path routed through it. Keep both executables in the same directory; the sidecar name must
remain exactly `codex-code-mode-host` even if the main executable is privately renamed.

## Platform Status

Only platforms that have been exercised on real hardware may be described as validated.

| Platform or surface | Status | Evidence or boundary |
| --- | --- | --- |
| macOS 15.7.4, Apple Silicon (`arm64`) | Validated development platform | Current implementation and manual TUI work were exercised here. The repository toolchain is Rust 1.95.0 via `codex-rs/rust-toolchain.toml`. |
| macOS Intel (`x86_64`) | Experimental | No real-machine run has been recorded. |
| Linux `x86_64` / `aarch64` | Experimental | Unix code paths exist, but no real Linux run has been recorded for this fork. |
| Windows `x86_64` / `arm64` | Experimental | Job-object, legacy sandbox, and elevated paths have source coverage, but no real Windows run has been recorded. |
| Local TUI sessions | Implemented | This is the primary supported surface for the fork. |
| Local app-server sessions | Experimental API | Monitor listing is behind `capabilities.experimentalApi`. |
| Remote execution environments | Unsupported | The monitor tool is exposed only when a local environment is available. |
| WebSocket monitoring | Unsupported | The fork accepts a shell command only. |

“Experimental” is a statement about missing validation, not a promise to implement or support the
surface in the current cycle. P2 platform expansion is paused.

## User-Visible Monitor Contract

### Start input and result

`monitor` accepts:

- `command`: required shell command or script;
- `description`: required short label used in events and TUI history;
- `timeout_ms`: defaults to 300,000 ms and must be at least 1,000 ms;
- `persistent`: defaults to `false`; and
- `environment_id`: present only where multiple local environments require selection.

For a non-persistent monitor, `timeout_ms` may not exceed 3,600,000 ms. For a persistent monitor,
the supplied deadline is ignored and the effective `timeoutMs` returned to the model is `0`.

The start result contains `taskId`, `timeoutMs`, and `persistent`. Task IDs are local monitor IDs
used by `task_stop`, notifications, lifecycle events, and the TUI process manager.

Commands containing control characters that would be hidden in an approval dialog are rejected.
Monitor commands otherwise use the same sandbox and approval path as shell commands. They are
started with `tty: false` and are not interactive terminal sessions.

### Event semantics

- Only stdout produces live monitor events.
- A physical stdout line is the input event boundary.
- CRLF input is normalized at the line boundary.
- An unterminated final stdout line is flushed when the stream ends normally.
- Multiple lines may be delivered in one event batch.
- stderr is captured for process details and archive output but does not wake the model as an
  event.
- The model is explicitly told not to poll or sleep after starting a monitor. An event can wake a
  turn even while Codex was waiting for the user; it is not treated as a user reply.

The line and batching limits intentionally use JavaScript-compatible UTF-16 units:

| Limit | Value |
| --- | ---: |
| Pending line buffer | 1,048,576 UTF-16 units |
| One emitted line | 500 UTF-16 units |
| One event batch | 3,000 UTF-16 units |
| Batch window | 200 ms |

Long content receives the literal suffix `...(truncated)`.

### Flood control

The delivery limiter has a capacity of 10 events and refills one token every 2 seconds. Sustained
suppression for more than 30 seconds stops the monitor. A quiet interval longer than 6 seconds
resets overload tracking.

The numeric thresholds and suppression accounting match the Claude Code 2.1.223 reference:
notices report events suppressed since the previous notice, and a later delivery after the quiet
interval resets stale suppression state without proactively emitting a quiet-period notice.

### Lifetime and termination

- A non-persistent monitor is terminated when its deadline fires.
- A persistent monitor runs until `task_stop`, session shutdown, capacity eviction, process exit,
  or an internal failure.
- Monitors are not durable jobs and are not resumed in a later Codex session.
- `task_stop` currently stops monitor tasks only and requires `task_id`.
- Natural process completion retains the normal completed/failed status and exit code.
- Controlled termination records an optional reason: `timedOut`, `userStopped`,
  `sessionShutdown`, `capacity`, or fallback `stopped`.

The timeout event text is:

```text
[Monitor timed out — re-arm if needed.]
```

The optional termination reason is backward compatible: older history without the field is read
as `None`. Current core, protocol, app-server, TUI, schema, replay, and hermetic TUI validation all
exercise this path.

### Output archive and cleanup

Combined process output is written under:

```text
$CODEX_HOME/monitor-tasks/<thread-id>/<task-id>.output
```

On Unix, the per-thread directory is set to mode `0700` and new archive files use mode `0600`.
Monitor archives share a 5 GiB budget. When the cap is reached, capture records a truncation marker
instead of allowing unbounded disk growth.

Controlled session shutdown requests monitor termination, waits for workers with bounded grace
periods, aborts workers that do not finish, and removes archives whose workers are no longer live.
A hard crash or `SIGKILL` can leave stale archives behind because there is no startup scavenger.
Treat the archive directory as potentially sensitive.

Unified exec has a soft capacity of 64 tracked processes. Capacity pruning can terminate a live
monitor, in which case the lifecycle records the `capacity` termination reason.

## Claude Code 2.1.223 Parity Matrix

Claude Code 2.1.223 is a pinned behavioral reference, not a source dependency or an ongoing
compatibility guarantee. The local public type declarations inspected for this comparison were
`MonitorInput`, `MonitorOutput`, `TaskStopInput`, and `TaskStopOutput` in the installed
`sdk-tools.d.ts`.

| Behavior | Claude Code 2.1.223 | This fork | Assessment |
| --- | --- | --- | --- |
| Tool name and display | `Monitor`, user-facing “Monitor” | Model tool `monitor`, TUI “Monitor” | Semantic match; naming follows Codex conventions |
| Default timeout | 300,000 ms | 300,000 ms | Match |
| Timeout bounds | Minimum 1,000 ms; non-persistent maximum 3,600,000 ms | Same | Match |
| Persistent lifetime | Effective timeout 0; runs until TaskStop or session end | Same | Match |
| Monitor source | Exactly one of shell command or WebSocket | Shell command required | Gap: no WebSocket |
| Hidden control characters | Rejects values hidden in approval UI | Rejects hidden command control characters | Match for command mode |
| Remote behavior | Forces non-persistent and caps timeout at 30 minutes | Monitor unavailable remotely | Unsupported rather than emulated |
| Start result | `taskId`, `timeoutMs`, optional `persistent` | Same values; `persistent` always present | Compatible extension |
| Start tool-result text | Includes task/lifetime and “do not poll or sleep” guidance | Same wording and semantics | Match |
| Event boundary | Each physical stdout line | Same | Match |
| stderr | Not a live event | Not a live event; retained in combined output | Match |
| Framing limits | 1 Mi UTF-16 pending, 500 line, 3,000 batch | Same | Match |
| Batch window | 200 ms | 200 ms | Match |
| Rate thresholds | Capacity 10, refill every 2 s, stop after more than 30 s, quiet reset after more than 6 s | Same numeric thresholds | Match |
| Suppression accounting | Notice count is since the previous notice; quiet state resets when a later delivery observes the quiet gap | Same | Match |
| Timeout notification | Exact timeout text above | Same | Match |
| TaskStop result | Message, task ID, task type, optional command | Same result shape for monitor tasks | Match within narrowed scope |
| TaskStop scope | Background tasks and agents; optional `task_id` plus deprecated `shell_id` | Monitor-only; required `task_id` | Intentional narrowing |
| TaskStop live event | Emits `[Monitor stopped]` | Same exact event text | Match |
| Natural completion | Process exit ends the watch | Same | Match |
| Controlled-stop replay | Renders stop/timeout semantics instead of only the signal exit code | Optional termination reason carries that distinction; timeout and user-stop replay were verified without `failed` or `exit 137` | Match for validated stop paths |
| Start timeout display | Seconds may be fractional, for example 1,500 ms as `1.5s` | Same decimal-second formatting | Match |
| Task manager | `/tasks` | `/ps`, `/stop`, and Down-arrow manager | Intentional Codex-native extension |
| WebSocket permission and SSRF checks | Implemented | Not applicable | Out of scope |

Do not copy Claude Code binary or minified implementation content into this repository. Keep the
comparison at the behavior and public-contract level, pinned to the inspected version.

## TUI Extensions

The fork surfaces monitor state through the existing command lifecycle rather than a parallel UI
history model.

- A start cell shows the monitor description, task ID, and either its deadline or `persistent`.
- stdout event batches appear as monitor event cells.
- completion cells show natural completion, failure, timeout, user stop, session shutdown, or
  capacity termination.
- active monitors participate in the unified-exec footer.
- `/ps` lists background terminals and monitors.
- `/stop` stops running monitors.
- pressing Down while the footer is active opens the background-process manager.
- monitor details show recent combined stdout/stderr output and allow stop actions.

TUI output is intentionally bounded:

- active TUI monitor state retains an 8 KiB UTF-8 tail and up to 10 detail lines;
- app-server-backed detail snapshots return an 8 KiB raw-byte tail;
- the detail view polls once per second while open; and
- a failed or 5-second timed-out snapshot request suspends polling for that target. Reopening the
  view or selecting another target starts a new polling lifecycle.

Ordinary background terminals retain their existing behavior; monitor metadata is optional, so
old command history and non-monitor commands remain representable.

## Validation Ledger

This ledger records evidence against the current P0/P1 implementation on 2026-08-07. Counts include
the current optional termination-reason path and regenerated protocol artifacts.

Status vocabulary:

- `PASS`: evidence applies to the current behavior under test.
- `PASS (automated only)`: automated coverage exists, but no corresponding manual scenario has
  been recorded.
- `ENV-FAIL`: the suite encountered a known host/environment failure unrelated to monitor logic.
- `PENDING`: not run, not completed, or superseded by a later code change.

### Automated evidence recorded on the development host

| Scope | Recorded result | Current interpretation |
| --- | --- | --- |
| Focused `codex-core` monitor tests | 71 passed | PASS |
| Full `codex-core` suite | 3,409 passed, 1 failed, 19 skipped, 3 flaky retries | ENV-FAIL: `managed_network_proxy_decider_survives_full_access_start` deterministically receives the host's local/private `example.com` rejection. The three flaky tests each passed on an exact single-thread rerun with retries disabled. |
| Full TUI suite | 3,462 passed, 4 skipped, no retries | PASS; no `.snap.new` or `.snap.pending` artifacts remain |
| Full app-server suite | 1,122 passed, 1 skipped, 2 flaky retries | PASS; the transient hosted-login HTTP 502 and shell-fork deadline cases each passed on an exact single-thread rerun with retries disabled |
| app-server-protocol suite | 291 passed, 1 skipped | PASS after stable and experimental schema regeneration |
| Supporting-crate batch | 1,096 of 1,097 passed, 3 skipped | ENV-FAIL: `delegated_http_failure_warning_redacts_request_url` assumes a closed localhost ephemeral port, which this host intercepts; the exact no-retry rerun remains environment-failed |
| `codex-utils-pty` | 25 passed | PASS (automated only) on macOS |
| Windows-sandbox subset run on macOS | 10 passed | PASS (automated only) for host-independent subset only; not Windows validation |
| Stable and experimental schema generation | Both writer modes passed; protocol suite passed afterward | PASS |
| Current debug CLI and hermetic TUI | Built from the current source and exercised against an isolated mock server and `CODEX_HOME` | PASS |
| Locked release build and isolated smoke | `codex-cli 0.0.0`, arm64 Mach-O, private install, clean-`CODEX_HOME` version smoke, checksum, and precise rollback | PASS |
| Code Mode release sidecar | Adjacent arm64 `codex-code-mode-host`; help and stdio startup/EOF shutdown smoke | PASS |

The full-suite environment failures above are not monitor regressions, but they remain visible in
the ledger rather than being rewritten as a clean full-suite pass.

### Hermetic manual TUI evidence

Recorded passing scenarios:

- bounded monitor timeout and exact timeout notification;
- persistent monitor start and continued operation;
- stdout event auto-wake without model polling;
- `task_stop`;
- `/ps`, Down-arrow detail view, and `/stop`;
- ANSI stripping, tabs, CJK text, and emoji rendering;
- 700-character line truncation behavior;
- combined stdout/stderr detail output;
- natural completion replay;
- timeout and user-stop replay with the explicit controlled reason and without `failed` or
  `exit 137`;
- session exit cleanup of a persistent monitor process and its archive; and
- a 10-minute persistent soak with 20 thirty-second heartbeats, `SOAK_DONE`, natural completion,
  and final process/archive cleanup.

Automated coverage also exists for no-output handling and deterministic flood control. A real
end-to-end flood scenario has not been recorded as passed.

Timeout and user-stop terminal reasons were verified end to end. Session-shutdown and capacity
terminal-reason propagation have deterministic automated coverage; session-shutdown process and
archive cleanup was also verified manually.

### Stable-release gate

Completed on 2026-08-07:

- focused core monitor tests after termination-reason propagation;
- full current core, TUI, app-server, protocol, and supporting-crate runs, with the two unrelated
  host failures and exact reruns retained in the ledger above;
- protocol serialization tests plus stable and experimental schema generation;
- TUI completion/replay tests and snapshot audit;
- manual timeout and user-stop replay without generic `failed · exit 137` output;
- manual session-exit process and archive cleanup; and
- the 10-minute persistent-monitor soak.

Release finalization completed on 2026-08-07:

- the locked release build completed with the pinned toolchain;
- the private install, checksum, clean-`CODEX_HOME` version smoke, and precise rollback passed; and
- the exact baseline, build-helper, and Monitor code commits are recorded below.

## Reproducible Build and Installation

The stable local artifact was built from the committed P0/P1 code with locked dependencies. Use
the same command when rebuilding it:

From `codex-rs`:

```sh
rustc -V
cargo -V
cargo build --locked --release -p codex-cli --bin codex
cargo build --locked --release -p codex-code-mode-host --bin codex-code-mode-host
./target/release/codex --version
./target/release/codex-code-mode-host --help
shasum -a 256 target/release/codex target/release/codex-code-mode-host
```

The repository pins Rust 1.95.0 in `codex-rs/rust-toolchain.toml`. Record the exact `rustc -V`,
`cargo -V`, fork commit, baseline commit, target triple, and SHA-256 alongside any distributed
binary.

### Recorded local artifact

| Property | Recorded value |
| --- | --- |
| Validation date | 2026-08-07 |
| Frozen upstream baseline | `57f42a81131ccf5933e7ec5dc659c381eeb5d72b` |
| Schema-writer commit | `b890de09ab` (`fix(build): use the app-server schema writer script`) |
| Monitor code commit | `4c5e9ff572` (`feat(command-monitor): add end-to-end local monitor support`) |
| Build command | `cargo build --locked --release -p codex-cli --bin codex` |
| Build result | PASS in 45m 30s; three unused-item warnings are in untouched frozen-baseline files |
| Rust | `rustc 1.95.0 (59807616e 2026-04-14)` |
| Cargo | `cargo 1.95.0 (f2d3ce0bd 2026-03-21)` |
| Host/format | `aarch64-apple-darwin`; Mach-O 64-bit arm64 |
| CLI version | `codex-cli 0.0.0` |
| Binary path | `codex-rs/target/release/codex` |
| Binary size | 282 MiB |
| SHA-256 | `08568c4573d73dfeb84b5cbf88f7055fc3e056da4b85a7c13729248a967a989f` |
| Code Mode host path | `codex-rs/target/release/codex-code-mode-host` |
| Code Mode host size | 61 MiB |
| Code Mode host SHA-256 | `65adfc61635f20893a90da70648f535aaa1a604f9030e43920b6497426a08c36` |
| Code Mode host smoke | `--help` passed; `--listen stdio </dev/null` exited successfully |

The release binary was copied to a `mktemp -d` private directory under the name `codex-monitor`,
run with a separate empty `CODEX_HOME`, and verified to have the same SHA-256. The private binary,
its generated `arg0` links, and both temporary directories were then removed explicitly. The
existing system command remained `/opt/homebrew/bin/codex`; it was never overwritten.

Install the fork under a non-conflicting name or explicit private path. Do not overwrite the
official `codex` binary during validation. One possible isolated smoke flow is:

```sh
MONITOR_INSTALL_DIR=/an/explicit/private/bin
MONITOR_SMOKE_HOME="$(mktemp -d)"
mkdir -p "$MONITOR_INSTALL_DIR"
install -m 0755 target/release/codex "$MONITOR_INSTALL_DIR/codex-monitor"
install -m 0755 target/release/codex-code-mode-host \
  "$MONITOR_INSTALL_DIR/codex-code-mode-host"
CODEX_HOME="$MONITOR_SMOKE_HOME" "$MONITOR_INSTALL_DIR/codex-monitor" --version
"$MONITOR_INSTALL_DIR/codex-code-mode-host" --listen stdio </dev/null
```

Before calling an installation repeatable, rebuild from the same clean fork commit in a fresh
checkout, compare the recorded metadata and checksum expectations, repeat the clean-home smoke,
and verify that removing the private binary restores the previous installation state. Exact
byte-for-byte reproducibility is not claimed until it has been measured.

## Known Limitations

- Only local shell-command monitors are implemented. WebSocket monitors are absent.
- Remote monitor behavior is unsupported; no Claude-style 30-minute remote cap is emulated.
- Platforms other than Apple Silicon macOS are experimental and unvalidated for this fork.
- Monitor commands are non-interactive (`tty: false`) and have no dedicated stdin workflow.
- Only stdout generates live events; stderr is visible through combined output inspection.
- ANSI control sequences are stripped from monitor event and detail rendering for safety and
  readability; this is an intentional display divergence from byte-preserving output.
- `task_stop` is monitor-only, requires `task_id`, and does not accept Claude Code's deprecated
  `shell_id` alias.
- TUI and app-server detail output is a bounded tail, not a complete log viewer.
- A failed or timed-out detail RPC suspends polling until the target lifecycle is restarted.
- App-server monitor listing is experimental and requires `capabilities.experimentalApi`.
- Hard process crashes can leave sensitive monitor archive files because cleanup is session-owned
  and there is no startup scavenger.
- The unified-exec process cap is 64; capacity pressure can terminate a monitor.
- Persistent monitors are session-scoped and are not durable or resumed after restart.
- A source-built installation is incomplete without the adjacent `codex-code-mode-host` sidecar;
  packaging only the main executable disables Code Mode execution.

## Deferred Upstream-Synchronization Risk Map

No large refactor is planned now. This section exists so a future synchronization effort does not
mistake high-conflict modules or duplicated lifecycle responsibilities for stable extension
points.

| Boundary | Current risk | Synchronization note |
| --- | --- | --- |
| `codex-rs/core/src/session/command_monitor.rs` | About 2,122 lines combining framing, rate limiting, archive I/O, delivery, recovery, and lifecycle | If upstream changes this area materially, first separate framing/rate/archive/delivery/lifecycle in a behavior-preserving commit. Do not do that refactor merely for style in the current cycle. |
| `codex-rs/core/src/unified_exec/process_manager.rs` | About 2,564 lines; monitor registry, process capacity, shutdown, archive cleanup, and generic exec management meet here | Preserve unified exec as the sole process-lifecycle authority. Avoid adding a second monitor-owned process registry during conflict resolution. |
| `codex-rs/core/src/unified_exec/process.rs` | About 1,166 lines and coupled to confirmed termination semantics | Reconcile upstream PTY/process termination changes before adapting monitor stop paths. |
| `codex-rs/tui/src/bottom_pane/background_processes_view.rs` | About 1,620 lines containing state, polling controller, rendering, actions, and tests | If upstream TUI churn forces a rewrite, split controller/model/render boundaries before adding more states. |
| Core protocol and rollout history | Monitor metadata and optional terminal outcome propagate through current and legacy events | Keep the terminal reason optional for backward compatibility and preserve one explicit outcome model. |
| app-server protocol and generated schemas | Small semantic changes produce wide generated-file churn | Change source protocol types first, regenerate deterministically, and keep generated outputs with the source change. |
| PTY and Windows sandbox paths | Unix, Windows legacy, elevated, and job-object termination paths interact | Treat source tests on macOS as insufficient; do not infer Windows support without real-platform validation. |
| Guardian, approvals, and attribution | Monitor reuses shell approval and command metadata paths | Preserve that reuse instead of introducing monitor-specific permission authority. |

A future synchronization should begin with a path inventory and contract tests, not a blind rebase.
Refactor only when an upstream conflict or a new supported surface makes the boundary necessary.
P2 work—real Linux/Windows/Intel Mac validation, remote execution, and WebSocket monitoring—remains
paused and is not implied by this risk map.

## Local Commit Structure

The fork uses a deliberately small buildable sequence on top of the frozen baseline:

1. `b890de09ab` fixes the root schema-writer recipe without changing Rust behavior.
2. `4c5e9ff572` contains the complete vertical Monitor slice: PTY and Windows termination
   plumbing, core lifecycle, protocol/history propagation, app-server API and generated schemas,
   TUI behavior, snapshots, and tests.
3. Documentation commits containing this file record validation, artifact, and required-sidecar
   evidence; they do not change either executable.

The functional code stays in one commit because the protocol fields, lifecycle authority, PTY
termination API, app-server consumers, and TUI consumers must change together to keep the
intermediate tree buildable. Tests remain with the behavior they define.

## Stable Release Checklist

A stable local release requires all of the following. This checklist closed on 2026-08-07:

- [x] the current work is represented by logical local commits on top of the frozen baseline;
- [x] the final recorded worktree is clean;
- [x] every item in the stable-release gate is recorded with a date and command/result;
- [x] environment-specific suite failures are documented with isolated rerun evidence;
- [x] only Apple Silicon macOS is labeled validated;
- [x] the release binary is built with `--locked` and the pinned Rust toolchain;
- [x] the release Code Mode host is built with `--locked`, placed next to the CLI, and smoke-tested;
- [x] the binary has a recorded SHA-256 and explicit private install/rollback evidence;
- [x] isolated-home start plus hermetic monitor stop, timeout, replay, soak, and shutdown cleanup
  are exercised; and
- [x] known limitations remain accurate for the exact release code commit.

“Stable” here means this local P0/P1 artifact on the validated host. It does not expand the P2
scope or imply support for other platforms, remote monitors, or WebSocket monitors.
