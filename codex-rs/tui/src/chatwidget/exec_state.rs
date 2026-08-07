//! Unified exec bookkeeping state and helpers for `ChatWidget`.

use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
use codex_app_server_protocol::CommandMonitorInfo;
use codex_protocol::parse_command::ParsedCommand;
use std::time::Instant;

use crate::exec_command::split_command_string;

pub(super) struct RunningCommand {
    pub(super) command: Vec<String>,
    pub(super) parsed_cmd: Vec<ParsedCommand>,
    pub(super) source: ExecCommandSource,
}

pub(super) struct UnifiedExecProcessSummary {
    pub(super) key: String,
    pub(super) call_id: String,
    pub(super) command_display: String,
    pub(super) recent_chunks: Vec<String>,
    pub(super) kind: UnifiedExecProcessKind,
    pub(super) started_at: Instant,
}

pub(super) enum UnifiedExecProcessKind {
    BackgroundTerminal,
    Monitor(MonitorProcessState),
}

pub(super) struct MonitorProcessState {
    pub(super) info: CommandMonitorInfo,
    output_tail: String,
}

pub(super) struct MonitorOutputBatch {
    pub(super) description: String,
    pub(super) event: String,
}

const MAX_RECENT_TERMINAL_LINES: usize = 3;
const MAX_MONITOR_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_MONITOR_DETAIL_LINES: usize = 10;

impl UnifiedExecProcessSummary {
    pub(super) fn new(
        key: String,
        call_id: String,
        command_display: String,
        monitor: Option<CommandMonitorInfo>,
    ) -> Self {
        let kind = monitor.map_or(UnifiedExecProcessKind::BackgroundTerminal, |info| {
            UnifiedExecProcessKind::Monitor(MonitorProcessState {
                info,
                output_tail: String::new(),
            })
        });
        Self {
            key,
            call_id,
            command_display,
            recent_chunks: Vec::new(),
            kind,
            started_at: Instant::now(),
        }
    }

    pub(super) fn reset(
        &mut self,
        call_id: String,
        command_display: String,
        monitor: Option<CommandMonitorInfo>,
    ) {
        *self = Self::new(self.key.clone(), call_id, command_display, monitor);
    }

    pub(super) fn monitor_info(&self) -> Option<&CommandMonitorInfo> {
        match &self.kind {
            UnifiedExecProcessKind::BackgroundTerminal => None,
            UnifiedExecProcessKind::Monitor(state) => Some(&state.info),
        }
    }

    pub(super) fn record_output(&mut self, chunk: &str) -> Option<MonitorOutputBatch> {
        if matches!(&self.kind, UnifiedExecProcessKind::BackgroundTerminal) {
            self.push_recent_lines(
                chunk
                    .lines()
                    .map(str::trim_end)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string),
            );
            return None;
        }
        let (description, lines) = match &mut self.kind {
            UnifiedExecProcessKind::BackgroundTerminal => unreachable!(),
            UnifiedExecProcessKind::Monitor(monitor) => {
                let lines = chunk
                    .lines()
                    .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
                    .collect::<Vec<_>>();
                if !monitor.output_tail.is_empty()
                    && !monitor.output_tail.ends_with('\n')
                    && !chunk.starts_with('\n')
                {
                    monitor.output_tail.push('\n');
                }
                monitor.output_tail.push_str(chunk);
                truncate_utf8_tail(&mut monitor.output_tail, MAX_MONITOR_OUTPUT_BYTES);
                (monitor.info.description.clone(), lines)
            }
        };
        if let UnifiedExecProcessKind::Monitor(monitor) = &self.kind {
            let mut recent = monitor
                .output_tail
                .lines()
                .rev()
                .take(MAX_MONITOR_DETAIL_LINES)
                .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
                .collect::<Vec<_>>();
            recent.reverse();
            self.recent_chunks = recent;
        }
        let event = lines.join("\n");
        (!event.is_empty()).then_some(MonitorOutputBatch { description, event })
    }

    fn push_recent_lines(&mut self, lines: impl IntoIterator<Item = String>) {
        self.recent_chunks.extend(lines);
        if self.recent_chunks.len() > MAX_RECENT_TERMINAL_LINES {
            let drop_count = self.recent_chunks.len() - MAX_RECENT_TERMINAL_LINES;
            self.recent_chunks.drain(0..drop_count);
        }
    }
}

