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
    pub(crate) state_path: Option<String>,
    pub(crate) state: Value,
}

impl QunuxCockpitSnapshot {
    pub(crate) fn new(process_id: String, state_path: Option<String>, state: Value) -> Self {
        Self {
            process_id,
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
        Span::styled("native TUI", Style::default().fg(Color::DarkGray)),
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
    state_path: Option<String>,
    state: Value,
}

impl From<&QunuxCockpitSnapshot> for ProcessSnapshot {
    fn from(snapshot: &QunuxCockpitSnapshot) -> Self {
        Self {
            process_id: snapshot.process_id.clone(),
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
            state_path: Some(state_path.display().to_string()),
            state,
        });
    }
    processes.sort_by(|left, right| left.process_id.cmp(&right.process_id));
    processes
}

fn append_process_lines(lines: &mut Vec<Line<'static>>, process: &ProcessSnapshot) {
    let state = &process.state;
    let root_id = str_field(state, "root_id").unwrap_or("P000");
    let main_thread_id = str_field(state, "main_thread_id").unwrap_or("QT000");
    let problem_count = object_len(state, "problems");
    let ticket_count = object_len(state, "tickets");
    let result_count = object_len(state, "results");
    let check_count = object_len(state, "checks");
    let thread_count = object_len(state, "threads");
    let wait_count = object_len(state, "waits");
    let handle_count = object_len(state, "handles");
    let open_problem_count = object_values(state, "problems")
        .filter(|problem| str_field(problem, "status") != Some("done"))
        .count();
    let running_thread_count = object_values(state, "threads")
        .filter(|thread| str_field(thread, "status") == Some("running"))
        .count();
    let next = derive_next(state, main_thread_id);

    lines.push(Line::from(vec![
        Span::styled(
            format!("process {}", process.process_id),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  root {root_id}  main {main_thread_id}")),
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
    lines.push(Line::from(vec![
        Span::styled("next ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            next.disposition,
            Style::default().fg(color_for_disposition(next.disposition)),
        ),
        Span::raw(format!(
            " {} {}",
            next.action,
            next.target.unwrap_or_default()
        )),
    ]));
    push_problem_section(lines, state);
    push_thread_section(lines, state);
    push_wait_handle_section(lines, state);
    push_event_section(lines, state);
}

fn push_problem_section(lines: &mut Vec<Line<'static>>, state: &Value) {
    let mut open: Vec<&Value> = object_values(state, "problems")
        .filter(|problem| str_field(problem, "status") != Some("done"))
        .collect();
    open.sort_by_key(|problem| str_field(problem, "id").unwrap_or_default());

    lines.push(section_header("open tasks"));
    if open.is_empty() {
        lines.push(Line::from("  none"));
        return;
    }
    for problem in open.into_iter().take(8) {
        let id = str_field(problem, "id").unwrap_or("?");
        let title = str_field(problem, "title").unwrap_or("untitled");
        let status = str_field(problem, "status").unwrap_or("unknown");
        let owner = str_field(problem, "owner_thread_id").unwrap_or("?");
        let ticket = str_field(problem, "ticket_id")
            .or_else(|| first_string_in_array(problem, "ticket_ids"))
            .unwrap_or("-");
        lines.push(Line::from(format!(
            "  {id} [{status}] owner {owner} ticket {ticket} :: {title}"
        )));
    }
}

fn push_thread_section(lines: &mut Vec<Line<'static>>, state: &Value) {
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
        lines.push(Line::from(format!(
            "  {id} [{status}] root {root} actor {actor}"
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
        let target = str_field(handle, "target_thread_id").unwrap_or("-");
        rows.push(format!("  handle {id} [{status}] target {target}"));
    }
    if rows.is_empty() {
        lines.push(Line::from("  none"));
    } else {
        for row in rows.into_iter().take(8) {
            lines.push(Line::from(row));
        }
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

fn derive_next(state: &Value, main_thread_id: &str) -> NextSummary {
    for wait in object_values(state, "waits") {
        if str_field(wait, "thread_id") == Some(main_thread_id)
            && str_field(wait, "status").is_some_and(|status| status != "consumed")
        {
            return NextSummary {
                disposition: "io_wait",
                action: "wait_thread",
                target: str_field(wait, "id").map(ToString::to_string),
            };
        }
    }

    let mut problems: Vec<&Value> = object_values(state, "problems")
        .filter(|problem| {
            str_field(problem, "owner_thread_id") == Some(main_thread_id)
                && str_field(problem, "status") != Some("done")
        })
        .collect();
    problems.sort_by_key(|problem| str_field(problem, "id").unwrap_or_default());
    let Some(problem) = problems.first().copied() else {
        return NextSummary {
            disposition: "terminal",
            action: "none",
            target: None,
        };
    };

    let problem_id = str_field(problem, "id").unwrap_or("?");
    let ticket_id =
        str_field(problem, "ticket_id").or_else(|| first_string_in_array(problem, "ticket_ids"));
    let action = ticket_id
        .and_then(|id| state.get("tickets").and_then(|tickets| tickets.get(id)))
        .map(ticket_action)
        .unwrap_or("create-solution-ticket");

    NextSummary {
        disposition: "runnable",
        action,
        target: Some(match ticket_id {
            Some(ticket_id) => format!("{problem_id}/{ticket_id}"),
            None => problem_id.to_string(),
        }),
    }
}

fn ticket_action(ticket: &Value) -> &'static str {
    match str_field(ticket, "status") {
        Some("defined") => "classify-ticket",
        Some("classified") => match str_field(ticket, "classification") {
            Some("split") => "split-ticket",
            Some("one_go") => "execute-ticket",
            _ => "classify-ticket",
        },
        Some("executing") => "execute-ticket",
        Some("splitting") => "record-result",
        Some("done") => "check-success",
        _ => "define-ticket",
    }
}

fn color_for_disposition(disposition: &str) -> Color {
    match disposition {
        "runnable" => Color::Green,
        "io_wait" => Color::Yellow,
        "terminal" => Color::DarkGray,
        _ => Color::White,
    }
}

fn object_len(value: &Value, field: &str) -> usize {
    value
        .get(field)
        .and_then(Value::as_object)
        .map(serde_json::Map::len)
        .unwrap_or(0)
}

fn object_values<'a>(value: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(field)
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|object| object.values())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_renders_placeholder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rendered = lines_to_string(lines_for_workspace(tmp.path(), 80));

        assert!(rendered.contains("Qunux Agent OS Cockpit"));
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
        assert!(rendered.contains("tasks 2 (1 open)"));
        assert!(rendered.contains("threads 2 (1 running)"));
        assert!(rendered.contains("next runnable split-ticket P000/T000"));
        assert!(rendered.contains("open tasks"));
        assert!(rendered.contains("threads"));
        assert!(rendered.contains("waits / handles"));
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
            Some("runtime push".to_string()),
            snapshot_state,
        );

        let rendered = lines_to_string(lines_for_snapshot_or_workspace(
            tmp.path(),
            Some(&snapshot),
            100,
        ));

        assert!(rendered.contains("process QP-live"));
        assert!(rendered.contains("state runtime push"));
        assert!(!rendered.contains("process QP-demo"));
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
      "status": "consumed",
      "target_thread_id": "QT001"
    }
  },
  "events": [
    {"created_at": "2026-05-14T00:00:00Z", "kind": "problem_created", "entity_id": "P000", "message": "root"}
  ],
  "io_events": [
    {"created_at": "2026-05-14T00:00:01Z", "kind": "child_thread_joined", "thread_id": "QT001", "message": "joined"}
  ]
}"#
    }
}
