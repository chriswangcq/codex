use super::super::delivery::monitor_completion_summary;
use super::super::worker::ingest_stdout;
use super::*;
use pretty_assertions::assert_eq;

#[test]
fn physical_lines_are_trimmed_and_blank_lines_are_skipped() {
    let mut framer = PhysicalLineFramer::default();

    assert_eq!(framer.push(b"  first  \n\r\n sec"), vec!["first"]);
    assert_eq!(framer.push(b"ond \r\n"), vec!["second"]);
    assert_eq!(framer.finish(), None);
}

#[test]
fn whitespace_only_stdout_counts_as_produced_output_without_emitting_an_event() {
    let mut lines = PhysicalLineFramer::default();
    let mut batch = MonitorBatch::default();
    let mut produced_output = false;

    ingest_stdout(
        b"  \t\n",
        &mut lines,
        &mut batch,
        &mut produced_output,
        Instant::now(),
    );

    assert!(produced_output);
    assert_eq!(batch.take(), None);
    assert_eq!(
        monitor_completion_summary(false, produced_output, Some(0), None),
        " stream ended"
    );
}

#[test]
fn physical_line_trims_ecmascript_bom_whitespace() {
    let mut framer = PhysicalLineFramer::default();

    assert_eq!(
        framer.push("\u{FEFF}event\u{FEFF}\n".as_bytes()),
        vec!["event".to_string()]
    );
}

#[test]
fn physical_line_preserves_non_ecmascript_next_line_character() {
    let mut framer = PhysicalLineFramer::default();

    assert_eq!(
        framer.push("\u{0085}event\u{0085}\n".as_bytes()),
        vec!["\u{0085}event\u{0085}".to_string()]
    );
}

#[test]
fn physical_line_limit_counts_javascript_utf16_units() {
    let exactly_500_units = "😀".repeat(250);
    let over_500_units = "😀".repeat(251);

    assert_eq!(
        truncate_utf16(&exactly_500_units, MAX_LINE_UTF16_UNITS, false),
        exactly_500_units
    );
    assert_eq!(
        truncate_utf16(&over_500_units, MAX_LINE_UTF16_UNITS, false),
        format!("{}{}", "😀".repeat(250), TRUNCATION_SUFFIX)
    );
}

#[test]
fn pending_physical_line_is_bounded_and_marked_truncated() {
    let mut framer = PhysicalLineFramer::default();
    let mut bytes = vec![b'a'; MAX_PENDING_UTF16_UNITS + 100];
    bytes.push(b'\n');

    let lines = framer.push(&bytes);

    assert_eq!(framer.pending_utf16_units, 0);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0],
        format!("{}{}", "a".repeat(MAX_LINE_UTF16_UNITS), TRUNCATION_SUFFIX)
    );
}

#[test]
fn pending_line_keeps_the_last_utf16_units_across_split_code_points() {
    let mut framer = PhysicalLineFramer::default();
    let mut bytes = format!("HEAD{}TAIL", "😀".repeat(MAX_PENDING_UTF16_UNITS / 2)).into_bytes();
    let split = bytes.len() - "TAIL".len() - 2;
    let tail = bytes.split_off(split);

    assert!(framer.push(&bytes).is_empty());
    let mut tail_with_newline = tail;
    tail_with_newline.push(b'\n');
    let lines = framer.push(&tail_with_newline);

    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with('😀'));
    assert!(lines[0].ends_with(TRUNCATION_SUFFIX));
}

#[test]
fn batch_limit_counts_utf16_units_and_uses_batch_suffix() {
    let now = Instant::now();
    let mut batch = MonitorBatch::default();
    for _ in 0..7 {
        batch.push_line(&"x".repeat(MAX_LINE_UTF16_UNITS), now);
    }

    let framed = batch.take().expect("batch should contain output");
    let prefix = framed
        .strip_suffix(&format!("\n{TRUNCATION_SUFFIX}"))
        .expect("batch should carry its truncation suffix");
    assert_eq!(utf16_units(prefix), MAX_BATCH_UTF16_UNITS);
}

