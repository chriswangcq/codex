use crate::render::renderable::Renderable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use serde_json::Value;
use std::fs;
use std::path::Path;

pub(crate) struct QunuxCockpitRenderable<'a> {
    workspace: &'a Path,
    snapshot: Option<&'a QunuxCockpitSnapshot>,
}

impl<'a> QunuxCockpitRenderable<'a> {
    pub(crate) fn new(workspace: &'a Path, snapshot: Option<&'a QunuxCockpitSnapshot>) -> Self {
        Self {
            workspace,
            snapshot,
        }
    }
}

impl Renderable for QunuxCockpitRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let lines = lines_for_snapshot_or_workspace(self.workspace, self.snapshot, area.width);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(lines_for_snapshot_or_workspace(self.workspace, self.snapshot, width).len())
            .unwrap_or(u16::MAX)
            .max(8)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QunuxCockpitSnapshot {
    pub(crate) process_id: String,
    pub(crate) qunux_thread_id: Option<String>,
    pub(crate) state_path: Option<String>,
    pub(crate) state: Value,
}

impl QunuxCockpitSnapshot {
    pub(crate) fn new(
        process_id: String,
        qunux_thread_id: Option<String>,
        state_path: Option<String>,
        state: Value,
    ) -> Self {
        Self {
            process_id,
            qunux_thread_id,
            state_path,
            state,
        }
    }
}

#[cfg(test)]
fn lines_for_workspace(workspace: &Path, width: u16) -> Vec<Line<'static>> {
    lines_for_snapshot_or_workspace(workspace, None, width)
}

pub(crate) fn lines_for_snapshot_or_workspace(
    workspace: &Path,
    snapshot: Option<&QunuxCockpitSnapshot>,
    _width: u16,
) -> Vec<Line<'static>> {
    let processes = snapshot
        .map(|snapshot| vec![ProcessSnapshot::from(snapshot)])
        .unwrap_or_else(|| load_processes(workspace));
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "Qunux Agent OS Cockpit",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "attachable runtime view",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("workspace ", Style::default().fg(Color::DarkGray)),
        Span::raw(workspace.display().to_string()),
    ]));
    lines.push(Line::from(""));

    if processes.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "No Qunux process state found.",
            Style::default().fg(Color::Yellow),
        )]));
        lines.push(Line::from(format!(
            "Expected persisted state under {}",
            workspace.join(".qunux/processes").display()
        )));
        lines.push(Line::from(
            "Start a Qunux-enabled session or call qunux.current to initialize the process.",
        ));
        return lines;
    }

    for process in processes {
        append_process_lines(&mut lines, &process);
        lines.push(Line::from(""));
    }
    lines
}

struct ProcessSnapshot {
    process_id: String,
    qunux_thread_id: Option<String>,
    state_path: Option<String>,
    state: Value,
}

impl From<&QunuxCockpitSnapshot> for ProcessSnapshot {
    fn from(snapshot: &QunuxCockpitSnapshot) -> Self {
        Self {
            process_id: snapshot.process_id.clone(),
            qunux_thread_id: snapshot.qunux_thread_id.clone(),
            state_path: snapshot.state_path.clone(),
            state: snapshot.state.clone(),
        }
    }
}

fn load_processes(workspace: &Path) -> Vec<ProcessSnapshot> {
    let processes_dir = workspace.join(".qunux/processes");
    let Ok(entries) = fs::read_dir(processes_dir) else {
        return Vec::new();
    };

    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let state_path = entry.path().join("closure.json");
        let Ok(raw) = fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let process_id = state
            .get("process_id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
        processes.push(ProcessSnapshot {
            process_id,
            qunux_thread_id: None,
            state_path: Some(state_path.display().to_string()),
            state,
        });
    }
    processes.sort_by(|left, right| {
        process_created_at(&right.state)
            .cmp(process_created_at(&left.state))
            .then_with(|| right.process_id.cmp(&left.process_id))
    });
    processes
}

