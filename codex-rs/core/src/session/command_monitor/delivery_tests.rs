use super::*;
use pretty_assertions::assert_eq;

#[test]
fn natural_no_output_summary_includes_zero_exit_code() {
    assert_eq!(
        monitor_completion_summary(false, false, Some(0), None),
        " ended without producing output (exit 0)"
    );
}

#[test]
fn natural_output_summary_omits_zero_exit_code() {
    assert_eq!(
        monitor_completion_summary(false, true, Some(0), None),
        " stream ended"
    );
    assert_eq!(
        monitor_completion_summary(true, true, Some(7), None),
        " script failed (exit 7)"
    );
}

#[test]
fn output_delta_uses_turn_id_and_keeps_tool_use_id_in_payload() {
    let event = monitor_output_delta_event("turn-123", "call-456", "framed");

    assert_eq!(event.id, "turn-123");
    let EventMsg::ExecCommandOutputDelta(delta) = event.msg else {
        panic!("expected output delta");
    };
    assert_eq!(delta.call_id, "call-456");
    assert_eq!(delta.stream, ExecOutputStream::Stdout);
    assert_eq!(delta.chunk, b"framed");
}