#[test]
fn batch_window_is_two_hundred_milliseconds() {
    let now = Instant::now();
    let mut batch = MonitorBatch::default();
    batch.push_line("event", now);

    assert_eq!(batch.deadline(), Some(now + Duration::from_millis(200)));
}

#[test]
fn token_bucket_starts_with_ten_tokens_and_refills_one_every_two_seconds() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let dispatch = limiter.submit(format!("event-{index}"), start, true);
        assert_eq!(dispatch.deliveries, vec![format!("event-{index}")]);
        assert_eq!(dispatch.stop_message, None);
    }

    let suppressed = limiter.submit("event-10".to_string(), start, true);
    assert_eq!(suppressed.deliveries, Vec::<String>::new());
    assert_eq!(suppressed.stop_message, None);

    let after_refill = limiter.submit(
        "event-11".to_string(),
        start + RATE_LIMIT_REFILL_INTERVAL,
        true,
    );
    assert_eq!(after_refill.deliveries, vec![suppression_message(1)]);
    assert_eq!(after_refill.stop_message, None);
}

#[test]
fn sustained_overload_stops_after_thirty_seconds() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let _ = limiter.submit(format!("initial-{index}"), start, true);
    }

    let mut stopped = None;
    for tick in 0..=151 {
        let now = start + Duration::from_millis(tick * 200);
        let dispatch = limiter.submit(format!("overload-{tick}"), now, true);
        if dispatch.stop_message.is_some() {
            stopped = dispatch.stop_message;
            break;
        }
    }

    let stopped = stopped.expect("continuous overload should stop the monitor");
    assert!(stopped.starts_with("[Monitor stopped — too much output ("));
    assert!(stopped.contains(" events suppressed over 30s)"));
    assert!(stopped.ends_with("Restart with a more selective source.]"));
}

#[test]
fn sustained_overload_does_not_stop_at_exactly_thirty_seconds() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let _ = limiter.submit(format!("initial-{index}"), start, true);
    }
    let _ = limiter.submit("suppressed".to_string(), start, true);

    let dispatch = limiter.submit(
        "exact-boundary".to_string(),
        start + OVERLOAD_STOP_AFTER,
        true,
    );

    assert_eq!(dispatch.stop_message, None);
}

#[test]
fn successful_refill_delivery_does_not_reset_continuous_overload() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let _ = limiter.submit(format!("initial-{index}"), start, true);
    }
    let _ = limiter.submit("suppressed".to_string(), start, true);

    let dispatch = limiter.submit(
        "delivered-after-refill".to_string(),
        start + Duration::from_secs(4),
        true,
    );

    assert_eq!(dispatch.deliveries.len(), 2);
    assert_eq!(limiter.overload_started, Some(start));
}

#[test]
fn quiet_gap_resets_overload_tracking_without_emitting_a_notice() {
    let start = Instant::now();
    let mut limiter = MonitorRateLimiter::new(start);
    for index in 0..RATE_LIMIT_CAPACITY {
        let _ = limiter.submit(format!("initial-{index}"), start, true);
    }
    let _ = limiter.submit("suppressed".to_string(), start, true);

    let dispatch = limiter.submit(
        "after-quiet".to_string(),
        start + OVERLOAD_QUIET_RESET_AFTER + Duration::from_nanos(1),
        true,
    );

    assert_eq!(dispatch.deliveries, vec!["after-quiet"]);
    assert_eq!(dispatch.stop_message, None);
    assert_eq!(limiter.overload_started, None);
    assert_eq!(limiter.suppressed_since_notice, 0);
}

#[test]
fn suppression_and_overload_messages_match_the_monitor_contract() {
    assert_eq!(
        suppression_message(7),
        "[7 events suppressed — output rate too high. Consider using TaskStop to restart this monitor with a more selective filter.]"
    );
    assert_eq!(
        overload_stop_message(42, Duration::from_secs(31)),
        "[Monitor stopped — too much output (42 events suppressed over 31s). Restart with a more selective source.]"
    );
}
