//! Claude-style command monitor lifecycle and event history cells.

use super::*;
use codex_ansi_escape::ansi_escape_line;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::CommandMonitorTerminationReason;

#[derive(Debug)]
pub(crate) struct MonitorStartedCell {
    description: String,
    task_id: String,
    timeout_ms: u64,
    persistent: bool,
}

impl HistoryCell for MonitorStartedCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        monitor_lines(
            width,
            monitor_header_spans(&self.description),
            monitor_started_detail(&self.task_id, self.timeout_ms, self.persistent),
        )
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(monitor_header_spans(&self.description)),
            ansi_escape_line(&monitor_started_detail(
                &self.task_id,
                self.timeout_ms,
                self.persistent,
            )),
        ]
    }
}

#[derive(Debug)]
pub(crate) struct MonitorEventCell {
    description: String,
    event: String,
}

#[derive(Debug)]
pub(crate) struct MonitorCompletedCell {
    description: String,
    task_id: String,
    status: CommandExecutionStatus,
    termination_reason: Option<CommandMonitorTerminationReason>,
    aggregated_output: Option<String>,
    exit_code: Option<i32>,
}

impl HistoryCell for MonitorCompletedCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = monitor_lines(
            width,
            monitor_header_spans(&self.description),
            monitor_completed_detail(
                &self.task_id,
                &self.status,
                self.termination_reason.as_ref(),
                self.exit_code,
            ),
        );
        let Some(aggregated_output) = self
            .aggregated_output
            .as_ref()
            .filter(|output| !output.is_empty())
        else {
            return lines;
        };
        let output = CommandOutput::new(
            self.exit_code.unwrap_or_default(),
            aggregated_output.clone(),
        );
        let output = output_lines(
            Some(&output),
            OutputLinesParams {
                line_limit: TOOL_CALL_MAX_LINES,
                only_err: false,
                include_angle_pipe: false,
                include_prefix: false,
            },
        );
        for line in output.lines {
            let wrapped = adaptive_wrap_line(
                &line,
                RtOptions::new(width as usize)
                    .initial_indent(Line::from("     "))
                    .subsequent_indent(Line::from("     ")),
            );
            push_owned_lines(&wrapped, &mut lines);
        }
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(monitor_header_spans(&self.description)),
            ansi_escape_line(&monitor_completed_detail(
                &self.task_id,
                &self.status,
                self.termination_reason.as_ref(),
                self.exit_code,
            )),
        ];
        if let Some(output) = self.aggregated_output.as_ref() {
            lines.extend(output.lines().map(ansi_escape_line));
        }
        lines
    }
}

impl HistoryCell for MonitorEventCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        monitor_lines(
            width,
            monitor_event_header_spans(&self.description),
            self.event.clone(),
        )
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        std::iter::once(Line::from(monitor_event_header_spans(&self.description)))
            .chain(self.event.lines().map(ansi_escape_line))
            .collect()
    }
}

pub(crate) fn new_monitor_started(
    description: String,
    task_id: String,
    timeout_ms: u64,
    persistent: bool,
) -> MonitorStartedCell {
    MonitorStartedCell {
        description,
        task_id,
        timeout_ms,
        persistent,
    }
}

pub(crate) fn new_monitor_event(description: String, event: String) -> MonitorEventCell {
    MonitorEventCell { description, event }
}

pub(crate) fn new_monitor_completed(
    description: String,
    task_id: String,
    status: CommandExecutionStatus,
    termination_reason: Option<CommandMonitorTerminationReason>,
    aggregated_output: Option<String>,
    exit_code: Option<i32>,
) -> MonitorCompletedCell {
    MonitorCompletedCell {
        description,
        task_id,
        status,
        termination_reason,
        aggregated_output,
        exit_code,
    }
}

pub(crate) fn format_monitor_timeout(timeout_ms: u64) -> String {
    let seconds = timeout_ms / 1_000;
    let remainder_ms = timeout_ms % 1_000;
    if remainder_ms == 0 {
        return format!("{seconds}s");
    }
    let fractional = format!("{remainder_ms:03}")
        .trim_end_matches('0')
        .to_string();
    format!("{seconds}.{fractional}s")
}

pub(crate) fn monitor_started_detail(task_id: &str, timeout_ms: u64, persistent: bool) -> String {
    if persistent {
        format!("Monitor started · task {task_id} · persistent")
    } else {
        format!(
            "Monitor started · task {task_id} · timeout {}",
            format_monitor_timeout(timeout_ms)
        )
    }
}

fn monitor_completed_detail(
    task_id: &str,
    status: &CommandExecutionStatus,
    termination_reason: Option<&CommandMonitorTerminationReason>,
    exit_code: Option<i32>,
) -> String {
    if let Some(reason) = termination_reason {
        return match reason {
            CommandMonitorTerminationReason::TimedOut => {
                format!("Monitor timed out · task {task_id}")
            }
            CommandMonitorTerminationReason::UserStopped
            | CommandMonitorTerminationReason::Stopped => {
                format!("Monitor stopped · task {task_id}")
            }
            CommandMonitorTerminationReason::SessionShutdown => {
                format!("Monitor stopped · task {task_id} · session ended")
            }
            CommandMonitorTerminationReason::Capacity => {
                format!("Monitor stopped · task {task_id} · capacity limit")
            }
        };
    }
    let status = match status {
        CommandExecutionStatus::InProgress => "running",
        CommandExecutionStatus::Completed => "completed",
        CommandExecutionStatus::Failed => "failed",
        CommandExecutionStatus::Declined => "declined",
    };
    match exit_code {
        Some(exit_code) => format!("Monitor {status} · task {task_id} · exit {exit_code}"),
        None => format!("Monitor {status} · task {task_id}"),
    }
}

fn monitor_header_spans(description: &str) -> Vec<Span<'static>> {
    let mut spans = vec!["Monitor".bold(), "(".into()];
    spans.extend(ansi_escape_line(description).spans);
    spans.push(")".into());
    spans
}

fn monitor_event_header_spans(description: &str) -> Vec<Span<'static>> {
    let mut spans = vec!["Monitor event: ".bold(), "\"".into()];
    spans.extend(ansi_escape_line(description).spans);
    spans.push("\"".into());
    spans
}

fn monitor_lines(
    width: u16,
    header_spans: Vec<Span<'static>>,
    detail: String,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }

    let width = width as usize;
    let mut lines = Vec::new();
    let header = Line::from(header_spans);
    let wrapped_header = adaptive_wrap_line(
        &header,
        RtOptions::new(width)
            .initial_indent(Line::from("⏺ ".cyan().bold()))
            .subsequent_indent(Line::from("  ")),
    );
    push_owned_lines(&wrapped_header, &mut lines);

    let mut detail_lines = detail.split('\n').peekable();
    let mut first_detail = true;
    while let Some(detail) = detail_lines.next() {
        if detail.is_empty() && detail_lines.peek().is_none() {
            break;
        }
        let detail = ansi_escape_line(detail).dim();
        let initial_indent = if first_detail {
            Line::from("  ⎿  ".dim())
        } else {
            Line::from("     ")
        };
        first_detail = false;
        let wrapped_detail = adaptive_wrap_line(
            &detail,
            RtOptions::new(width)
                .initial_indent(initial_indent)
                .subsequent_indent(Line::from("     ")),
        );
        push_owned_lines(&wrapped_detail, &mut lines);
    }
    lines
}
