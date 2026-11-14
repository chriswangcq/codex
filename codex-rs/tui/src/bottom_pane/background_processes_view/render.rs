//! Background-process list and detail presentation without request side effects.

use super::*;

impl BackgroundProcessesView {
    fn list_row(&self, idx: usize, item: &BackgroundProcessItem, width: u16) -> Line<'static> {
        let selected = self.state.selected_idx == Some(idx);
        let base_style = if selected {
            accent_style()
        } else {
            Style::default()
        };
        let prefix = if selected { "› " } else { "  " };
        let label = item.monitor.as_ref().map_or_else(
            || item.command_display.clone(),
            |monitor| monitor.description.clone(),
        );
        let stopping = matches!(
            &self.stop_state,
            StopState::InFlight { process_id } if process_id == &item.process_id
        );
        let suffix = if stopping {
            " (stopping…)"
        } else {
            " (running)"
        };
        let label_width = (width as usize).saturating_sub(prefix.width() + suffix.width());
        let label = truncate_line_with_ellipsis_if_overflow(ansi_escape_line(&label), label_width);
        let mut spans = vec![Span::styled(prefix, base_style)];
        spans.extend(label.spans.into_iter().map(|mut span| {
            span.style = span.style.patch(base_style);
            span
        }));
        spans.push(Span::styled(suffix, base_style.dim()));
        Line::from(spans)
    }

    fn list_height(&self) -> u16 {
        self.items.len().clamp(1, MAX_POPUP_ROWS) as u16
    }

    fn detail_metadata_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(item) = self.detail_idx.and_then(|idx| self.items.get(idx)) else {
            return Vec::new();
        };
        let title = if item.monitor.is_some() {
            "Monitor details"
        } else {
            "Shell details"
        };
        let command_label = if item.monitor.is_some() {
            "Script: "
        } else {
            "Command: "
        };
        let stopping = matches!(
            &self.stop_state,
            StopState::InFlight { process_id } if process_id == &item.process_id
        );
        let mut command_spans = vec![command_label.bold()];
        command_spans.extend(ansi_escape_line(&item.command_display).spans);
        let mut lines = vec![
            Line::from(title.bold()),
            Line::from(""),
            Line::from(vec![
                "Status: ".bold(),
                if stopping { "Stopping…" } else { "Running" }.into(),
            ]),
            Line::from(vec![
                "Runtime: ".bold(),
                format_runtime(item.started_at.elapsed()).into(),
            ]),
            Line::from(command_spans),
            Line::from(""),
            Line::from("Output:".bold()),
        ];
        if let StopState::Failed { message } = &self.stop_state {
            lines.insert(3, ansi_escape_line(message).red());
        }
        word_wrap_lines(lines, width.max(1) as usize)
    }

    pub(super) fn detail_output_state(&self) -> DetailOutputState {
        let Some(item) = self.detail_idx.and_then(|idx| self.items.get(idx)) else {
            return DetailOutputState::NoOutput;
        };
        if item.monitor.is_some() {
            return self.monitor_output_state.clone();
        }

        let tail = last_nonempty_lines(item.recent_output.iter().flat_map(|chunk| chunk.lines()));
        if tail.lines.is_empty() {
            DetailOutputState::NoOutput
        } else {
            DetailOutputState::Content(DetailOutputContent {
                lines: tail.lines,
                bytes_total: None,
                truncated: false,
                lines_omitted: tail.lines_omitted,
            })
        }
    }

    fn render_list(&self, area: Rect, buf: &mut Buffer) {
        let [content_area, footer_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        Block::default()
            .style(user_message_style())
            .render(content_area, buf);
        let inner = content_area.inset(Insets::vh(/*v*/ 1, /*h*/ 2));
        let [title_area, subtitle_area, _, rows_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);
        Line::from("Background".bold()).render(title_area, buf);
        let monitor_count = self
            .items
            .iter()
            .filter(|item| item.monitor.is_some())
            .count();
        let shell_count = self.items.len().saturating_sub(monitor_count);
        let counts_subtitle = match (shell_count, monitor_count) {
            (0, 0) => "No active processes".to_string(),
            (shells, 0) => format!(
                "{shells} active shell{}",
                if shells == 1 { "" } else { "s" }
            ),
            (0, monitors) => format!(
                "{monitors} active monitor{}",
                if monitors == 1 { "" } else { "s" }
            ),
            (shells, monitors) => format!(
                "{shells} shell{}, {monitors} monitor{} active",
                if shells == 1 { "" } else { "s" },
                if monitors == 1 { "" } else { "s" }
            ),
        };
        let subtitle = match &self.stop_state {
            StopState::Idle => Line::from(counts_subtitle).dim(),
            StopState::InFlight { .. } => Line::from("Stopping selected process…").dim(),
            StopState::Failed { message } => ansi_escape_line(message).red(),
        };
        truncate_line_with_ellipsis_if_overflow(subtitle, subtitle_area.width as usize)
            .render(subtitle_area, buf);
        if self.items.is_empty() {
            Line::from("No tasks currently running")
                .dim()
                .italic()
                .render(rows_area, buf);
        } else {
            for (offset, (idx, item)) in self
                .items
                .iter()
                .enumerate()
                .skip(self.state.scroll_top)
                .take(rows_area.height as usize)
                .enumerate()
            {
                self.list_row(idx, item, rows_area.width).render(
                    Rect::new(
                        rows_area.x,
                        rows_area.y.saturating_add(offset as u16),
                        rows_area.width,
                        1,
                    ),
                    buf,
                );
            }
        }
        let footer_area = footer_area.inset(Insets::vh(/*v*/ 0, /*h*/ 2));
        list_hint(footer_area.width).dim().render(footer_area, buf);
    }

    fn render_detail(&self, area: Rect, buf: &mut Buffer) {
        let output_state = self.detail_output_state();
        let has_content = output_state.has_content();
        let refresh_suspended = matches!(&output_state, DetailOutputState::Suspended(Some(_)));
        let footer_height = u16::from(has_content);
        let [content_area, footer_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(footer_height)]).areas(area);
        Block::default()
            .style(user_message_style())
            .render(content_area, buf);
        let inner = content_area.inset(Insets::vh(/*v*/ 1, /*h*/ 2));
        let metadata = self.detail_metadata_lines(inner.width);
        let metadata_height = metadata.len() as u16;
        let output_height = if has_content {
            MONITOR_OUTPUT_FRAME_HEIGHT
        } else {
            1
        };
        let [metadata_area, output_area] = Layout::vertical([
            Constraint::Length(metadata_height),
            Constraint::Length(output_height),
        ])
        .areas(inner);
        for (offset, line) in metadata.into_iter().enumerate() {
            line.render(
                Rect::new(
                    metadata_area.x,
                    metadata_area.y.saturating_add(offset as u16),
                    metadata_area.width,
                    1,
                ),
                buf,
            );
        }
        match output_state {
            DetailOutputState::Loading => {
                Line::from("Loading output…")
                    .dim()
                    .italic()
                    .render(output_area, buf);
            }
            DetailOutputState::NoOutput => {
                Line::from("No output available")
                    .dim()
                    .italic()
                    .render(output_area, buf);
            }
            DetailOutputState::Suspended(None) => {
                Line::from("Output unavailable · refresh paused; reopen to retry")
                    .dim()
                    .italic()
                    .render(output_area, buf);
            }
            DetailOutputState::Content(DetailOutputContent {
                lines,
                bytes_total,
                truncated,
                lines_omitted,
            })
            | DetailOutputState::Suspended(Some(DetailOutputContent {
                lines,
                bytes_total,
                truncated,
                lines_omitted,
            })) => {
                let output_area = Rect::new(
                    area.x.saturating_add(3),
                    output_area.y,
                    area.width.saturating_sub(6),
                    output_area.height,
                );
                let output_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .padding(Padding::horizontal(1));
                let output_inner = output_block.inner(output_area);
                output_block.render(output_area, buf);
                let line_count = lines.len().min(output_inner.height as usize);
                for (offset, line) in lines.into_iter().take(line_count).enumerate() {
                    truncate_line_with_ellipsis_if_overflow(
                        ansi_escape_line(&line),
                        output_inner.width as usize,
                    )
                    .render(
                        Rect::new(
                            output_inner.x,
                            output_inner.y.saturating_add(offset as u16),
                            output_inner.width,
                            1,
                        ),
                        buf,
                    );
                }
                let mut footer = if truncated {
                    format!(
                        "Showing {line_count} lines of {}",
                        format_bytes(bytes_total.unwrap_or_default())
                    )
                } else if lines_omitted {
                    format!("Showing last {line_count} lines · earlier lines hidden")
                } else {
                    format!("Showing {line_count} lines")
                };
                if refresh_suspended {
                    footer.push_str(" · stale; refresh paused; reopen to retry");
                }
                Line::from(footer)
                    .dim()
                    .italic()
                    .render(footer_area.inset(Insets::vh(/*v*/ 0, /*h*/ 2)), buf);
            }
        }
    }
}

