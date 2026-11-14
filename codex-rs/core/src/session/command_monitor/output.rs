//! Pure physical-line framing, batching and event-rate policy.

use std::time::Duration;
use tokio::time::Instant;

const MAX_PENDING_UTF16_UNITS: usize = 1024 * 1024;
const MAX_LINE_UTF16_UNITS: usize = 500;
const MAX_BATCH_UTF16_UNITS: usize = 3000;
const BATCH_WINDOW: Duration = Duration::from_millis(200);
const RATE_LIMIT_CAPACITY: u32 = 10;
const RATE_LIMIT_REFILL_INTERVAL: Duration = Duration::from_secs(2);
const OVERLOAD_STOP_AFTER: Duration = Duration::from_secs(30);
const OVERLOAD_QUIET_RESET_AFTER: Duration = Duration::from_secs(6);
const TRUNCATION_SUFFIX: &str = "...(truncated)";

#[derive(Default)]
pub(super) struct PhysicalLineFramer {
    pending: String,
    pending_start: usize,
    pending_utf16_units: usize,
    undecoded: Vec<u8>,
    truncated_head: bool,
}

impl PhysicalLineFramer {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            if byte == b'\n' {
                self.append_encoded(&bytes[start..index]);
                self.finish_decoder();
                if let Some(line) = self.take_line() {
                    lines.push(line);
                }
                start = index + 1;
            }
        }
        self.append_encoded(&bytes[start..]);
        lines
    }

    pub(super) fn finish(&mut self) -> Option<String> {
        self.finish_decoder();
        if self.pending_start == self.pending.len() && !self.truncated_head {
            return None;
        }
        self.take_line()
    }

    fn take_line(&mut self) -> Option<String> {
        let trimmed = self.pending[self.pending_start..].trim_matches(is_ecmascript_whitespace);
        let line = (!trimmed.is_empty())
            .then(|| truncate_utf16(trimmed, MAX_LINE_UTF16_UNITS, self.truncated_head));
        self.pending.clear();
        self.pending_start = 0;
        self.pending_utf16_units = 0;
        self.truncated_head = false;
        line
    }

    fn append_encoded(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut encoded = std::mem::take(&mut self.undecoded);
        encoded.extend_from_slice(bytes);
        let mut offset = 0;
        while offset < encoded.len() {
            match std::str::from_utf8(&encoded[offset..]) {
                Ok(text) => {
                    self.append_text(text);
                    offset = encoded.len();
                }
                Err(err) => {
                    let valid_end = offset + err.valid_up_to();
                    if valid_end > offset
                        && let Ok(valid) = std::str::from_utf8(&encoded[offset..valid_end])
                    {
                        self.append_text(valid);
                    }
                    match err.error_len() {
                        Some(error_len) => {
                            self.append_text("�");
                            offset = valid_end + error_len;
                        }
                        None => {
                            self.undecoded.extend_from_slice(&encoded[valid_end..]);
                            offset = encoded.len();
                        }
                    }
                }
            }
        }
    }

    fn finish_decoder(&mut self) {
        if self.undecoded.is_empty() {
            return;
        }
        let undecoded = std::mem::take(&mut self.undecoded);
        self.append_text(&String::from_utf8_lossy(&undecoded));
    }

    fn append_text(&mut self, text: &str) {
        self.pending.push_str(text);
        self.pending_utf16_units = self.pending_utf16_units.saturating_add(utf16_units(text));
        if self.pending_utf16_units <= MAX_PENDING_UTF16_UNITS {
            return;
        }

        let units_to_drop = self.pending_utf16_units - MAX_PENDING_UTF16_UNITS;
        let mut dropped_units = 0;
        let mut new_start = self.pending_start;
        for (index, ch) in self.pending[self.pending_start..].char_indices() {
            dropped_units += ch.len_utf16();
            new_start = self.pending_start + index + ch.len_utf8();
            if dropped_units >= units_to_drop {
                break;
            }
        }
        self.pending_start = new_start;
        self.pending_utf16_units -= dropped_units;
        self.truncated_head = true;
        if self.pending_start >= 64 * 1024 && self.pending_start * 2 >= self.pending.len() {
            self.pending.drain(..self.pending_start);
            self.pending_start = 0;
        }
    }
}

fn is_ecmascript_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

#[derive(Default)]
pub(super) struct MonitorBatch {
    text: String,
    utf16_units: usize,
    truncated: bool,
    started_at: Option<Instant>,
}

