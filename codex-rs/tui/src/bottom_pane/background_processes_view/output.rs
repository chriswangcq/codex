//! Output refresh state, request generations, and bounded detail snapshots.
//!
//! The view owns the single lifecycle; this module advances it and rejects stale replies.

use super::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MonitorOutputTarget {
    pub(super) thread_id: ThreadId,
    pub(super) process_id: String,
    pub(super) task_id: String,
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
pub(super) enum MonitorPollLifecycle {
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
pub(super) struct DetailOutputContent {
    pub(super) lines: Vec<String>,
    pub(super) bytes_total: Option<u64>,
    pub(super) truncated: bool,
    pub(super) lines_omitted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DetailOutputState {
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

    pub(super) fn has_content(&self) -> bool {
        matches!(self, Self::Content(_) | Self::Suspended(Some(_)))
    }
}

impl BackgroundProcessesView {
    fn selected_monitor_output_target(&self) -> Option<MonitorOutputTarget> {
        let item = self.detail_idx.and_then(|idx| self.items.get(idx))?;
        let monitor = item.monitor.as_ref()?;
        Some(MonitorOutputTarget {
            thread_id: self.thread_id?,
            process_id: item.process_id.clone(),
            task_id: monitor.task_id.clone(),
        })
    }

    pub(super) fn sync_monitor_output_target_at(&mut self, now: Instant) -> bool {
        let target = self.selected_monitor_output_target();
        if target != self.monitor_output_target {
            self.monitor_output_target = target;
            self.monitor_output_state = DetailOutputState::Loading;
            self.monitor_poll_lifecycle = MonitorPollLifecycle::Idle;
            self.monitor_output_last_started_at = None;
        }
        self.maybe_request_monitor_output_at(now)
    }

    pub(super) fn maybe_request_monitor_output_at(&mut self, now: Instant) -> bool {
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
        self.monitor_output_last_started_at = Some(now);
        self.monitor_poll_lifecycle = MonitorPollLifecycle::InFlight {
            request: request.clone(),
            started_at: now,
        };
        self.app_event_tx
            .send(AppEvent::FetchBackgroundMonitorOutput(request));
        true
    }

    pub(super) fn apply_monitor_output_response_at(
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
            .is_some_and(|target| target.matches_request(&request));
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
}

pub(super) fn detail_output_state_from_snapshot(
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
pub(super) struct LastNonemptyLines {
    pub(super) lines: Vec<String>,
    pub(super) lines_omitted: bool,
}

pub(super) fn last_nonempty_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
) -> LastNonemptyLines {
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
