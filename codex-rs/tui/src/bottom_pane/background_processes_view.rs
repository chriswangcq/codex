//! Interactive manager for background shells and command monitors.

use std::time::Duration;
use std::time::Instant;

use codex_ansi_escape::ansi_escape_line;
use codex_app_server_protocol::CommandMonitorInfo;
use codex_app_server_protocol::ThreadBackgroundTerminalOutput;
use codex_protocol::ThreadId;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Padding;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use crate::app_command::AppCommand;
use crate::app_event::AppEvent;
use crate::app_event::BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT;
use crate::app_event::BackgroundMonitorOutputRequest;
use crate::app_event::BackgroundMonitorOutputResponse;
use crate::app_event_sender::AppEventSender;
use crate::key_hint::KeyBindingListExt;
use crate::keymap::KeymapContext;
use crate::keymap::KeymapContextSet;
use crate::keymap::ListKeymap;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::render::Insets;
use crate::render::RectExt as _;
use crate::render::renderable::Renderable;
use crate::style::accent_style;
use crate::style::user_message_style;
use crate::wrapping::word_wrap_lines;

use super::CancellationEvent;
use super::ViewCompletion;
use super::bottom_pane_view::BottomPaneView;
use super::popup_consts::MAX_POPUP_ROWS;
use super::scroll_state::ScrollState;

const BACKGROUND_PROCESSES_VIEW_ID: &str = "background-processes";
const MONITOR_OUTPUT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MONITOR_OUTPUT_MAX_LINES: usize = 10;
const MONITOR_OUTPUT_FRAME_HEIGHT: u16 = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitorOutputTarget {
    thread_id: ThreadId,
    process_id: String,
    task_id: String,
}

impl MonitorOutputTarget {
    fn request(&self, generation: Uuid) -> BackgroundMonitorOutputRequest {
        BackgroundMonitorOutputRequest {
            thread_id: self.thread_id,
            process_id: self.process_id.clone(),
            task_id: self.task_id.clone(),
            generation,
        }
    }