fn append_process_lines(lines: &mut Vec<Line<'static>>, process: &ProcessSnapshot) {
    let state = &process.state;
    let root_id = str_field(state, "root_id").unwrap_or("P000");
    let main_thread_id = str_field(state, "main_thread_id").unwrap_or("QT000");
    let current_thread_id = process.qunux_thread_id.as_deref().unwrap_or(main_thread_id);
    let problem_count = object_len(state, "problems");
    let ticket_count = object_len(state, "tickets");
    let result_count = object_len(state, "results");
    let check_count = object_len(state, "checks");
    let thread_count = object_len(state, "threads");
    let wait_count = object_len(state, "waits");
    let handle_count = object_len(state, "handles");
    let passive_event_count = array_len(state, "passive_events");
    let inbox_count = array_len(state, "inbox");
    let pending_inbox_count = array_values(state, "inbox")
        .filter(|item| str_field(item, "status") == Some("inboxed"))
        .count();
    let open_problem_count = object_values(state, "problems")
        .filter(|problem| str_field(problem, "status") != Some("done"))
        .count();
    let running_thread_count = object_values(state, "threads")
        .filter(|thread| str_field(thread, "status") == Some("running"))
        .count();
    let next = derive_next(state, current_thread_id);

    lines.push(Line::from(vec![
        Span::styled(
            format!("process {}", process.process_id),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  root {root_id}  main {main_thread_id}  current {current_thread_id}"
        )),
    ]));
    lines.push(Line::from(format!(
        "state {}",
        process.state_path.as_deref().unwrap_or("pushed snapshot")
    )));
    lines.push(Line::from(format!(
        "tasks {problem_count} ({open_problem_count} open) | tickets {ticket_count} | results {result_count} | checks {check_count}"
    )));
    lines.push(Line::from(format!(
        "threads {thread_count} ({running_thread_count} running) | waits {wait_count} | handles {handle_count}"
    )));
    lines.push(Line::from(format!(
        "passive events {passive_event_count} | inbox {inbox_count} ({pending_inbox_count} pending)"
    )));
    lines.push(Line::from(vec![
        Span::styled("next ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            next.disposition,
            Style::default().fg(color_for_disposition(next.disposition)),
        ),
        Span::raw(format!(
            " {} {}",
            next.action,
            next.target.as_deref().unwrap_or_default()
        )),
    ]));
    push_agent_loop_section(lines, state, current_thread_id, &next);
    push_problem_section(lines, state);
    push_thread_section(lines, state, current_thread_id);
    push_wait_handle_section(lines, state);
    push_passive_section(lines, state);
    push_event_section(lines, state);
}

fn push_problem_section(lines: &mut Vec<Line<'static>>, state: &Value) {
    let root_id = str_field(state, "root_id").unwrap_or("P000");

    lines.push(section_header("process mission"));
    if let Some(root_problem) = state
        .get("problems")
        .and_then(|problems| problems.get(root_id))
    {
        lines.push(Line::from(problem_row(root_problem)));
    } else {
        lines.push(Line::from(format!("  {root_id} [missing]")));
    }

    let mut user_tasks: Vec<&Value> = object_values(state, "problems")
        .filter(|problem| str_field(problem, "id") != Some(root_id))
        .filter(|problem| str_field(problem, "status") != Some("done"))
        .filter(|problem| is_user_task_problem(problem))
        .collect();
    user_tasks.sort_by_key(|problem| str_field(problem, "id").unwrap_or_default());

    lines.push(section_header("active user tasks"));
    if user_tasks.is_empty() {
        lines.push(Line::from("  none"));
    } else {
        for problem in user_tasks.into_iter().take(8) {
            lines.push(Line::from(problem_row(problem)));
        }
    }

    let mut open_work: Vec<&Value> = object_values(state, "problems")
        .filter(|problem| str_field(problem, "id") != Some(root_id))
        .filter(|problem| str_field(problem, "status") != Some("done"))
        .filter(|problem| !is_user_task_problem(problem))
        .collect();
    open_work.sort_by_key(|problem| str_field(problem, "id").unwrap_or_default());

    lines.push(section_header("open work tree"));
    if open_work.is_empty() {
        lines.push(Line::from("  none"));
    } else {
        for problem in open_work.into_iter().take(8) {
            lines.push(Line::from(problem_row(problem)));
        }
    }
}

fn problem_row(problem: &Value) -> String {
    let id = str_field(problem, "id").unwrap_or("?");
    let title = str_field(problem, "title").unwrap_or("untitled");
    let status = str_field(problem, "status").unwrap_or("unknown");
    let owner = str_field(problem, "owner_thread_id").unwrap_or("?");
    let ticket = str_field(problem, "ticket_id")
        .or_else(|| first_string_in_array(problem, "ticket_ids"))
        .unwrap_or("-");
    let source = user_task_source_suffix(problem).unwrap_or_default();
    format!("  {id} [{status}] owner {owner} ticket {ticket} :: {title}{source}")
}

fn is_user_task_problem(problem: &Value) -> bool {
    str_field(problem, "created_from_inbox_item_id").is_some()
        || str_field(problem, "created_from_passive_event_id").is_some()
}

fn user_task_source_suffix(problem: &Value) -> Option<String> {
    let inbox = str_field(problem, "created_from_inbox_item_id");
    let event = str_field(problem, "created_from_passive_event_id");
    match (inbox, event) {
        (Some(inbox), Some(event)) => Some(format!(" source inbox {inbox} event {event}")),
        (Some(inbox), None) => Some(format!(" source inbox {inbox}")),
        (None, Some(event)) => Some(format!(" source event {event}")),
        (None, None) => None,
    }
}