fn truncate_utf8_tail(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut start = value.len().saturating_sub(max_bytes);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value.drain(..start);
}

pub(super) struct UnifiedExecWaitState {
    command_display: String,
}

impl UnifiedExecWaitState {
    pub(super) fn new(command_display: String) -> Self {
        Self { command_display }
    }

    pub(super) fn is_duplicate(&self, command_display: &str) -> bool {
        self.command_display == command_display
    }
}

#[derive(Clone, Debug)]
pub(super) struct UnifiedExecWaitStreak {
    pub(super) process_id: String,
    pub(super) command_display: Option<String>,
}

impl UnifiedExecWaitStreak {
    pub(super) fn new(process_id: String, command_display: Option<String>) -> Self {
        Self {
            process_id,
            command_display: command_display.filter(|display| !display.is_empty()),
        }
    }

    pub(super) fn update_command_display(&mut self, command_display: Option<String>) {
        if self.command_display.is_some() {
            return;
        }
        self.command_display = command_display.filter(|display| !display.is_empty());
    }
}

pub(super) fn is_unified_exec_source(source: ExecCommandSource) -> bool {
    matches!(
        source,
        ExecCommandSource::UnifiedExecStartup | ExecCommandSource::UnifiedExecInteraction
    )
}

pub(super) fn is_standard_tool_call(parsed_cmd: &[ParsedCommand]) -> bool {
    !parsed_cmd.is_empty()
        && parsed_cmd
            .iter()
            .all(|parsed| !matches!(parsed, ParsedCommand::Unknown { .. }))
}

pub(super) fn command_execution_command_and_parsed(
    command: &str,
    command_actions: &[codex_app_server_protocol::CommandAction],
) -> (Vec<String>, Vec<ParsedCommand>) {
    (
        split_command_string(command),
        command_actions
            .iter()
            .cloned()
            .map(codex_app_server_protocol::CommandAction::into_core)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor_summary() -> UnifiedExecProcessSummary {
        UnifiedExecProcessSummary::new(
            "401".to_string(),
            "monitor-call".to_string(),
            "tail -f app.log".to_string(),
            Some(CommandMonitorInfo {
                task_id: "task-a1".to_string(),
                description: "deployment readiness".to_string(),
                timeout_ms: 300_000,
                persistent: false,
            }),
        )
    }

    #[test]
    fn monitor_batch_stays_single_event_and_detail_keeps_last_ten_lines() {
        let mut summary = monitor_summary();
        let output = (0..12)
            .map(|idx| format!("line {idx}"))
            .collect::<Vec<_>>()
            .join("\n");

        let batch = summary.record_output(&output).expect("monitor batch");

        assert_eq!(batch.event, output);
        assert_eq!(summary.recent_chunks.len(), 10);
        assert_eq!(
            summary.recent_chunks.first().map(String::as_str),
            Some("line 2")
        );
        assert_eq!(
            summary.recent_chunks.last().map(String::as_str),
            Some("line 11")
        );
    }

    #[test]
    fn monitor_output_tail_is_utf8_safe_and_bounded_to_eight_kib() {
        let mut summary = monitor_summary();
        summary.record_output(&"é".repeat(5_000));

        let UnifiedExecProcessKind::Monitor(monitor) = &summary.kind else {
            panic!("expected monitor state");
        };
        assert!(monitor.output_tail.len() <= MAX_MONITOR_OUTPUT_BYTES);
        assert!(monitor.output_tail.ends_with('é'));
    }

    #[test]
    fn monitor_output_tail_separates_authoritative_batches() {
        let mut summary = monitor_summary();

        summary.record_output("ready");
        summary.record_output("healthy");

        let UnifiedExecProcessKind::Monitor(monitor) = &summary.kind else {
            panic!("expected monitor state");
        };
        assert_eq!(monitor.output_tail, "ready\nhealthy");
        assert_eq!(summary.recent_chunks, vec!["ready", "healthy"]);
    }
}