impl Renderable for BackgroundProcessesView {
    fn desired_height(&self, width: u16) -> u16 {
        if self.detail_idx.is_some() {
            let content_width = width.saturating_sub(4);
            let output_height = if self.detail_output_state().has_content() {
                15
            } else {
                3
            };
            return (self.detail_metadata_lines(content_width).len() as u16)
                .saturating_add(output_height);
        }
        self.list_height().saturating_add(6)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        if self.detail_idx.is_some() {
            self.render_detail(area, buf);
        } else {
            self.render_list(area, buf);
        }
    }
}

fn format_runtime(runtime: Duration) -> String {
    crate::status_indicator_widget::fmt_elapsed_compact(runtime.as_secs())
}

pub(super) fn ansi_line_text(text: &str) -> String {
    ansi_escape_line(text)
        .spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
}

pub(super) fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }

    const UNITS: [(&str, u64); 3] = [
        ("GB", 1024 * 1024 * 1024),
        ("MB", 1024 * 1024),
        ("KB", 1024),
    ];
    let (unit, divisor) = UNITS
        .into_iter()
        .find(|(_, divisor)| bytes >= *divisor)
        .unwrap_or(("KB", 1024));
    let mut value = format!("{:.1}", bytes as f64 / divisor as f64);
    if value.ends_with(".0") {
        value.truncate(value.len().saturating_sub(2));
    }
    format!("{value}{unit}")
}

fn list_hint(width: u16) -> Line<'static> {
    const FULL: &str = "↑/↓ select · Enter view · x stop · Esc close";
    const COMPACT: &str = "↑/↓ · Enter · x stop · Esc";
    if FULL.width() <= width as usize {
        Line::from(FULL)
    } else {
        Line::from(COMPACT)
    }
}