fn push_agent_loop_section(
    lines: &mut Vec<Line<'static>>,
    state: &Value,
    current_thread_id: &str,
    next: &NextSummary,
) {
    let current_thread = state
        .get("threads")
        .and_then(|threads| threads.get(current_thread_id));
    let status = current_thread
        .and_then(|thread| str_field(thread, "status"))
        .unwrap_or("unknown");
    let root = current_thread
        .and_then(|thread| str_field(thread, "root_problem_id"))
        .unwrap_or("-");
    let wait = active_wait_summary_for_thread(state, current_thread_id)
        .unwrap_or_else(|| "wait none".to_string());

    lines.push(section_header("dispatch state"));
    lines.push(Line::from(format!(
        "  thread {current_thread_id} [{status}] root {root}"
    )));
    lines.push(Line::from(vec![
        Span::raw("  dispatch "),
        Span::styled(
            loop_state_for_next(next.disposition),
            Style::default().fg(color_for_disposition(next.disposition)),
        ),
        Span::raw(format!(
            " next {} {}",
            next.action,
            next.target.as_deref().unwrap_or("-")
        )),
    ]));
    lines.push(Line::from(format!("  {wait}")));
}

fn push_thread_section(lines: &mut Vec<Line<'static>>, state: &Value, current_thread_id: &str) {
    let mut threads: Vec<&Value> = object_values(state, "threads").collect();
    threads.sort_by_key(|thread| str_field(thread, "id").unwrap_or_default());

    lines.push(section_header("threads"));
    if threads.is_empty() {
        lines.push(Line::from("  none"));
        return;
    }
    for thread in threads.into_iter().take(8) {
        let id = str_field(thread, "id").unwrap_or("?");
        let status = str_field(thread, "status").unwrap_or("unknown");
        let root = str_field(thread, "root_problem_id").unwrap_or("?");
        let actor = str_field(thread, "actor_session_id").unwrap_or("no-actor");
        let marker = if id == current_thread_id { "*" } else { " " };
        let runtime_state = loop_state_for_thread_status(status);
        let wait =
            active_wait_summary_for_thread(state, id).unwrap_or_else(|| "wait -".to_string());
        lines.push(Line::from(format!(
            "{marker} {id} [{status}] runtime {runtime_state} root {root} actor {actor} {wait}"
        )));
    }
}

fn push_wait_handle_section(lines: &mut Vec<Line<'static>>, state: &Value) {
    lines.push(section_header("waits / handles"));
    let mut rows = Vec::new();
    for wait in object_values(state, "waits") {
        let id = str_field(wait, "id").unwrap_or("?");
        let status = str_field(wait, "status").unwrap_or("unknown");
        let thread = str_field(wait, "thread_id").unwrap_or("?");
        rows.push(format!("  wait {id} [{status}] thread {thread}"));
    }
    for handle in object_values(state, "handles") {
        let id = str_field(handle, "id").unwrap_or("?");
        let status = str_field(handle, "status").unwrap_or("unknown");
        let kind = str_field(handle, "kind").unwrap_or("unknown");
        let target = str_field(handle, "target_thread_id").unwrap_or("-");
        rows.push(format!(
            "  handle {id} [{status}] kind {kind} target {target}"
        ));
    }
    if rows.is_empty() {
        lines.push(Line::from("  none"));
    } else {
        for row in rows.into_iter().take(8) {
            lines.push(Line::from(row));
        }
    }
}

fn push_passive_section(lines: &mut Vec<Line<'static>>, state: &Value) {
    lines.push(section_header("passive inbox"));
    let mut inbox: Vec<&Value> = array_values(state, "inbox")
        .filter(|item| str_field(item, "status") == Some("inboxed"))
        .collect();
    inbox.sort_by_key(|item| str_field(item, "created_at").unwrap_or_default());
    if inbox.is_empty() {
        lines.push(Line::from("  none"));
    } else {
        for item in inbox.into_iter().rev().take(4) {
            let id = str_field(item, "id").unwrap_or("?");
            let event = str_field(item, "passive_event_id").unwrap_or("?");
            let thread = str_field(item, "target_thread_id").unwrap_or("-");
            let summary = str_field(item, "summary").unwrap_or("");
            let mut metadata = Vec::new();
            if let Some(source) = str_field(item, "source") {
                metadata.push(format!("source {source}"));
            }
            if let Some(condition) = str_field(item, "condition") {
                metadata.push(format!("condition {condition}"));
            }
            if let Some(payload_ref) = str_field(item, "payload_ref") {
                metadata.push(format!("payload {payload_ref}"));
            }
            if let Some(dedupe_key) = str_field(item, "dedupe_key") {
                metadata.push(format!("dedupe {dedupe_key}"));
            }
            let metadata = if metadata.is_empty() {
                String::new()
            } else {
                format!(" {}", metadata.join(" "))
            };
            lines.push(Line::from(format!(
                "  {id} event {event} thread {thread}{metadata} :: {summary}"
            )));
        }
    }

    lines.push(section_header("passive events"));
    let mut events: Vec<&Value> = array_values(state, "passive_events").collect();
    events.sort_by_key(|event| str_field(event, "created_at").unwrap_or_default());
    if events.is_empty() {
        lines.push(Line::from("  none"));
        return;
    }
    for event in events.into_iter().rev().take(4) {
        let id = str_field(event, "id").unwrap_or("?");
        let kind = str_field(event, "kind").unwrap_or("?");
        let status = str_field(event, "status").unwrap_or("unknown");
        let thread = str_field(event, "target_thread_id").unwrap_or("-");
        let summary = str_field(event, "summary").unwrap_or("");
        lines.push(Line::from(format!(
            "  {id} [{status}] {kind} thread {thread} :: {summary}"
        )));
    }
}

