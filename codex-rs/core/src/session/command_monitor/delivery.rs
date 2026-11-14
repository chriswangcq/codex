//! UI events and model notifications through the session input path.

use crate::context::push_xml_escaped_text;
use crate::session::session::Session;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CommandMonitorInfo;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecOutputStream;
use codex_protocol::turn_input::TurnInputSubmission;
use std::path::Path;
use std::sync::Weak;

pub(super) async fn deliver_monitor_timeout(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
) {
    deliver_monitor_event(
        session,
        monitor,
        tool_use_id,
        event_turn_id,
        "[Monitor timed out — re-arm if needed.]",
    )
    .await;
}

pub(super) async fn deliver_monitor_event(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    event_turn_id: &str,
    event: &str,
) {
    let Some(active_session) = session.upgrade() else {
        return;
    };
    let Some(delta) =
        admit_monitor_output_delta(active_session.as_ref(), event_turn_id, tool_use_id, event)
    else {
        return;
    };
    // Admission is linearized under the runtime gate, but persistence and
    // delivery remain async and never hold the gate's synchronous mutex.
    active_session.send_event_raw(delta).await;
    if active_session.is_session_runtime_closing() {
        return;
    }

    let mut text = String::from("<task-notification>\n<task-id>");
    push_xml_escaped_text(&mut text, &monitor.task_id);
    text.push_str("</task-id>\n<summary>Monitor event: \"");
    push_xml_escaped_text(&mut text, &monitor.description);
    text.push_str("\"</summary>\n<event>");
    push_xml_escaped_text(&mut text, event);
    text.push_str("</event>\n</task-notification>");
    deliver_monitor_item(session, text_response_item(text)).await;
}

pub(super) fn admit_monitor_output_delta(
    session: &Session,
    event_turn_id: &str,
    tool_use_id: &str,
    event: &str,
) -> Option<Event> {
    session.try_run_while_session_runtime_open(|| {
        monitor_output_delta_event(event_turn_id, tool_use_id, event)
    })
}

pub(super) fn monitor_output_delta_event(
    event_turn_id: &str,
    tool_use_id: &str,
    event: &str,
) -> Event {
    Event {
        id: event_turn_id.to_string(),
        msg: EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
            call_id: tool_use_id.to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: event.as_bytes().to_vec(),
        }),
    }
}

pub(super) async fn deliver_monitor_failure(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    message: &str,
) {
    let mut text = String::from("<task-notification>\n<task-id>");
    push_xml_escaped_text(&mut text, &monitor.task_id);
    text.push_str("</task-id>\n<tool-use-id>");
    push_xml_escaped_text(&mut text, tool_use_id);
    text.push_str("</tool-use-id>\n<status>failed</status>\n<summary>Monitor \"");
    push_xml_escaped_text(&mut text, &monitor.description);
    text.push_str("\" failed: ");
    push_xml_escaped_text(&mut text, message);
    text.push_str("</summary>\n</task-notification>");
    deliver_monitor_item(session, text_response_item(text)).await;
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn deliver_monitor_completion(
    session: &Weak<Session>,
    monitor: &CommandMonitorInfo,
    tool_use_id: &str,
    output_file: &Path,
    exit_code: Option<i32>,
    failure_message: Option<String>,
    produced_output: bool,
) {
    let failed = failure_message.is_some() || exit_code.is_some_and(|code| code != 0);
    let status = if failed { "failed" } else { "completed" };
    let mut text = String::from("<task-notification>\n<task-id>");
    push_xml_escaped_text(&mut text, &monitor.task_id);
    text.push_str("</task-id>\n<tool-use-id>");
    push_xml_escaped_text(&mut text, tool_use_id);
    text.push_str("</tool-use-id>\n<output-file>");
    push_xml_escaped_text(&mut text, &output_file.display().to_string());
    text.push_str("</output-file>\n<status>");
    text.push_str(status);
    text.push_str("</status>\n<summary>Monitor \"");
    push_xml_escaped_text(&mut text, &monitor.description);
    text.push('"');
    let summary = monitor_completion_summary(
        failed,
        produced_output,
        exit_code,
        failure_message.as_deref(),
    );
    push_xml_escaped_text(&mut text, &summary);
    text.push_str("</summary>\n</task-notification>");
    deliver_monitor_item(session, text_response_item(text)).await;
}

pub(super) fn monitor_completion_summary(
    failed: bool,
    produced_output: bool,
    exit_code: Option<i32>,
    failure_message: Option<&str>,
) -> String {
    let mut summary = if failed {
        " script failed".to_string()
    } else if produced_output {
        " stream ended".to_string()
    } else {
        " ended without producing output".to_string()
    };
    if let Some(exit_code) = exit_code.filter(|_| failed || !produced_output) {
        summary.push_str(&format!(" (exit {exit_code})"));
    }
    if let Some(message) = failure_message {
        summary.push_str(": ");
        summary.push_str(message);
    }
    summary
}

pub(super) async fn deliver_monitor_item(session: &Weak<Session>, item: ResponseItem) {
    let Some(session) = session.upgrade() else {
        return;
    };
    let mut items = vec![item];
    loop {
        // Register before checking admission so teardown cannot be missed between the check
        // and the wait. This signal carries no work; the existing input path still owns it.
        let changed = session.active_turn_changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if session.is_session_runtime_closing() {
            return;
        }
        items = match session.inject_if_running(items).await {
            Ok(()) => return,
            Err(items) => items,
        };
        // The finishing token only marks transcript persistence. It can already be
        // cancelled while stop hooks and the terminal event still own the active turn.
        if session
            .active_turn
            .lock()
            .await
            .as_ref()
            .is_none_or(|turn| turn.finishing.is_none())
        {
            break;
        }
        changed.await;
    }
    if session.is_session_runtime_closing() {
        return;
    }
    for item in items {
        match session.try_start_monitor_turn_if_idle(item.clone()).await {
            // Accepted transfers input ownership to Core even when no turn ID is available.
            Ok(
                TurnInputSubmission::Accepted
                | TurnInputSubmission::Started { .. }
                | TurnInputSubmission::Steered { .. },
            ) => {}
            Ok(TurnInputSubmission::NotSubmitted { .. }) | Err(_) => {
                session
                    .inject_no_new_turn(vec![item], /*current_turn_context*/ None)
                    .await;
            }
        }
    }
}

pub(super) fn text_response_item(text: String) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
