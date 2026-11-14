# Open-source preview launch

Prepared on 2026-11-14. The owner chose open-source adoption and demand validation
before monetization. This document contains research and **unsent** outreach drafts.

## Positioning

Local command output can trigger useful follow-up in the same Codex conversation.
The first audience is Apple Silicon CLI users working with logs, existing test
watchers, and long-running local jobs. The distribution is a community source
preview, with a bounded feature set and explicit platform/validation evidence.

The existing repository is already a public fork. Its previous default homepage
pointed to upstream installation, and feedback issues were disabled. The preview
branch supplies a fork-specific entry point, source-build instructions, a manual
demo recipe, and a short feedback form.

## Evidence and first outreach targets

Checked via GitHub on 2026-11-14; these three issues were open. Check their current
state again immediately before posting. Existing discussions already contain
community implementations, so disclose maintenance and explain this fork's scope.

| Priority | Discussion | Relevant need | Proposed contribution |
| --- | --- | --- | --- |
| 1 | [openai/codex #29922](https://github.com/openai/codex/issues/29922) | Agent-callable monitors for live local output | Invite a few macOS ARM64 users to a source preview; show stdout events, shared approval/sandbox and inspect/stop behavior. |
| 2 | [openai/codex #22003](https://github.com/openai/codex/issues/22003) | Inject background output into the conversation | Explain running-turn queue injection and idle-turn admission; link implementation boundaries. |
| Later | [openai/codex #33542](https://github.com/openai/codex/issues/33542) | Background completion callbacks | Relevant to long jobs with explicit output, but this preview is session-scoped rather than a durable detached scheduler. |

Start with one substantive comment on #29922. A second comment should add technical
information relevant to #22003, not repeat the first pitch. Avoid duplicate issues,
unsolicited mentions, direct-message campaigns and repetitive bumps. A discussion
of the problem is evidence of interest, not evidence that users will switch forks
or pay for one.

The threads already reference alternatives such as
[codex-long-jobs](https://github.com/zycdev/codex-long-jobs),
[codex-wake](https://github.com/alexx-ftw/codex-wake), and native monitor prototypes.
Those are author-reported scopes, not projects benchmarked here. Acknowledge them;
do not claim this is the first or only implementation. Our specific offering is
in-process monitoring of live stdout with CLI lifecycle and inspection support.

After the first successful external trials, share an actual recording with the
relevant Codex community and Chinese Rust/agent communities the maintainer already
participates in. Check each community's current self-promotion rules before posting.
A broad launch should follow a usable binary preview and a recorded real-model demo.

## Draft 1: #29922

Disclosure: I maintain a community fork of Codex CLI with a local `monitor` tool,
now migrated to the 0.169.0 stable baseline. I'm looking for a few Apple Silicon
CLI users to test the live-output workflow described here.

A monitor starts a local command through the existing approval/sandbox path.
Stdout lines become bounded, rate-limited notifications: they join the current
turn's input flow or can wake the idle conversation. The CLI exposes process
inspection and stopping through `/ps`, `/stop`, and the background-process view.

Source and build instructions:
https://github.com/chriswangcq/codex/tree/codex/monitor-preview

A two-terminal log demo is included. This is a source preview, not an official
release; there is no new prebuilt package or live-model end-to-end validation for
this migration yet. Watches
are session-scoped, stderr is archive-only unless redirected, and remote execution
is unsupported. The migration passed 2,825 selected Rust tests and 9 Python
contract tests; I don't have comparative token-saving measurements.

If you try it with a real log or existing test watcher, feedback on setup, idle
follow-up, output noise and stopping would be useful. The fork's Issues are open.

## Draft 2: #22003

Disclosure: I maintain a source-preview Codex CLI fork exploring this input path:
https://github.com/chriswangcq/codex/tree/codex/monitor-preview

For a running turn, its `monitor` tool delivers stdout notifications as internal
ResponseItems through the existing input queue. For an idle conversation it uses
the normal turn-admission path, including user-priority and shutdown checks.
The notification is not submitted as an answer to a pending user-input tool.

The command stays under unified_exec's approval, sandbox and process lifecycle;
failed stops remain addressable. This covers live local stdout events, not durable
cross-session scheduling or arbitrary remote event ingestion. Apple Silicon macOS
is the validated development platform, and the README includes a source-build and
two-terminal trial. I'd welcome feedback on busy/idle delivery and cleanup edge cases.

## Suggested first validation round

These are proposed targets, not observed adoption numbers:

- Invite 5–10 relevant users, then help them finish one real workflow.
- Record build/setup failures separately from monitor behavior failures.
- Ask whether follow-up happened while idle and busy, whether output was useful or
  noisy, and whether stop/cleanup behaved as expected.
- After a week, ask whether they used it again and what they still did manually.
- Decide on binary packaging and broader platform work from repeated use and
  concrete blockers. Stars alone do not demonstrate workflow value.

Use voluntary issues and replies for this round. Do not promise a savings
percentage, every-line delivery, a fixed wake-up latency, durable persistence,
automatic repair, or support for untested platforms.

## Before a binary release

Use the official canonical package layout and validate the actual distributed
artifact. The fork still needs a consistent displayed/package version, a deliberate
update source, licenses/notices for distributed resources, and install/rollback
plus live-model smoke evidence. The official upstream release workflows depend on
OpenAI signing/publishing infrastructure and should not be triggered as the fork's
release process.

Apache-2.0 permits distribution subject to its conditions; retain LICENSE/NOTICE,
attribution and clear modification notices. The license does not grant general
trademark rights. Keep independent-fork wording and use your own identity rather
than implying OpenAI endorsement. References:
[license](https://www.apache.org/licenses/LICENSE-2.0),
[OpenAI brand guidelines](https://openai.com/brand/).