fn push_event_section(lines: &mut Vec<Line<'static>>, state: &Value) {
    lines.push(section_header("recent events"));
    let mut events = Vec::new();
    if let Some(items) = state.get("events").and_then(Value::as_array) {
        events.extend(items.iter());
    }
    if let Some(items) = state.get("io_events").and_then(Value::as_array) {
        events.extend(items.iter());
    }
    events.sort_by_key(|event| str_field(event, "created_at").unwrap_or_default());
    if events.is_empty() {
        lines.push(Line::from("  none"));
        return;
    }
    for event in events.into_iter().rev().take(6) {
        let kind = str_field(event, "kind").unwrap_or("event");
        let entity = str_field(event, "entity_id")
            .or_else(|| str_field(event, "thread_id"))
            .or_else(|| str_field(event, "handle_id"))
            .unwrap_or("-");
        let message = str_field(event, "message").unwrap_or("");
        lines.push(Line::from(format!("  {kind} {entity} {message}")));
    }
}

fn section_header(title: &'static str) -> Line<'static> {
    Line::from(vec![Span::styled(
        title,
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    )])
}

#[derive(Debug)]
struct NextSummary {
    disposition: &'static str,
    action: &'static str,
    target: Option<String>,
}

fn derive_next(state: &Value, current_thread_id: &str) -> NextSummary {
    if let Some(inbox_id) = pending_inbox_for_thread(state, current_thread_id) {
        return NextSummary {
            disposition: "runnable",
            action: "handle-inbox",
            target: Some(inbox_id.to_string()),
        };
    }

    for wait in object_values(state, "waits") {
        if str_field(wait, "thread_id") == Some(current_thread_id)
            && str_field(wait, "status").is_some_and(|status| status != "consumed")
        {
            let action = if wait_has_passive_handle(state, wait) {
                "wait_io"
            } else {
                "wait_thread"
            };
            return NextSummary {
                disposition: "io_wait",
                action,
                target: str_field(wait, "id").map(ToString::to_string),
            };
        }
    }

    let root_problem_id = state
        .get("threads")
        .and_then(|threads| threads.get(current_thread_id))
        .and_then(|thread| str_field(thread, "root_problem_id"))
        .or_else(|| str_field(state, "root_problem_id"))
        .or_else(|| str_field(state, "root_id"))
        .unwrap_or("P000");
    derive_next_for_problem(state, current_thread_id, root_problem_id).unwrap_or(NextSummary {
        disposition: "terminal",
        action: "none",
        target: None,
    })
}

fn pending_inbox_for_thread<'a>(state: &'a Value, current_thread_id: &str) -> Option<&'a str> {
    state.get("inbox")?.as_array()?.iter().find_map(|item| {
        (str_field(item, "status") == Some("inboxed")
            && str_field(item, "target_thread_id").is_none_or(|target| target == current_thread_id))
        .then(|| str_field(item, "id"))
        .flatten()
    })
}

fn derive_next_for_problem(
    state: &Value,
    current_thread_id: &str,
    problem_id: &str,
) -> Option<NextSummary> {
    let problem = state.get("problems")?.get(problem_id)?;
    if str_field(problem, "owner_thread_id") != Some(current_thread_id)
        || str_field(problem, "status") == Some("done")
    {
        return None;
    }

    for child_id in string_array(problem, "child_problem_ids") {
        let Some(child) = state
            .get("problems")
            .and_then(|problems| problems.get(child_id))
        else {
            continue;
        };
        if let Some(thread) = thread_for_root_problem(state, child_id)
            && str_field(thread, "id") != Some(current_thread_id)
        {
            match str_field(thread, "status") {
                Some("done") if str_field(thread, "joined_at").is_some() => continue,
                Some("done") => {
                    return Some(NextSummary {
                        disposition: "runnable",
                        action: "join-thread",
                        target: Some(child_id.to_string()),
                    });
                }
                Some("failed") | Some("cancelled") => {
                    return Some(NextSummary {
                        disposition: "runnable",
                        action: "recover-thread",
                        target: Some(child_id.to_string()),
                    });
                }
                _ => {
                    return Some(NextSummary {
                        disposition: "io_wait",
                        action: "wait-thread",
                        target: Some(child_id.to_string()),
                    });
                }
            }
        }
        if str_field(child, "status") == Some("done") {
            continue;
        }
        if str_field(child, "owner_thread_id") == Some(current_thread_id) {
            return Some(NextSummary {
                disposition: "runnable",
                action: "spawn-thread",
                target: Some(child_id.to_string()),
            });
        }
    }

    for followup_id in string_array(problem, "followup_problem_ids") {
        let Some(followup) = state
            .get("problems")
            .and_then(|problems| problems.get(followup_id))
        else {
            continue;
        };
        if str_field(followup, "owner_thread_id") == Some(current_thread_id) {
            if str_field(followup, "status") == Some("done") {
                continue;
            }
            if let Some(next) = derive_next_for_problem(state, current_thread_id, followup_id) {
                return Some(next);
            }
        } else if let Some(thread) = thread_for_root_problem(state, followup_id) {
            match str_field(thread, "status") {
                Some("done") if str_field(thread, "joined_at").is_some() => continue,
                Some("done") => {
                    return Some(NextSummary {
                        disposition: "runnable",
                        action: "join-thread",
                        target: Some(followup_id.to_string()),
                    });
                }
                Some("failed") | Some("cancelled") => {
                    return Some(NextSummary {
                        disposition: "runnable",
                        action: "recover-thread",
                        target: Some(followup_id.to_string()),
                    });
                }
                _ => {
                    return Some(NextSummary {
                        disposition: "io_wait",
                        action: "wait-thread",
                        target: Some(followup_id.to_string()),
                    });
                }
            }
        }
    }

    let problem_id = str_field(problem, "id").unwrap_or("?");
    let ticket_id =
        str_field(problem, "ticket_id").or_else(|| first_string_in_array(problem, "ticket_ids"));
    let action = ticket_id
        .and_then(|id| {
            state
                .get("tickets")
                .and_then(|tickets| tickets.get(id))
                .map(|ticket| ticket_action(state, ticket))
        })
        .unwrap_or("create-solution-ticket");

    Some(NextSummary {
        disposition: "runnable",
        action,
        target: Some(match ticket_id {
            Some(ticket_id) => format!("{problem_id}/{ticket_id}"),
            None => problem_id.to_string(),
        }),
    })
}

