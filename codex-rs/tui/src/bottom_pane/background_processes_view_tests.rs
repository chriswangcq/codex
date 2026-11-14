use super::*;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::unbounded_channel;
use uuid::Uuid;

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

fn next_monitor_request(rx: &mut UnboundedReceiver<AppEvent>) -> BackgroundMonitorOutputRequest {
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

    assert!(
        !view.pre_draw_tick(
            now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT - Duration::from_millis(1)
        )
    );
    assert!(rx.try_recv().is_err());

    assert!(view.pre_draw_tick(now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT));
    assert_eq!(view.monitor_poll_lifecycle, MonitorPollLifecycle::Suspended);
    assert!(rx.try_recv().is_err());
    let rendered = render_view(&view, 70);
    assert!(rendered.contains("Output unavailable"));
    assert!(rendered.contains("refresh paused"));
    assert!(
        !view.pre_draw_tick(
            now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT + Duration::from_secs(30)
        )
    );
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
    assert!(
        !view.pre_draw_tick(
            now + BACKGROUND_MONITOR_OUTPUT_REQUEST_TIMEOUT + Duration::from_secs(30)
        )
    );
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
    let (mut view, mut rx) = view_with_rx_at(vec![monitor_item_at(now), shell_item_at(now)], now);

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
