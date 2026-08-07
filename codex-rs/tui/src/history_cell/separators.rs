//! Turn separators and runtime-metrics labels for transcript history.

use super::*;

#[derive(Debug)]
/// A visual divider between turns, optionally showing how long the assistant "worked for".
///
/// This separator is only emitted for turns that performed concrete work (e.g., running commands,
/// applying patches, making MCP tool calls), so purely conversational turns do not show an empty
/// divider.
pub struct FinalMessageSeparator {
    elapsed_seconds: Option<u64>,
    runtime_metrics: Option<RuntimeMetricsSummary>,
    background_process_summary: Option<String>,
    monitor_count: usize,
    monitor_duration_verb: &'static str,
}
impl FinalMessageSeparator {
    /// Creates a separator; completed turns should pass protocol turn duration when available.
    pub(crate) fn new(
        elapsed_seconds: Option<u64>,
        runtime_metrics: Option<RuntimeMetricsSummary>,
    ) -> Self {
        Self {
            elapsed_seconds,
            runtime_metrics,
            background_process_summary: None,
            monitor_count: 0,
            monitor_duration_verb: random_monitor_duration_verb(),
        }
    }

    /// Appends the active background-process count to this turn's existing footer label.
    pub(crate) fn with_background_processes(
        mut self,
        shell_count: usize,
        monitor_count: usize,
    ) -> Self {
        self.background_process_summary = background_process_summary(shell_count, monitor_count);
        self.monitor_count = monitor_count;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_monitor_duration_verb(mut self, verb: &'static str) -> Self {
        self.monitor_duration_verb = verb;
        self
    }
}
impl HistoryCell for FinalMessageSeparator {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut label_parts = Vec::new();
        if let Some(elapsed_seconds) = self
            .elapsed_seconds
            .map(crate::status_indicator_widget::fmt_elapsed_compact)
        {
            if self.monitor_count > 0 {
                label_parts.push(format!(
                    "{} for {elapsed_seconds}",
                    self.monitor_duration_verb
                ));
            } else if self.elapsed_seconds.is_some_and(|seconds| seconds > 60) {
                label_parts.push(format!("Worked for {elapsed_seconds}"));
            }
        }
        if let Some(metrics_label) = self.runtime_metrics.and_then(runtime_metrics_label) {
            label_parts.push(metrics_label);
        }

        let label_text = footer_label(&label_parts, self.background_process_summary.as_deref());
        if label_text.is_empty() {
            return vec![Line::from_iter(["─".repeat(width as usize).dim()])];
        }

        let label = format!("─ {label_text} ─");
        let (label, _suffix, label_width) = take_prefix_by_width(&label, width as usize);
        vec![
            Line::from_iter([
                label,
                "─".repeat((width as usize).saturating_sub(label_width)),
            ])
            .dim(),
        ]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut label_parts = Vec::new();
        if let Some(elapsed_seconds) = self
            .elapsed_seconds
            .map(crate::status_indicator_widget::fmt_elapsed_compact)
        {
            if self.monitor_count > 0 {
                label_parts.push(format!(
                    "{} for {elapsed_seconds}",
                    self.monitor_duration_verb
                ));
            } else if self.elapsed_seconds.is_some_and(|seconds| seconds > 60) {
                label_parts.push(format!("Worked for {elapsed_seconds}"));
            }
        }
        if let Some(metrics_label) = self.runtime_metrics.and_then(runtime_metrics_label) {
            label_parts.push(metrics_label);
        }
        let label = footer_label(&label_parts, self.background_process_summary.as_deref());
        if label.is_empty() {
            Vec::new()
        } else {
            vec![Line::from(label)]
        }
    }
}

#[cfg(not(test))]
fn random_monitor_duration_verb() -> &'static str {
    use rand::Rng as _;

    const VERBS: [&str; 8] = [
        "Baked",
        "Brewed",
        "Churned",
        "Cogitated",
        "Cooked",
        "Crunched",
        "Sautéed",
        "Worked",
    ];
    VERBS[rand::rng().random_range(0..VERBS.len())]
}

#[cfg(test)]
fn random_monitor_duration_verb() -> &'static str {
    "Baked"
}

fn footer_label(label_parts: &[String], background_process_summary: Option<&str>) -> String {
    let mut label = label_parts.join(" • ");
    if let Some(summary) = background_process_summary {
        if !label.is_empty() {
            label.push_str(" · ");
        }
        label.push_str(summary);
    }
    label
}