fn ticket_action(state: &Value, ticket: &Value) -> &'static str {
    match str_field(ticket, "status") {
        Some("defined") => "classify-ticket",
        Some("classified") => match str_field(ticket, "classification") {
            Some("split") => "split-ticket",
            Some("one_go") => "execute-ticket",
            _ => "classify-ticket",
        },
        Some("executing") => "record-result",
        Some("splitting") => {
            let ticket_id = str_field(ticket, "id").unwrap_or_default();
            if child_problem_ids_from_ticket(state, ticket_id)
                .next()
                .is_some()
            {
                "record-result"
            } else {
                "split-ticket"
            }
        }
        Some("done") => "check-success",
        _ => "define-ticket",
    }
}

fn child_problem_ids_from_ticket<'a>(
    state: &'a Value,
    ticket_id: &'a str,
) -> impl Iterator<Item = &'a str> {
    object_values(state, "problems").filter_map(move |problem| {
        (str_field(problem, "created_from_ticket_id") == Some(ticket_id))
            .then(|| str_field(problem, "id"))
            .flatten()
    })
}

fn thread_for_root_problem<'a>(state: &'a Value, problem_id: &str) -> Option<&'a Value> {
    object_values(state, "threads")
        .find(|thread| str_field(thread, "root_problem_id") == Some(problem_id))
}

fn color_for_disposition(disposition: &str) -> Color {
    match disposition {
        "runnable" => Color::Green,
        "io_wait" => Color::Yellow,
        "terminal" => Color::DarkGray,
        _ => Color::White,
    }
}

fn loop_state_for_next(disposition: &str) -> &'static str {
    match disposition {
        "runnable" => "ready",
        "io_wait" => "parked_io",
        "terminal" => "terminal",
        _ => "unknown",
    }
}

fn loop_state_for_thread_status(status: &str) -> &'static str {
    match status {
        "running" => "running",
        "waiting_io" => "parked_io",
        "waiting_children" => "waiting_child",
        "done" => "terminal",
        "failed" => "failed",
        "cancelled" => "cancelled",
        _ => "unknown",
    }
}

fn object_len(value: &Value, field: &str) -> usize {
    value
        .get(field)
        .and_then(Value::as_object)
        .map(serde_json::Map::len)
        .unwrap_or(0)
}

fn active_wait_summary_for_thread(state: &Value, thread_id: &str) -> Option<String> {
    let wait = object_values(state, "waits").find(|wait| {
        str_field(wait, "thread_id") == Some(thread_id)
            && str_field(wait, "status").is_some_and(|status| status != "consumed")
    })?;
    let wait_id = str_field(wait, "id").unwrap_or("?");
    let wait_status = str_field(wait, "status").unwrap_or("unknown");
    let handle_summary = wait
        .get("handle_ids")
        .and_then(Value::as_array)
        .and_then(|ids| {
            ids.iter().filter_map(Value::as_str).find_map(|handle_id| {
                let handle = state.get("handles")?.get(handle_id)?;
                let kind = str_field(handle, "kind").unwrap_or("unknown");
                let status = str_field(handle, "status").unwrap_or("unknown");
                Some(format!("handle {handle_id} {kind}/{status}"))
            })
        })
        .unwrap_or_else(|| "handle -".to_string());
    Some(format!("wait {wait_id} [{wait_status}] {handle_summary}"))
}

fn array_len(value: &Value, field: &str) -> usize {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
}