impl MonitorBatch {
    pub(super) fn push_line(&mut self, line: &str, now: Instant) {
        self.started_at.get_or_insert(now);
        if self.truncated {
            return;
        }

        let separator_units = usize::from(!self.text.is_empty());
        let line_units = utf16_units(line);
        if self.utf16_units + separator_units + line_units <= MAX_BATCH_UTF16_UNITS {
            if separator_units == 1 {
                self.text.push('\n');
            }
            self.text.push_str(line);
            self.utf16_units += separator_units + line_units;
            return;
        }

        let mut remaining = MAX_BATCH_UTF16_UNITS.saturating_sub(self.utf16_units);
        if separator_units == 1 && remaining > 0 {
            self.text.push('\n');
            self.utf16_units += 1;
            remaining -= 1;
        }
        let prefix = utf16_prefix(line, remaining);
        self.text.push_str(prefix);
        self.utf16_units += utf16_units(prefix);
        self.truncated = true;
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.started_at.map(|started| started + BATCH_WINDOW)
    }

    pub(super) fn request_flush(&mut self, now: Instant) {
        if self.started_at.is_some() {
            self.started_at = Some(now.checked_sub(BATCH_WINDOW).unwrap_or(now));
        }
    }

    pub(super) fn take(&mut self) -> Option<String> {
        self.started_at?;
        let mut text = std::mem::take(&mut self.text);
        if self.truncated {
            text.push('\n');
            text.push_str(TRUNCATION_SUFFIX);
        }
        *self = Self::default();
        Some(text)
    }
}

pub(super) struct MonitorRateLimiter {
    tokens: u32,
    last_refill: Instant,
    suppressed_since_notice: u64,
    overload_started: Option<Instant>,
    last_event_at: Option<Instant>,
}

pub(super) struct RateDispatch {
    pub(super) deliveries: Vec<String>,
    pub(super) stop_message: Option<String>,
}

impl MonitorRateLimiter {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            tokens: RATE_LIMIT_CAPACITY,
            last_refill: now,
            suppressed_since_notice: 0,
            overload_started: None,
            last_event_at: None,
        }
    }

    pub(super) fn submit(
        &mut self,
        batch: String,
        now: Instant,
        stop_on_overload: bool,
    ) -> RateDispatch {
        self.refill(now);
        if self
            .last_event_at
            .is_some_and(|last| now.saturating_duration_since(last) > OVERLOAD_QUIET_RESET_AFTER)
        {
            self.reset_overload_tracking();
        }
        self.last_event_at = Some(now);
        let mut deliveries = Vec::new();
        if self.suppressed_since_notice > 0 && self.take_token() {
            deliveries.push(suppression_message(self.suppressed_since_notice));
            self.suppressed_since_notice = 0;
        }
        if self.take_token() {
            deliveries.push(batch);
            return RateDispatch {
                deliveries,
                stop_message: None,
            };
        }

        self.suppressed_since_notice = self.suppressed_since_notice.saturating_add(1);
        self.overload_started.get_or_insert(now);
        let stop_message = self.overload_started.and_then(|started| {
            let duration = now.saturating_duration_since(started);
            (stop_on_overload && duration > OVERLOAD_STOP_AFTER)
                .then(|| overload_stop_message(self.suppressed_since_notice, duration))
        });
        if stop_message.is_some() {
            deliveries.clear();
            self.suppressed_since_notice = 0;
        }
        RateDispatch {
            deliveries,
            stop_message,
        }
    }

    pub(super) fn take_suppression_notice(&mut self) -> Option<String> {
        if self.suppressed_since_notice == 0 {
            return None;
        }
        let message = suppression_message(self.suppressed_since_notice);
        self.suppressed_since_notice = 0;
        self.reset_overload_tracking();
        Some(message)
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill);
        let intervals = elapsed.as_nanos() / RATE_LIMIT_REFILL_INTERVAL.as_nanos();
        if intervals == 0 {
            return;
        }
        let intervals = u32::try_from(intervals).unwrap_or(u32::MAX);
        self.tokens = self
            .tokens
            .saturating_add(intervals)
            .min(RATE_LIMIT_CAPACITY);
        self.last_refill += RATE_LIMIT_REFILL_INTERVAL * intervals;
    }

    fn take_token(&mut self) -> bool {
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }

    fn reset_overload_tracking(&mut self) {
        self.suppressed_since_notice = 0;
        self.overload_started = None;
    }
}

fn utf16_units(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_prefix(text: &str, max_units: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let units = ch.len_utf16();
        if used + units > max_units {
            break;
        }
        used += units;
        end = index + ch.len_utf8();
    }
    &text[..end]
}

fn truncate_utf16(text: &str, max_units: usize, force_truncated: bool) -> String {
    if !force_truncated && utf16_units(text) <= max_units {
        return text.to_string();
    }
    let mut truncated = utf16_prefix(text, max_units).to_string();
    truncated.push_str(TRUNCATION_SUFFIX);
    truncated
}

fn suppression_message(count: u64) -> String {
    format!(
        "[{count} events suppressed — output rate too high. Consider using TaskStop to restart this monitor with a more selective filter.]"
    )
}

fn overload_stop_message(count: u64, duration: Duration) -> String {
    format!(
        "[Monitor stopped — too much output ({count} events suppressed over {}s). Restart with a more selective source.]",
        duration.as_secs().max(OVERLOAD_STOP_AFTER.as_secs())
    )
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