fn background_process_summary(shell_count: usize, monitor_count: usize) -> Option<String> {
    let shells = (shell_count > 0).then(|| {
        let suffix = if shell_count == 1 { "" } else { "s" };
        format!("{shell_count} shell{suffix}")
    });
    let monitors = (monitor_count > 0).then(|| {
        let suffix = if monitor_count == 1 { "" } else { "s" };
        format!("{monitor_count} monitor{suffix}")
    });
    match (shells, monitors) {
        (Some(shells), Some(monitors)) => Some(format!("{shells}, {monitors} still running")),
        (Some(shells), None) => Some(format!("{shells} still running")),
        (None, Some(monitors)) => Some(format!("{monitors} still running")),
        (None, None) => None,
    }
}

pub(crate) fn runtime_metrics_label(summary: RuntimeMetricsSummary) -> Option<String> {
    let mut parts = Vec::new();
    if summary.tool_calls.count > 0 {
        let duration = format_duration_ms(summary.tool_calls.duration_ms);
        let calls = pluralize(summary.tool_calls.count, "call", "calls");
        parts.push(format!(
            "Local tools: {} {calls} ({duration})",
            summary.tool_calls.count
        ));
    }
    if summary.api_calls.count > 0 {
        let duration = format_duration_ms(summary.api_calls.duration_ms);
        let calls = pluralize(summary.api_calls.count, "call", "calls");
        parts.push(format!(
            "Inference: {} {calls} ({duration})",
            summary.api_calls.count
        ));
    }
    if summary.websocket_calls.count > 0 {
        let duration = format_duration_ms(summary.websocket_calls.duration_ms);
        parts.push(format!(
            "WebSocket: {} events send ({duration})",
            summary.websocket_calls.count
        ));
    }
    if summary.streaming_events.count > 0 {
        let duration = format_duration_ms(summary.streaming_events.duration_ms);
        let stream_label = pluralize(summary.streaming_events.count, "Stream", "Streams");
        let events = pluralize(summary.streaming_events.count, "event", "events");
        parts.push(format!(
            "{stream_label}: {} {events} ({duration})",
            summary.streaming_events.count
        ));
    }
    if summary.websocket_events.count > 0 {
        let duration = format_duration_ms(summary.websocket_events.duration_ms);
        parts.push(format!(
            "{} events received ({duration})",
            summary.websocket_events.count
        ));
    }
    if summary.responses_api_overhead_ms > 0 {
        let duration = format_duration_ms(summary.responses_api_overhead_ms);
        parts.push(format!("Responses API overhead: {duration}"));
    }
    if summary.responses_api_inference_time_ms > 0 {
        let duration = format_duration_ms(summary.responses_api_inference_time_ms);
        parts.push(format!("Responses API inference: {duration}"));
    }
    if summary.responses_api_engine_iapi_ttft_ms > 0
        || summary.responses_api_engine_service_ttft_ms > 0
    {
        let mut ttft_parts = Vec::new();
        if summary.responses_api_engine_iapi_ttft_ms > 0 {
            let duration = format_duration_ms(summary.responses_api_engine_iapi_ttft_ms);
            ttft_parts.push(format!("{duration} (iapi)"));
        }
        if summary.responses_api_engine_service_ttft_ms > 0 {
            let duration = format_duration_ms(summary.responses_api_engine_service_ttft_ms);
            ttft_parts.push(format!("{duration} (service)"));
        }
        parts.push(format!("TTFT: {}", ttft_parts.join(" ")));
    }
    if summary.responses_api_engine_iapi_tbt_ms > 0.0
        || summary.responses_api_engine_service_tbt_ms > 0.0
    {
        let mut tbt_parts = Vec::new();
        if summary.responses_api_engine_iapi_tbt_ms > 0.0 {
            let duration =
                format_duration_ms(summary.responses_api_engine_iapi_tbt_ms.round() as u64);
            tbt_parts.push(format!("{duration} (iapi)"));
        }
        if summary.responses_api_engine_service_tbt_ms > 0.0 {
            let duration =
                format_duration_ms(summary.responses_api_engine_service_tbt_ms.round() as u64);
            tbt_parts.push(format!("{duration} (service)"));
        }
        parts.push(format!("TBT: {}", tbt_parts.join(" ")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" • "))
    }
}

fn format_duration_ms(duration_ms: u64) -> String {
    if duration_ms >= 1_000 {
        let seconds = duration_ms as f64 / 1_000.0;
        format!("{seconds:.1}s")
    } else {
        format!("{duration_ms}ms")
    }
}

fn pluralize(count: u64, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}
