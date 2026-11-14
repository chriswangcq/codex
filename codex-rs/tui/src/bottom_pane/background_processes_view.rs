//! Interactive manager for background shells and command monitors.
//!
//! Selection and stop actions live here. `output` owns refresh transitions and stale-reply
//! checks; `render` projects that state into the existing bottom-pane UI.

mod output;
mod render;

use output::DetailOutputContent;
use output::DetailOutputState;
use output::MonitorOutputTarget;
use output::MonitorPollLifecycle;
#[cfg(test)]
use output::detail_output_state_from_snapshot;
use output::last_nonempty_lines;
use render::ansi_line_text;
#[cfg(test)]
use render::format_bytes;

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

#[cfg(test)]
#[path = "background_processes_view_tests.rs"]
mod tests;
