//! Renders and formats unified-exec background session summary text.
//!
//! This module provides one canonical summary string so the bottom pane can
//! either render a dedicated footer row or reuse the same text inline in the
//! status row without duplicating copy/grammar logic.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::live_wrap::take_prefix_by_width;
use crate::render::renderable::Renderable;

/// Tracks active unified-exec processes and renders a compact summary.
pub(crate) struct UnifiedExecFooter {
    background_terminal_count: usize,
    monitor_count: usize,
}

impl UnifiedExecFooter {
    pub(crate) fn new() -> Self {
        Self {
            background_terminal_count: 0,
            monitor_count: 0,
        }
    }

    pub(crate) fn set_process_counts(
        &mut self,
        background_terminal_count: usize,
        monitor_count: usize,
    ) -> bool {
        if self.background_terminal_count == background_terminal_count
            && self.monitor_count == monitor_count
        {
            return false;
        }
        self.background_terminal_count = background_terminal_count;
        self.monitor_count = monitor_count;
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.background_terminal_count == 0 && self.monitor_count == 0
    }

    /// Returns the unindented summary text used by both footer and status-row rendering.
    ///
    /// The returned string intentionally omits leading spaces and separators so
    /// callers can choose layout-specific framing (inline separator vs. row
    /// indentation). Returning `None` means there is nothing to surface.
    pub(crate) fn summary_text(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let monitor_summary = (self.monitor_count > 0).then(|| {
            let plural = if self.monitor_count == 1 { "" } else { "s" };
            format!("{} monitor{plural}", self.monitor_count)
        });
        let terminal_summary = (self.background_terminal_count > 0).then(|| {
            let plural = if self.background_terminal_count == 1 {
                ""
            } else {
                "s"
            };
            format!(
                "{} background terminal{plural} running · /ps to view · /stop to close",
                self.background_terminal_count
            )
        });
        match (monitor_summary, terminal_summary) {
            (Some(monitors), Some(_)) => {
                let shell_plural = if self.background_terminal_count == 1 {
                    ""
                } else {
                    "s"
                };
                Some(format!(
                    "{} shell{shell_plural}, {monitors} · ↓ to manage",
                    self.background_terminal_count
                ))
            }
            (Some(monitors), None) => Some(format!("{monitors} · ↓ to manage")),
            (None, Some(terminals)) => Some(terminals),
            (None, None) => None,
        }
    }

    fn render_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width < 4 {
            return Vec::new();
        }
        let Some(summary) = self.summary_text() else {
            return Vec::new();
        };
        let message = format!("  {summary}");
        let (truncated, _, _) = take_prefix_by_width(&message, width as usize);
        vec![Line::from(truncated.dim())]
    }
}

impl Renderable for UnifiedExecFooter {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        Paragraph::new(self.render_lines(area.width)).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.render_lines(width).len() as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;

    #[test]
    fn desired_height_empty() {
        let footer = UnifiedExecFooter::new();
        assert_eq!(footer.desired_height(/*width*/ 40), 0);
    }

    #[test]
    fn render_more_sessions() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_process_counts(
            /*background_terminal_count*/ 1, /*monitor_count*/ 0,
        );
        let width = 50;
        let height = footer.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        footer.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_more_sessions", format!("{buf:?}"));
    }

    #[test]
    fn render_many_sessions() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_process_counts(
            /*background_terminal_count*/ 123, /*monitor_count*/ 0,
        );
        let width = 50;
        let height = footer.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        footer.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_many_sessions", format!("{buf:?}"));
    }

    #[test]
    fn render_one_monitor() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_process_counts(
            /*background_terminal_count*/ 0, /*monitor_count*/ 1,
        );
        let width = 50;
        let height = footer.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        footer.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_one_monitor", format!("{buf:?}"));
    }

    #[test]
    fn render_mixed_processes() {
        let mut footer = UnifiedExecFooter::new();
        footer.set_process_counts(
            /*background_terminal_count*/ 1, /*monitor_count*/ 2,
        );
        let width = 90;
        let height = footer.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        footer.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_mixed_processes", format!("{buf:?}"));
    }
}