    fn matches_request(&self, request: &BackgroundMonitorOutputRequest) -> bool {
        self.thread_id == request.thread_id
            && self.process_id == request.process_id
            && self.task_id == request.task_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MonitorPollLifecycle {
    Idle,
    InFlight {
        request: BackgroundMonitorOutputRequest,
        started_at: Instant,
    },
    /// The current target's request failed or expired. A target change explicitly resumes polling.
    /// Do not automatically retry: a timed-out remote RPC can remain transport-pending until the
    /// connection responds or closes.
    Suspended,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DetailOutputContent {
    lines: Vec<String>,
    bytes_total: Option<u64>,
    truncated: bool,
    lines_omitted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DetailOutputState {
    Loading,
    NoOutput,
    Content(DetailOutputContent),
    /// Automatic refresh stopped after the current target's RPC failed or timed out. Preserve the
    /// last successful snapshot, when one exists, but label it as stale in the UI.
    Suspended(Option<DetailOutputContent>),
}

impl DetailOutputState {
    fn suspend(&mut self) -> bool {
        let content = match self {
            Self::Content(content) => Some(content.clone()),
            Self::Suspended(_) => return false,
            Self::Loading | Self::NoOutput => None,
        };
        *self = Self::Suspended(content);
        true
    }

    fn has_content(&self) -> bool {
        matches!(self, Self::Content(_) | Self::Suspended(Some(_)))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum StopState {
    #[default]
    Idle,
    InFlight {
        process_id: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct BackgroundProcessItem {
    pub(crate) process_id: String,
    pub(crate) command_display: String,
    pub(crate) recent_output: Vec<String>,
    pub(crate) monitor: Option<CommandMonitorInfo>,
    pub(crate) started_at: Instant,
}

pub(crate) struct BackgroundProcessesView {
    items: Vec<BackgroundProcessItem>,
    thread_id: Option<ThreadId>,
    state: ScrollState,
    detail_idx: Option<usize>,
    monitor_output_target: Option<MonitorOutputTarget>,
    monitor_output_state: DetailOutputState,
    monitor_poll_lifecycle: MonitorPollLifecycle,
    monitor_output_generation: Option<Uuid>,
    monitor_output_last_started_at: Option<Instant>,
    stop_state: StopState,
    completion: Option<ViewCompletion>,
    app_event_tx: AppEventSender,
    keymap: ListKeymap,
}

impl BackgroundProcessesView {
    pub(crate) fn new(
        items: Vec<BackgroundProcessItem>,
        thread_id: Option<ThreadId>,
        app_event_tx: AppEventSender,
        keymap: ListKeymap,
    ) -> Self {
        Self::new_at(items, thread_id, app_event_tx, keymap, Instant::now())
    }

    fn new_at(
        items: Vec<BackgroundProcessItem>,
        thread_id: Option<ThreadId>,
        app_event_tx: AppEventSender,
        keymap: ListKeymap,
        now: Instant,
    ) -> Self {
        let items = normalize_items(items);
        let detail_idx = (items.len() == 1).then_some(0);
        let mut state = ScrollState::new();
        state.selected_idx = (!items.is_empty()).then_some(0);
        let mut view = Self {
            items,
            thread_id,
            state,
            detail_idx,
            monitor_output_target: None,
            monitor_output_state: DetailOutputState::Loading,
            monitor_poll_lifecycle: MonitorPollLifecycle::Idle,
            monitor_output_generation: None,
            monitor_output_last_started_at: None,
            stop_state: StopState::Idle,
            completion: None,
            app_event_tx,
            keymap,
        };
        view.sync_monitor_output_target_at(now);
        view
    }

    fn selected_idx(&self) -> Option<usize> {
        self.state
            .selected_idx
            .filter(|idx| *idx < self.items.len())
    }

    fn move_up(&mut self) {
        let len = self.items.len();
        self.state.move_up_wrap(len);
        self.state
            .ensure_visible(len, MAX_POPUP_ROWS.min(len.max(1)));
    }

    fn move_down(&mut self) {
        let len = self.items.len();
        self.state.move_down_wrap(len);
        self.state
            .ensure_visible(len, MAX_POPUP_ROWS.min(len.max(1)));
    }

    fn open_selected(&mut self) {
        self.open_selected_at(Instant::now());
    }

    fn open_selected_at(&mut self, now: Instant) {
        self.detail_idx = self.selected_idx();
        self.sync_monitor_output_target_at(now);
    }

    fn close(&mut self) {
        self.completion = Some(ViewCompletion::Cancelled);
    }

    fn go_back(&mut self) {
        if self.items.len() == 1 {
            self.close();
        } else {
            self.detail_idx = None;
            self.sync_monitor_output_target_at(Instant::now());
        }
    }

    fn selected_monitor_output_target(&self) -> Option<MonitorOutputTarget> {
        let item = self.detail_idx.and_then(|idx| self.items.get(idx))?;
        let monitor = item.monitor.as_ref()?;
        Some(MonitorOutputTarget {
            thread_id: self.thread_id?,
            process_id: item.process_id.clone(),
            task_id: monitor.task_id.clone(),
        })
    }

    fn sync_monitor_output_target_at(&mut self, now: Instant) -> bool {
        let target = self.selected_monitor_output_target();
        if target != self.monitor_output_target {
            self.monitor_output_target = target;
            self.monitor_output_state = DetailOutputState::Loading;
            self.monitor_poll_lifecycle = MonitorPollLifecycle::Idle;
            self.monitor_output_generation = None;
            self.monitor_output_last_started_at = None;
        }
        self.maybe_request_monitor_output_at(now)
    }

    fn maybe_request_monitor_output_at(&mut self, now: Instant) -> bool {
        match &self.monitor_poll_lifecycle {
            MonitorPollLifecycle::Idle => {}
            MonitorPollLifecycle::InFlight { started_at, .. } => {
                if now.saturating_duration_since(*started_at)
                    < BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT
                {
                    return false;
                }
                self.monitor_poll_lifecycle = MonitorPollLifecycle::Suspended;
                return self.monitor_output_state.suspend();
            }
            MonitorPollLifecycle::Suspended => return false,
        }
        let Some(target) = self.monitor_output_target.as_ref() else {
            return false;
        };
        if self
            .monitor_output_last_started_at
            .is_some_and(|started_at| {
                now.saturating_duration_since(started_at) < MONITOR_OUTPUT_POLL_INTERVAL
            })
        {
            return false;
        }

        let generation = Uuid::new_v4();
        let request = target.request(generation);
        self.monitor_output_generation = Some(generation);
        self.monitor_output_last_started_at = Some(now);
        self.monitor_poll_lifecycle = MonitorPollLifecycle::InFlight {
            request: request.clone(),
            started_at: now,
        };
        self.app_event_tx
            .send(AppEvent::FetchBackgroundMonitorOutput(request));
        true
    }

    fn apply_monitor_output_response_at(
        &mut self,
        response: BackgroundMonitorOutputResponse,
        now: Instant,
    ) -> bool {
        let BackgroundMonitorOutputResponse { request, result } = response;
        let matches_in_flight = matches!(
            &self.monitor_poll_lifecycle,
            MonitorPollLifecycle::InFlight { request: in_flight, .. } if in_flight == &request
        );
        if !matches_in_flight {
            return false;
        }

        let matches_current_target = self
            .monitor_output_target
            .as_ref()
            .is_some_and(|target| target.matches_request(&request))
            && self.monitor_output_generation == Some(request.generation);
        match result {
            Ok(output) => {
                self.monitor_poll_lifecycle = MonitorPollLifecycle::Idle;
                let mut changed = false;
                if matches_current_target {
                    let next_state = detail_output_state_from_snapshot(output);
                    changed = self.monitor_output_state != next_state;
                    self.monitor_output_state = next_state;
                }
                self.maybe_request_monitor_output_at(now) || changed
            }
            Err(err) => {
                self.monitor_poll_lifecycle = MonitorPollLifecycle::Suspended;
                if matches_current_target {
                    tracing::warn!(
                        thread_id = %request.thread_id,
                        process_id = %request.process_id,
                        task_id = %request.task_id,
                        generation = %request.generation,
                        error = %err,
                        "failed to refresh command monitor output"
                    );
                    return self.monitor_output_state.suspend();
                }
                false
            }
        }
    }

    fn stop_selected(&mut self) {
        if matches!(&self.stop_state, StopState::InFlight { .. }) {
            return;
        }
        let idx = self.detail_idx.or_else(|| self.selected_idx());
        let Some(item) = idx.and_then(|idx| self.items.get(idx)) else {
            return;
        };
        let process_id = item.process_id.clone();
        self.stop_state = StopState::InFlight {
            process_id: process_id.clone(),
        };
        self.app_event_tx
            .send(AppEvent::CodexOp(AppCommand::terminate_background_process(
                process_id,
            )));
    }

    fn apply_stop_response(&mut self, process_id: &str, result: Result<bool, String>) -> bool {
        if !matches!(
            &self.stop_state,
            StopState::InFlight {
                process_id: pending_process_id
            } if pending_process_id == process_id
        ) {
            return false;
        }

        match result {
            Ok(true) => {
                self.completion = Some(ViewCompletion::Accepted);
            }
            Ok(false) => {
                self.stop_state = StopState::Failed {
                    message: "Process still running · press x to retry".to_string(),
                };
            }
            Err(err) => {
                let err = ansi_line_text(&err);
                self.stop_state = StopState::Failed {
                    message: format!("Stop failed: {err}. Press x to retry."),
                };
            }
        }
        true
    }

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

    fn detail_output_state(&self) -> DetailOutputState {
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

impl BottomPaneView for BackgroundProcessesView {
    fn keymap_contexts(&self) -> KeymapContextSet {
        KeymapContextSet::new(KeymapContext::List)
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Release {
            return;
        }
        if matches!(
            key_event,
            KeyEvent {
                code: KeyCode::Char('x'),
                modifiers: KeyModifiers::NONE,
                ..
            }
        ) {
            self.stop_selected();
            return;
        }
        if self.detail_idx.is_some() {
            if key_event.code == KeyCode::Left || self.keymap.move_left.is_pressed(key_event) {
                self.go_back();
            } else if matches!(
                key_event.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ')
            ) || self.keymap.cancel.is_pressed(key_event)
            {
                self.close();
            }
            return;
        }
        if key_event.code == KeyCode::Esc || self.keymap.cancel.is_pressed(key_event) {
            self.close();
            return;
        }
        if self.keymap.move_up.is_pressed(key_event) {
            self.move_up();
        } else if self.keymap.move_down.is_pressed(key_event) {
            self.move_down();
        } else if self.keymap.accept.is_pressed(key_event) {
            self.open_selected();
        }
    }

    fn is_complete(&self) -> bool {
        self.completion.is_some()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.completion
    }

    fn selected_index(&self) -> Option<usize> {
        self.detail_idx.or_else(|| self.selected_idx())
    }

    fn view_id(&self) -> Option<&'static str> {
        Some(BACKGROUND_PROCESSES_VIEW_ID)
    }

    fn update_background_processes(&mut self, items: Vec<BackgroundProcessItem>) -> bool {
        let selected_process_id = self
            .selected_idx()
            .and_then(|idx| self.items.get(idx))
            .map(|item| item.process_id.clone());
        let detail_process_id = self
            .detail_idx
            .and_then(|idx| self.items.get(idx))
            .map(|item| item.process_id.clone());
        self.items = normalize_items(items);
        if self.items.is_empty() {
            self.close();
            return true;
        }
        if let Some(detail_process_id) = detail_process_id {
            let Some(idx) = self
                .items
                .iter()
                .position(|item| item.process_id == detail_process_id)
            else {
                self.close();
                return true;
            };
            self.detail_idx = Some(idx);
            self.state.selected_idx = Some(idx);
            self.sync_monitor_output_target_at(Instant::now());
            return true;
        }
        if self.items.len() == 1 {
            self.detail_idx = Some(0);
            self.state.selected_idx = Some(0);
            self.sync_monitor_output_target_at(Instant::now());
            return true;
        }
        let selected_idx = selected_process_id
            .and_then(|process_id| {
                self.items
                    .iter()
                    .position(|item| item.process_id == process_id)
            })
            .unwrap_or(0);
        self.state.selected_idx = Some(selected_idx);
        self.state
            .ensure_visible(self.items.len(), MAX_POPUP_ROWS.min(self.items.len()));
        self.sync_monitor_output_target_at(Instant::now());
        true
    }

    fn update_background_monitor_output(
        &mut self,
        response: BackgroundMonitorOutputResponse,
    ) -> bool {
        self.apply_monitor_output_response_at(response, Instant::now())
    }

    fn update_background_process_stop(
        &mut self,
        process_id: &str,
        result: Result<bool, String>,
    ) -> bool {
        self.apply_stop_response(process_id, result)
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completion = Some(ViewCompletion::Cancelled);
        CancellationEvent::Handled
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn pre_draw_tick(&mut self, now: Instant) -> bool {
        self.sync_monitor_output_target_at(now)
    }

    fn next_frame_delay(&self) -> Option<Duration> {
        self.detail_idx
            .is_some()
            .then_some(MONITOR_OUTPUT_POLL_INTERVAL)
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

fn ansi_line_text(text: &str) -> String {
    ansi_escape_line(text)
        .spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
}

fn detail_output_state_from_snapshot(
    output: Option<ThreadBackgroundTerminalOutput>,
) -> DetailOutputState {
    let Some(output) = output else {
        return DetailOutputState::NoOutput;
    };
    let tail = last_nonempty_lines(output.tail.lines());
    if tail.lines.is_empty() {
        DetailOutputState::NoOutput
    } else {
        DetailOutputState::Content(DetailOutputContent {
            lines: tail.lines,
            bytes_total: Some(output.bytes_total),
            truncated: output.truncated,
            lines_omitted: tail.lines_omitted,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct LastNonemptyLines {
    lines: Vec<String>,
    lines_omitted: bool,
}

fn last_nonempty_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> LastNonemptyLines {
    let lines = lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let lines_omitted = lines.len() > MONITOR_OUTPUT_MAX_LINES;
    let lines = lines
        .into_iter()
        .rev()
        .take(MONITOR_OUTPUT_MAX_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    LastNonemptyLines {
        lines,
        lines_omitted,
    }
}

fn format_bytes(bytes: u64) -> String {
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

fn normalize_items(items: Vec<BackgroundProcessItem>) -> Vec<BackgroundProcessItem> {
    items
        .into_iter()
        .map(|mut item| {
            item.command_display = truncate_utf16_units(&item.command_display, 280);
            item
        })
        .collect()
}

fn truncate_utf16_units(value: &str, max_units: usize) -> String {
    let mut units = 0;
    value
        .chars()
        .take_while(|ch| {
            let next = units + ch.len_utf16();
            if next > max_units {
                return false;
            }
            units = next;
            true
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::sync::mpsc::unbounded_channel;

    fn thread_id() -> ThreadId {
        ThreadId::from_string("11111111-1111-4111-8111-111111111111").expect("thread id")
    }

    fn monitor_item_at(now: Instant) -> BackgroundProcessItem {
        BackgroundProcessItem {
            process_id: "401".to_string(),
            command_display: "tail -f app.log".to_string(),
            recent_output: vec!["ready".to_string(), "healthy".to_string()],
            monitor: Some(CommandMonitorInfo {
                task_id: "task-a1".to_string(),
                description: "deployment readiness".to_string(),
                timeout_ms: 300_000,
                persistent: false,
            }),
            started_at: now
                .checked_sub(Duration::from_secs(12))
                .expect("test instant"),
        }
    }

    fn monitor_item() -> BackgroundProcessItem {
        monitor_item_at(Instant::now())
    }

    fn second_monitor_at(now: Instant) -> BackgroundProcessItem {
        let mut item = monitor_item_at(now);
        item.process_id = "403".to_string();
        item.command_display = "watch second.log".to_string();
        let monitor = item.monitor.as_mut().expect("monitor");
        monitor.task_id = "task-b2".to_string();
        monitor.description = "second monitor".to_string();
        item
    }

    fn shell_item_at(now: Instant) -> BackgroundProcessItem {
        BackgroundProcessItem {
            process_id: "402".to_string(),
            command_display: "npm run dev".to_string(),
            recent_output: Vec::new(),
            monitor: None,
            started_at: now
                .checked_sub(Duration::from_secs(4))
                .expect("test instant"),
        }
    }

    fn shell_item() -> BackgroundProcessItem {
        shell_item_at(Instant::now())
    }

    fn view_with_rx_at(
        items: Vec<BackgroundProcessItem>,
        now: Instant,
    ) -> (BackgroundProcessesView, UnboundedReceiver<AppEvent>) {
        let (tx, rx) = unbounded_channel();
        (
            BackgroundProcessesView::new_at(
                items,
                Some(thread_id()),
                AppEventSender::new(tx),
                crate::keymap::RuntimeKeymap::defaults().list,
                now,
            ),
            rx,
        )
    }

    fn view(items: Vec<BackgroundProcessItem>) -> BackgroundProcessesView {
        let (tx, _rx) = unbounded_channel();
        BackgroundProcessesView::new(
            items,
            Some(thread_id()),
            AppEventSender::new(tx),
            crate::keymap::RuntimeKeymap::defaults().list,
        )
    }

    fn next_monitor_request(
        rx: &mut UnboundedReceiver<AppEvent>,
    ) -> BackgroundMonitorOutputRequest {
        let event = rx.try_recv().expect("monitor output request");
        let AppEvent::FetchBackgroundMonitorOutput(request) = event else {
            panic!("expected monitor output request");
        };
        request
    }

    fn response(
        request: BackgroundMonitorOutputRequest,
        tail: &str,
        bytes_total: u64,
        truncated: bool,
    ) -> BackgroundMonitorOutputResponse {
        BackgroundMonitorOutputResponse {
            request,
            result: Ok(Some(ThreadBackgroundTerminalOutput {
                tail: tail.to_string(),
                bytes_total,
                truncated,
            })),
        }
    }

    fn monitor_view_with_output(
        tail: &str,
        bytes_total: u64,
        truncated: bool,
    ) -> BackgroundProcessesView {
        let now = Instant::now();
        let (mut view, mut rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let request = next_monitor_request(&mut rx);
        assert!(view.apply_monitor_output_response_at(
            response(request, tail, bytes_total, truncated),
            now + Duration::from_millis(100),
        ));
        view
    }

    fn render_view(view: &BackgroundProcessesView, width: u16) -> String {
        let height = view.desired_height(width);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        (0..height)
            .map(|row| {
                let mut line = String::new();
                for col in 0..width {
                    line.push_str(buf[(col, row)].symbol());
                }
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn empty_snapshot() {
        assert_snapshot!(render_view(&view(Vec::new()), 70));
    }

    #[test]
    fn list_snapshot() {
        assert_snapshot!(render_view(&view(vec![monitor_item(), shell_item()]), 80));
    }

    #[test]
    fn one_monitor_opens_detail_snapshot() {
        assert_snapshot!(render_view(
            &monitor_view_with_output("ready\r\nhealthy\r\n", 16, false),
            70
        ));
    }

    #[test]
    fn monitor_loading_output_snapshot() {
        let mut monitor = monitor_item();
        monitor.recent_output.clear();
        assert_snapshot!(render_view(&view(vec![monitor]), 70));
    }

    #[test]
    fn monitor_no_output_after_loading_snapshot() {
        let now = Instant::now();
        let (mut view, mut rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let request = next_monitor_request(&mut rx);
        assert!(view.apply_monitor_output_response_at(
            response(request, "\r\n\r\n", 4, false),
            now + Duration::from_millis(100),
        ));
        assert_snapshot!(render_view(&view, 70));
    }

    #[test]
    fn monitor_truncated_output_snapshot() {
        assert_snapshot!(render_view(
            &monitor_view_with_output("first\nsecond\nthird\n", 1536, true),
            70
        ));
    }

    #[test]
    fn shell_no_output_detail_snapshot() {
        let view = view(vec![shell_item()]);
        assert_snapshot!(render_view(&view, 70));
    }

    #[test]
    fn long_output_lines_are_truncated_without_wrapping() {
        let tail = format!("{}\nstill visible", "x".repeat(200));
        let rendered = render_view(&monitor_view_with_output(&tail, 220, false), 70);

        assert!(rendered.contains("still visible"));
        assert_eq!(rendered.matches("still visible").count(), 1);
        assert!(rendered.contains('…'));
    }

    #[test]
    fn x_waits_for_confirmed_stop_and_keeps_failures_actionable() {
        let (tx, mut rx) = unbounded_channel();
        let mut view = BackgroundProcessesView::new(
            vec![monitor_item(), shell_item()],
            Some(thread_id()),
            AppEventSender::new(tx),
            crate::keymap::RuntimeKeymap::defaults().list,
        );
        view.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        view.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));

        assert_eq!(view.completion(), None);
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::CodexOp(AppCommand::TerminateBackgroundProcess { process_id }))
                if process_id == "402"
        ));
        view.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(rx.try_recv().is_err(), "duplicate stop must be suppressed");

        assert!(view.apply_stop_response("402", Ok(false)));
        assert!(!view.is_complete());
        let rendered = render_view(&view, 44);
        assert!(rendered.contains("still running"));
        assert!(rendered.contains("retry"));
        assert_snapshot!("stop_failure_keeps_manager_open_snapshot", rendered);

        view.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::CodexOp(AppCommand::TerminateBackgroundProcess { process_id }))
                if process_id == "402"
        ));
        assert!(view.apply_stop_response("402", Err("connection lost".to_string())));
        assert!(!view.is_complete());
        let rendered = render_view(&view, 70);
        assert!(rendered.contains("Stop failed: connection lost"));
        assert!(rendered.contains("Press x to retry"));

        view.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let _ = rx.try_recv();
        assert!(view.apply_stop_response("402", Ok(true)));
        assert_eq!(view.completion(), Some(ViewCompletion::Accepted));
    }

    #[test]
    fn shell_local_line_truncation_is_explicit_in_footer() {
        let mut item = shell_item();
        item.recent_output = vec![
            (1..=12)
                .map(|line| format!("line{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ];
        let view = view(vec![item]);
        let DetailOutputState::Content(content) = view.detail_output_state() else {
            panic!("expected shell output");
        };
        assert_eq!(content.lines.first().map(String::as_str), Some("line3"));
        assert!(content.lines_omitted);
        let rendered = render_view(&view, 70);

        assert!(rendered.contains("line3"));
        assert!(rendered.contains("line12"));
        assert!(rendered.contains("Showing last 10 lines"));
        assert!(rendered.contains("earlier lines hidden"));
        assert_snapshot!("shell_local_line_truncation_snapshot", rendered);
    }

    #[test]
    fn monitor_detail_sanitizes_ansi_and_expands_tabs_at_narrow_width() {
        let rendered = render_view(
            &monitor_view_with_output("\x1b[31mRED\x1b[0m\t列🚀", 24, false),
            32,
        );

        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\t'));
        assert!(rendered.contains("RED"));
        assert!(rendered.contains('列'));
        assert!(rendered.contains('🚀'));
        assert_snapshot!("monitor_detail_ansi_tabs_narrow_snapshot", rendered);
    }

    #[test]
    fn process_list_sanitizes_monitor_label_ansi_and_tabs() {
        let mut monitor = monitor_item();
        monitor.monitor.as_mut().unwrap().description = "\x1b[31m监控\x1b[0m\t🚀".to_string();
        let rendered = render_view(&view(vec![monitor, shell_item()]), 32);

        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\t'));
        assert!(rendered.contains("监"));
        assert!(rendered.contains("控"));
        assert!(rendered.contains('🚀'));
        assert_snapshot!("process_list_ansi_tabs_narrow_snapshot", rendered);
    }

    #[test]
    fn enter_opens_details_and_left_returns_to_list() {
        let mut view = view(vec![monitor_item(), shell_item()]);

        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(view.detail_idx, Some(0));
        assert!(!view.is_complete());

        view.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(view.detail_idx, None);
        assert!(!view.is_complete());
    }

    #[test]
    fn escape_closes_details() {
        let mut view = view(vec![monitor_item()]);

        view.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(view.completion(), Some(ViewCompletion::Cancelled));
    }

    #[test]
    fn live_update_refreshes_detail_and_completion_closes_it() {
        let now = Instant::now();
        let (mut view, mut rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let request = next_monitor_request(&mut rx);
        assert!(view.apply_monitor_output_response_at(
            response(request, "authoritative output", 20, false),
            now + Duration::from_millis(100),
        ));
        let mut updated = monitor_item_at(now);
        updated.recent_output = vec!["framed-only delta".to_string()];

        assert!(view.update_background_processes(vec![updated]));
        let rendered = render_view(&view, 70);
        assert!(rendered.contains("authoritative output"));
        assert!(!rendered.contains("framed-only delta"));
        assert!(!view.is_complete());

        assert!(view.update_background_processes(Vec::new()));
        assert!(view.is_complete());
    }

    #[test]
    fn monitor_poll_is_throttled_and_has_only_one_in_flight_request() {
        let now = Instant::now();
        let (mut view, mut rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let first = next_monitor_request(&mut rx);

        assert!(!view.pre_draw_tick(now + Duration::from_secs(2)));
        assert!(
            rx.try_recv().is_err(),
            "in-flight request must suppress polling"
        );

        assert!(view.apply_monitor_output_response_at(
            response(first, "ready", 5, false),
            now + Duration::from_millis(500),
        ));
        assert!(rx.try_recv().is_err());
        assert!(!view.pre_draw_tick(now + Duration::from_millis(999)));
        assert!(rx.try_recv().is_err());
        assert!(view.pre_draw_tick(now + MONITOR_OUTPUT_POLL_INTERVAL));
        let _second = next_monitor_request(&mut rx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn hung_monitor_poll_suspends_automatic_retries() {
        let now = Instant::now();
        let (mut view, mut rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let first = next_monitor_request(&mut rx);

        assert!(!view.pre_draw_tick(
            now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT - Duration::from_millis(1)
        ));
        assert!(rx.try_recv().is_err());

        assert!(view.pre_draw_tick(now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT));
        assert_eq!(view.monitor_poll_lifecycle, MonitorPollLifecycle::Suspended);
        assert!(rx.try_recv().is_err());
        let rendered = render_view(&view, 70);
        assert!(rendered.contains("Output unavailable"));
        assert!(rendered.contains("refresh paused"));
        assert!(!view.pre_draw_tick(
            now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT + Duration::from_secs(30)
        ));
        assert!(rx.try_recv().is_err());

        assert!(!view.apply_monitor_output_response_at(
            response(first, "stale output", 12, false),
            now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT + Duration::from_millis(1),
        ));
        assert_eq!(view.monitor_poll_lifecycle, MonitorPollLifecycle::Suspended);
        assert!(!render_view(&view, 70).contains("stale output"));
    }

    #[test]
    fn monitor_poll_error_suspends_automatic_retries() {
        let now = Instant::now();
        let (mut view, mut rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let request = next_monitor_request(&mut rx);

        assert!(view.apply_monitor_output_response_at(
            BackgroundMonitorOutputResponse {
                request,
                result: Err("request timed out".to_string()),
            },
            now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT,
        ));
        assert_eq!(view.monitor_poll_lifecycle, MonitorPollLifecycle::Suspended);
        assert!(!view.pre_draw_tick(
            now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT + Duration::from_secs(30)
        ));
        assert!(rx.try_recv().is_err());
        let rendered = render_view(&view, 70);
        assert!(rendered.contains("Output unavailable"));
        assert!(rendered.contains("refresh paused"));
        insta::assert_snapshot!("monitor_poll_error_suspended_snapshot", rendered);
    }

    #[test]
    fn monitor_poll_error_preserves_last_output_but_marks_it_stale() {
        let now = Instant::now();
        let (mut view, mut rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let first = next_monitor_request(&mut rx);

        assert!(view.apply_monitor_output_response_at(
            response(first, "ready\nsecret from stderr", 24, false),
            now + Duration::from_millis(100),
        ));
        assert!(view.pre_draw_tick(now + MONITOR_OUTPUT_POLL_INTERVAL));
        let second = next_monitor_request(&mut rx);
        assert!(view.apply_monitor_output_response_at(
            BackgroundMonitorOutputResponse {
                request: second,
                result: Err("transport closed".to_string()),
            },
            now + MONITOR_OUTPUT_POLL_INTERVAL + Duration::from_millis(100),
        ));

        assert_eq!(view.monitor_poll_lifecycle, MonitorPollLifecycle::Suspended);
        assert!(matches!(
            &view.monitor_output_state,
            DetailOutputState::Suspended(Some(_))
        ));
        let rendered = render_view(&view, 70);
        assert!(rendered.contains("ready"));
        assert!(rendered.contains("secret from stderr"));
        assert!(rendered.contains("stale"));
        assert!(rendered.contains("refresh paused"));
        insta::assert_snapshot!(
            "monitor_poll_error_preserves_last_output_but_marks_it_stale_snapshot",
            rendered
        );
    }

    #[test]
    fn switching_targets_replaces_hung_request_and_rejects_stale_responses() {
        let now = Instant::now();
        let (mut view, mut rx) =
            view_with_rx_at(vec![monitor_item_at(now), second_monitor_at(now)], now);
        assert!(rx.try_recv().is_err(), "list view must not poll");

        view.open_selected_at(now);
        let first = next_monitor_request(&mut rx);
        view.detail_idx = None;
        view.sync_monitor_output_target_at(now + Duration::from_millis(10));
        view.state.selected_idx = Some(1);
        view.open_selected_at(now + Duration::from_millis(20));
        let second = next_monitor_request(&mut rx);
        assert_eq!(second.process_id, "403");
        assert_eq!(second.task_id, "task-b2");

        assert!(!view.apply_monitor_output_response_at(
            response(first, "stale output", 12, false),
            now + Duration::from_millis(30),
        ));
        assert!(!render_view(&view, 70).contains("stale output"));

        let mut wrong_generation = second.clone();
        wrong_generation.generation = Uuid::new_v4();
        assert!(!view.apply_monitor_output_response_at(
            response(wrong_generation, "wrong generation", 16, false),
            now + Duration::from_millis(40),
        ));
        assert!(matches!(
            &view.monitor_poll_lifecycle,
            MonitorPollLifecycle::InFlight { request, .. } if request == &second
        ));

        assert!(view.apply_monitor_output_response_at(
            response(second, "current output", 14, false),
            now + Duration::from_millis(50),
        ));
        let rendered = render_view(&view, 70);
        assert!(rendered.contains("current output"));
        assert!(!rendered.contains("wrong generation"));
    }

    #[test]
    fn leaving_and_reopening_details_replaces_hung_request() {
        let now = Instant::now();
        let (mut view, mut rx) =
            view_with_rx_at(vec![monitor_item_at(now), shell_item_at(now)], now);

        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let first = next_monitor_request(&mut rx);
        view.handle_key_event(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(view.detail_idx, None);
        assert_eq!(view.monitor_poll_lifecycle, MonitorPollLifecycle::Idle);

        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let reopened = next_monitor_request(&mut rx);
        assert_eq!(reopened.thread_id, first.thread_id);
        assert_eq!(reopened.process_id, first.process_id);
        assert_eq!(reopened.task_id, first.task_id);
        assert_ne!(reopened.generation, first.generation);

        assert!(!view.apply_monitor_output_response_at(
            response(first, "stale output", 12, false),
            Instant::now(),
        ));
        assert!(matches!(
            &view.monitor_poll_lifecycle,
            MonitorPollLifecycle::InFlight { request, .. } if request == &reopened
        ));
    }

    #[test]
    fn reopened_popup_rejects_response_from_closed_instance() {
        let now = Instant::now();
        let (mut closed_view, mut closed_rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let closed_request = next_monitor_request(&mut closed_rx);
        closed_view.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(closed_view.completion(), Some(ViewCompletion::Cancelled));

        let (mut reopened_view, mut reopened_rx) = view_with_rx_at(vec![monitor_item_at(now)], now);
        let reopened_request = next_monitor_request(&mut reopened_rx);
        assert_ne!(reopened_request.generation, closed_request.generation);

        assert!(!reopened_view.apply_monitor_output_response_at(
            response(closed_request, "stale output", 12, false),
            now + Duration::from_millis(1),
        ));
        assert!(matches!(
            &reopened_view.monitor_poll_lifecycle,
            MonitorPollLifecycle::InFlight { request, .. } if request == &reopened_request
        ));
    }

    #[test]
    fn shell_and_unselected_monitor_do_not_poll() {
        let now = Instant::now();
        let (mut shell_view, mut shell_rx) = view_with_rx_at(vec![shell_item_at(now)], now);
        assert!(!shell_view.pre_draw_tick(now + Duration::from_secs(1)));
        assert!(shell_rx.try_recv().is_err());

        let (mut list_view, mut list_rx) =
            view_with_rx_at(vec![monitor_item_at(now), shell_item_at(now)], now);
        assert!(!list_view.pre_draw_tick(now + Duration::from_secs(1)));
        assert!(list_rx.try_recv().is_err());
    }

    #[test]
    fn output_uses_last_ten_nonempty_crlf_lines_and_preserves_spaces() {
        let tail = concat!(
            "line1\r\n",
            "\r\n",
            "line2\r\n",
            "line3\r\n",
            "  padded  \r\n",
            "   \r\n",
            "line5\r\n",
            "line6\r\n",
            "line7\r\n",
            "line8\r\n",
            "line9\r\n",
            "line10\r\n",
            "line11\r\n",
            "line12\r\n"
        );
        let DetailOutputState::Content(DetailOutputContent {
            lines,
            lines_omitted,
            ..
        }) = detail_output_state_from_snapshot(Some(ThreadBackgroundTerminalOutput {
            tail: tail.to_string(),
            bytes_total: tail.len() as u64,
            truncated: true,
        }))
        else {
            panic!("expected content");
        };

        assert_eq!(
            lines,
            vec![
                "line3",
                "  padded  ",
                "line5",
                "line6",
                "line7",
                "line8",
                "line9",
                "line10",
                "line11",
                "line12",
            ]
        );
        assert!(lines_omitted);
    }

    #[test]
    fn absent_or_whitespace_only_snapshot_has_no_output() {
        assert_eq!(
            detail_output_state_from_snapshot(None),
            DetailOutputState::NoOutput
        );
        assert_eq!(
            detail_output_state_from_snapshot(Some(ThreadBackgroundTerminalOutput {
                tail: "  \r\n\t\r\n".to_string(),
                bytes_total: 7,
                truncated: false,
            })),
            DetailOutputState::NoOutput
        );
    }

    #[test]
    fn human_bytes_uses_binary_units_without_unit_spacing() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(1023), "1023 bytes");
        assert_eq!(format_bytes(1024), "1KB");
        assert_eq!(format_bytes(1536), "1.5KB");
        assert_eq!(format_bytes(1024 * 1024), "1MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 + 512 * 1024), "5.5MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1GB");
    }
}
