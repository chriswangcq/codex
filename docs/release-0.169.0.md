# Command Monitor 0.169.0 preview — macOS ARM64

Release tag: `monitor-v0.169.0-20261114`.

This independent Codex CLI fork lets local command stdout trigger follow-up in
an idle conversation or join the input flow of an active turn. It includes the
`monitor` and `task_stop` tools, process inspection, bounded output handling and
shared command approval, sandbox and process ownership.

## Download and run

Use the [download instructions](../README.md#download-and-try) and keep the complete
extracted directory. Run `./bin/codex` from that directory, then follow the
[two-terminal monitor demo](monitor-demo.md).

The archive contains the CLI, Code Mode host, executor, ripgrep, package manifest,
`BUILD.json`, license texts and notices. It does not install a global command.
Existing authentication/configuration follows normal Codex CLI behavior.

This is an Apple Silicon macOS preview, built and checked on macOS 15.7.4.
Intel macOS, Linux and Windows binaries are not included.

## Build identity and limitations

- Based on Codex CLI `0.169.0`; the CLI and `codex-package.json` both report that
  baseline. Include the `sourceCommit` from `BUILD.json` in feedback.
- Built with Rust 1.95.0, `cargo build --locked`, the `dev` profile and repository-
  verified Codex V8 152.2.0 artifacts. Symbols are stripped from the package copies.
  This is an unoptimized preview, so size and performance differ from an optimized
  production build.
- The packaged ripgrep 15.2.0 statically links PCRE2 10.49 with JIT and `-P` support.
  Its source patch, locked dependencies and build/runtime version evidence are in
  `build-provenance/ripgrep/`. The macOS ARM64 source-builder resource is pinned to
  the matching separately downloadable archive with size and SHA256 verification.
- The development profile excludes the upstream TUI update checker. Download
  subsequent fork releases from this repository; official installers install
  upstream Codex.
- Executables are ad-hoc signed, not Apple Developer ID signed or notarized.
  macOS may block software downloaded through a browser. After verifying the
  checksum, use System Settings → Privacy & Security → Open Anyway for this
  download if prompted. Do not disable Gatekeeper globally.
- Monitors are local and session-scoped. Persistent monitors do not survive a
  session restart. Output may be batched, truncated or rate-limited.
- Model access is not included. Live-model end-to-end validation and other host
  platforms have not been verified for this release.

## Remove or switch versions

Exit the running preview before removing its extracted directory. To switch to a
new preview, extract that version into a separate directory and run its explicit
`bin/codex` path. Shared Codex configuration and conversation history are stored
outside the package directory and are retained.

## Validation

The migration passed 2,825 Rust tests in the selected affected-area matrix,
9 Python contract checks, standard/experimental schema generation, formatting
and Clippy across 16 affected crates. See the [migration evidence](../FORK_NOTES.md#migration-validation)
for scope and exclusions. Package-specific results are recorded in `BUILD.json`.

## Feedback

[Open an issue](https://github.com/chriswangcq/codex/issues) with `BUILD.json`'s
source revision, platform, command, expected behavior and observed result.
Remove credentials and private logs before posting.