fn object_values<'a>(value: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(field)
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|object| object.values())
}

fn array_values<'a>(value: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|array| array.iter())
}

fn wait_has_passive_handle(state: &Value, wait: &Value) -> bool {
    let Some(handle_ids) = wait.get("handle_ids").and_then(Value::as_array) else {
        return false;
    };
    handle_ids
        .iter()
        .filter_map(Value::as_str)
        .any(|handle_id| {
            state
                .get("handles")
                .and_then(|handles| handles.get(handle_id))
                .and_then(|handle| str_field(handle, "kind"))
                .is_some_and(|kind| matches!(kind, "user_input" | "timer" | "external_signal"))
        })
}

fn str_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn first_string_in_array<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_array)?
        .iter()
        .find_map(Value::as_str)
}

fn string_array<'a>(value: &'a Value, field: &str) -> impl Iterator<Item = &'a str> {
    value
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|array| array.iter())
        .filter_map(Value::as_str)
}

fn process_created_at(state: &Value) -> &str {
    state
        .get("process")
        .and_then(|process| str_field(process, "created_at"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_renders_placeholder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rendered = lines_to_string(lines_for_workspace(tmp.path(), 80));

        assert!(rendered.contains("Qunux Agent OS Cockpit"));
        assert!(rendered.contains("attachable runtime view"));
        assert!(rendered.contains("No Qunux process state found."));
    }

    #[test]
    fn fixture_state_renders_core_cockpit_sections() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join(".qunux/processes/QP-demo");
        fs::create_dir_all(&state_dir).expect("create state dir");
        fs::write(state_dir.join("closure.json"), fixture_state()).expect("write state");

        let rendered = lines_to_string(lines_for_workspace(tmp.path(), 100));

        assert!(rendered.contains("process QP-demo"));
        assert!(rendered.contains("main QT000  current QT000"));
        assert!(rendered.contains("tasks 4 (3 open)"));
        assert!(rendered.contains("threads 2 (1 running)"));
        assert!(rendered.contains("next runnable handle-inbox IN000"));
        assert!(rendered.contains("dispatch state"));
        assert!(rendered.contains("dispatch ready next handle-inbox IN000"));
        assert!(rendered.contains("* QT000 [running] runtime running"));
        assert!(rendered.contains("process mission"));
        assert!(rendered.contains("P000 [doing] owner QT000 ticket T000 :: Root task"));
        assert!(rendered.contains("active user tasks"));
        assert!(rendered.contains(
            "P002 [todo] owner QT000 ticket - :: Investigate OpenClaw source inbox IN000 event PE000"
        ));
        assert!(rendered.contains("open work tree"));
        assert!(rendered.contains("P003 [todo] owner QT000 ticket - :: Runtime follow-up"));
        assert!(rendered.contains("threads"));
        assert!(rendered.contains("waits / handles"));
        assert!(rendered.contains("passive events 2 | inbox 1 (1 pending)"));
        assert!(rendered.contains("passive inbox"));
        assert!(rendered.contains(
            "IN000 event PE000 thread QT000 source chat condition reply payload turn:1 dedupe msg-1 :: user replied"
        ));
        assert!(rendered.contains("PE001 [matched] timer thread QT000 :: timer fired"));
        assert!(rendered.contains("recent events"));
    }

    #[test]
    fn pushed_snapshot_is_preferred_over_persisted_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join(".qunux/processes/QP-disk");
        fs::create_dir_all(&state_dir).expect("create state dir");
        fs::write(state_dir.join("closure.json"), fixture_state()).expect("write state");
        let snapshot_state = serde_json::json!({
            "process_id": "QP-live",
            "root_id": "P900",
            "main_thread_id": "QT900",
            "problems": {},
            "tickets": {},
            "results": {},
            "checks": {},
            "threads": {},
            "waits": {},
            "handles": {}
        });
        let snapshot = QunuxCockpitSnapshot::new(
            "QP-live".to_string(),
            Some("QT900".to_string()),
            Some("runtime push".to_string()),
            snapshot_state,
        );

        let rendered = lines_to_string(lines_for_snapshot_or_workspace(
            tmp.path(),
            Some(&snapshot),
            100,
        ));

        assert!(rendered.contains("process QP-live"));
        assert!(rendered.contains("current QT900"));
        assert!(rendered.contains("state runtime push"));
        assert!(!rendered.contains("process QP-demo"));
    }

    #[test]
    fn persisted_processes_render_newest_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_state(tmp.path(), "QP-old", "2026-05-14T00:00:00Z", "Old root");
        write_state(tmp.path(), "QP-new", "2026-05-16T00:00:00Z", "New root");

        let rendered = lines_to_string(lines_for_workspace(tmp.path(), 100));

        let new_index = rendered.find("process QP-new").expect("new process");
        let old_index = rendered.find("process QP-old").expect("old process");
        assert!(new_index < old_index);
    }

    #[test]
    fn executing_ticket_renders_record_result_next() {
        let state = minimal_state(
            serde_json::json!({
                "P000": {
                    "id": "P000",
                    "title": "Root task",
                    "status": "doing",
                    "owner_thread_id": "QT000",
                    "ticket_id": "T000"
                }
            }),
            serde_json::json!({
                "T000": {
                    "id": "T000",
                    "problem_id": "P000",
                    "status": "executing",
                    "classification": "one_go"
                }
            }),
        );

        let rendered = render_snapshot_state(state);

        assert!(rendered.contains("next runnable record-result P000/T000"));
        assert!(rendered.contains("dispatch ready next record-result P000/T000"));
    }

    #[test]
    fn splitting_ticket_without_children_renders_split_ticket_next() {
        let state = minimal_state(
            serde_json::json!({
                "P000": {
                    "id": "P000",
                    "title": "Root task",
                    "status": "doing",
                    "owner_thread_id": "QT000",
                    "ticket_id": "T000"
                }
            }),
            serde_json::json!({
                "T000": {
                    "id": "T000",
                    "problem_id": "P000",
                    "status": "splitting",
                    "classification": "split"
                }
            }),
        );

        let rendered = render_snapshot_state(state);

        assert!(rendered.contains("next runnable split-ticket P000/T000"));
        assert!(rendered.contains("dispatch ready next split-ticket P000/T000"));
    }

    #[test]
    fn splitting_ticket_with_closed_child_renders_record_result_next() {
        let state = minimal_state(
            serde_json::json!({
                "P000": {
                    "id": "P000",
                    "title": "Root task",
                    "status": "doing",
                    "owner_thread_id": "QT000",
                    "ticket_id": "T000",
                    "child_problem_ids": ["P001"]
                },
                "P001": {
                    "id": "P001",
                    "title": "Closed child",
                    "status": "done",
                    "owner_thread_id": "QT000",
                    "parent_id": "P000",
                    "created_from_ticket_id": "T000"
                }
            }),
            serde_json::json!({
                "T000": {
                    "id": "T000",
                    "problem_id": "P000",
                    "status": "splitting",
                    "classification": "split"
                }
            }),
        );

        let rendered = render_snapshot_state(state);

        assert!(rendered.contains("next runnable record-result P000/T000"));
        assert!(rendered.contains("dispatch ready next record-result P000/T000"));
    }

    #[test]
    fn open_child_problem_renders_spawn_thread_next() {
        let state = minimal_state(
            serde_json::json!({
                "P000": {
                    "id": "P000",
                    "title": "Root task",
                    "status": "doing",
                    "owner_thread_id": "QT000",
                    "ticket_id": "T000",
                    "child_problem_ids": ["P001"]
                },
                "P001": {
                    "id": "P001",
                    "title": "Open child",
                    "status": "todo",
                    "owner_thread_id": "QT000",
                    "parent_id": "P000",
                    "created_from_ticket_id": "T000"
                }
            }),
            serde_json::json!({
                "T000": {
                    "id": "T000",
                    "problem_id": "P000",
                    "status": "splitting",
                    "classification": "split"
                }
            }),
        );

        let rendered = render_snapshot_state(state);

        assert!(rendered.contains("next runnable spawn-thread P001"));
        assert!(rendered.contains("dispatch ready next spawn-thread P001"));
    }

    #[test]
    fn waiting_io_thread_renders_parked_io_next_and_wait_rows() {
        let mut state = minimal_state(
            serde_json::json!({
                "P000": {
                    "id": "P000",
                    "title": "Root task",
                    "status": "doing",
                    "owner_thread_id": "QT000"
                }
            }),
            serde_json::json!({}),
        );
        state["threads"]["QT000"]["status"] = serde_json::json!("waiting_io");
        state["waits"] = serde_json::json!({
            "W000": {
                "id": "W000",
                "thread_id": "QT000",
                "status": "waiting",
                "handle_ids": ["H000"]
            }
        });
        state["handles"] = serde_json::json!({
            "H000": {
                "id": "H000",
                "kind": "user_input",
                "status": "pending",
                "target_thread_id": "QT000"
            }
        });

        let rendered = render_snapshot_state(state);

        assert!(rendered.contains("next io_wait wait_io W000"));
        assert!(rendered.contains("dispatch parked_io next wait_io W000"));
        assert!(rendered.contains(
            "* QT000 [waiting_io] runtime parked_io root P000 actor root-session wait W000 [waiting] handle H000 user_input/pending"
        ));
        assert!(rendered.contains("wait W000 [waiting] thread QT000"));
        assert!(rendered.contains("handle H000 [pending] kind user_input target QT000"));
    }

    fn render_snapshot_state(state: Value) -> String {
        let tmp = tempfile::tempdir().expect("tempdir");
        let snapshot = QunuxCockpitSnapshot::new(
            "QP-test".to_string(),
            Some("QT000".to_string()),
            Some("runtime push".to_string()),
            state,
        );
        lines_to_string(lines_for_snapshot_or_workspace(
            tmp.path(),
            Some(&snapshot),
            100,
        ))
    }

    fn minimal_state(problems: Value, tickets: Value) -> Value {
        serde_json::json!({
            "process_id": "QP-test",
            "root_id": "P000",
            "process": {
                "id": "QP-test",
                "root_actor_session_id": "root-session",
                "main_thread_id": "QT000",
                "created_at": "2026-05-16T00:00:00Z",
                "updated_at": "2026-05-16T00:00:00Z"
            },
            "main_thread_id": "QT000",
            "problems": problems,
            "tickets": tickets,
            "results": {},
            "checks": {},
            "threads": {
                "QT000": {
                    "id": "QT000",
                    "status": "running",
                    "root_problem_id": "P000",
                    "actor_session_id": "root-session"
                }
            },
            "waits": {},
            "handles": {},
            "passive_events": [],
            "inbox": [],
            "events": [],
            "io_events": []
        })
    }

    fn write_state(workspace: &Path, process_id: &str, created_at: &str, root_title: &str) {
        let state_dir = workspace.join(".qunux/processes").join(process_id);
        fs::create_dir_all(&state_dir).expect("create state dir");
        let state = serde_json::json!({
            "process_id": process_id,
            "root_id": "P000",
            "process": {
                "id": process_id,
                "root_actor_session_id": "root-session",
                "main_thread_id": "QT000",
                "created_at": created_at,
                "updated_at": created_at
            },
            "main_thread_id": "QT000",
            "problems": {
                "P000": {
                    "id": "P000",
                    "title": root_title,
                    "status": "todo",
                    "owner_thread_id": "QT000"
                }
            },
            "tickets": {},
            "results": {},
            "checks": {},
            "threads": {
                "QT000": {
                    "id": "QT000",
                    "status": "running",
                    "root_problem_id": "P000",
                    "actor_session_id": "root-session"
                }
            },
            "waits": {},
            "handles": {},
            "passive_events": [],
            "inbox": [],
            "events": [],
            "io_events": []
        });
        fs::write(state_dir.join("closure.json"), state.to_string()).expect("write state");
    }

    fn lines_to_string(lines: Vec<Line<'static>>) -> String {
        lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fixture_state() -> &'static str {
        r#"{
  "process_id": "QP-demo",
  "root_id": "P000",
  "main_thread_id": "QT000",
  "problems": {
    "P000": {
      "id": "P000",
      "title": "Root task",
      "status": "doing",
      "owner_thread_id": "QT000",
      "ticket_id": "T000"
    },
    "P001": {
      "id": "P001",
      "title": "Closed child",
      "status": "done",
      "owner_thread_id": "QT001"
    },
    "P002": {
      "id": "P002",
      "title": "Investigate OpenClaw",
      "status": "todo",
      "owner_thread_id": "QT000",
      "parent_id": "P000",
      "created_from_inbox_item_id": "IN000",
      "created_from_passive_event_id": "PE000",
      "created_from_user_task_kind": "user_input"
    },
    "P003": {
      "id": "P003",
      "title": "Runtime follow-up",
      "status": "todo",
      "owner_thread_id": "QT000",
      "parent_id": "P000",
      "created_from_ticket_id": "T000",
      "child_mode": "split"
    }
  },
  "tickets": {
    "T000": {
      "id": "T000",
      "problem_id": "P000",
      "status": "classified",
      "classification": "split"
    }
  },
  "results": {},
  "checks": {},
  "threads": {
    "QT000": {
      "id": "QT000",
      "status": "running",
      "root_problem_id": "P000",
      "actor_session_id": "root-session"
    },
    "QT001": {
      "id": "QT001",
      "status": "done",
      "root_problem_id": "P001",
      "actor_session_id": "child-session"
    }
  },
  "waits": {
    "W000": {
      "id": "W000",
      "status": "consumed",
      "thread_id": "QT000"
    }
  },
  "handles": {
    "H000": {
      "id": "H000",
      "kind": "child_thread",
      "status": "consumed",
      "target_thread_id": "QT001"
    }
  },
  "passive_events": [
    {
      "id": "PE000",
      "kind": "user_input",
      "status": "inboxed",
      "target_thread_id": "QT000",
      "summary": "user replied",
      "created_at": "2026-05-14T00:00:02Z"
    },
    {
      "id": "PE001",
      "kind": "timer",
      "status": "matched",
      "target_thread_id": "QT000",
      "summary": "timer fired",
      "created_at": "2026-05-14T00:00:03Z"
    }
  ],
  "inbox": [
    {
      "id": "IN000",
      "passive_event_id": "PE000",
      "target_thread_id": "QT000",
      "condition": "reply",
      "source": "chat",
      "summary": "user replied",
      "payload_ref": "turn:1",
      "dedupe_key": "msg-1",
      "status": "inboxed",
      "created_at": "2026-05-14T00:00:02Z"
    }
  ],
  "events": [
    {"created_at": "2026-05-14T00:00:00Z", "kind": "problem_created", "entity_id": "P000", "message": "root"}
  ],
  "io_events": [
    {"created_at": "2026-05-14T00:00:01Z", "kind": "child_thread_joined", "thread_id": "QT001", "message": "joined"}
  ]
}"#
    }
}
