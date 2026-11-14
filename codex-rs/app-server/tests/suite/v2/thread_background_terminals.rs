use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CommandMonitorTerminationReason;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::ThreadBackgroundTerminalsListParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsListResponse;
use codex_app_server_protocol::ThreadBackgroundTerminalsTerminateParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsTerminateResponse;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_monitor_survives_turn_and_can_be_terminated_via_rpc() -> Result<()> {
    skip_if_remote!(Ok(()), "command monitors require a local executor");
    let server = create_mock_responses_server_sequence(vec![
        responses::sse(vec![
            responses::ev_response_created("monitor-response"),
            responses::ev_function_call(
                "monitor-call",
                "monitor",
                &json!({
                    "command": "sleep 60",
                    "description": "silent background monitor",
                    "persistent": true,
                })
                .to_string(),
            ),
            responses::ev_completed("monitor-response"),
        ]),
        create_final_assistant_message_sse_response("Monitor attached")?,
        create_final_assistant_message_sse_response("Monitor stopped")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .enable_feature(Feature::UnifiedExec)
        .with_approval_policy("never")
        .with_sandbox_mode("danger-full-access")
        .write(codex_home.path())?;
    // The existing initialize helper opts into experimental APIs.
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let ThreadStartResponse {
        thread,
        approval_policy,
        sandbox,
        ..
    } = app
        .start_thread(ThreadStartParams {
            cwd: Some(workspace.path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    assert_eq!(approval_policy, AskForApproval::Never);
    assert_eq!(sandbox, SandboxPolicy::DangerFullAccess);
    let TurnStartResponse { turn } = app
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "start a persistent monitor".to_string(),
                    text_elements: Vec::new(),
                }],
                cwd: Some(workspace.path().to_path_buf()),
                ..Default::default()
            },
        })
        .await?;
    let completed: TurnCompletedNotification = timeout(
        Duration::from_secs(10),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(completed.thread_id, thread.id);
    assert_eq!(completed.turn.id, turn.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);

    let list_params = ThreadBackgroundTerminalsListParams {
        thread_id: thread.id.clone(),
        cursor: None,
        limit: Some(10),
    };
    let listed: ThreadBackgroundTerminalsListResponse = app
        .request(|request_id| ClientRequest::ThreadBackgroundTerminalsList {
            request_id,
            params: list_params.clone(),
        })
        .await?;
    assert_eq!(listed.data.len(), 1);
    assert_eq!(listed.next_cursor, None);
    let terminal = &listed.data[0];
    assert_eq!(terminal.item_id, "monitor-call");
    assert!(!terminal.process_id.is_empty());
    let monitor = terminal.monitor.as_ref().expect("monitor metadata");
    assert_eq!(monitor.description, "silent background monitor");
    assert!(monitor.persistent);
    assert!(!monitor.task_id.is_empty());
    let output = terminal.output.as_ref().expect("monitor output snapshot");
    assert_eq!(output.tail, "");
    assert_eq!(output.bytes_total, 0);
    assert!(!output.truncated);

    let stopped: ThreadBackgroundTerminalsTerminateResponse = app
        .request(
            |request_id| ClientRequest::ThreadBackgroundTerminalsTerminate {
                request_id,
                params: ThreadBackgroundTerminalsTerminateParams {
                    thread_id: thread.id.clone(),
                    process_id: terminal.process_id.clone(),
                },
            },
        )
        .await?;
    assert!(stopped.terminated);
    let remaining: ThreadBackgroundTerminalsListResponse = app
        .request(|request_id| ClientRequest::ThreadBackgroundTerminalsList {
            request_id,
            params: list_params,
        })
        .await?;
    assert!(remaining.data.is_empty());

    timeout(Duration::from_secs(10), async {
        loop {
            let completed: ItemCompletedNotification =
                app.read_notification("item/completed").await?;
            if let ThreadItemPayload::CommandExecution {
                id,
                monitor: completed_monitor,
                monitor_termination_reason,
                ..
            } = completed.item.payload
                && id == terminal.item_id
            {
                assert_eq!(completed_monitor.as_ref(), Some(monitor));
                assert_eq!(
                    monitor_termination_reason,
                    Some(CommandMonitorTerminationReason::UserStopped)
                );
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await??;
    // The explicit stop notification wakes an idle turn; observe its completion too.
    let stop_turn: TurnCompletedNotification = timeout(
        Duration::from_secs(10),
        app.read_notification("turn/completed"),
    )
    .await??;
    assert_eq!(stop_turn.thread_id, thread.id);
    assert_ne!(stop_turn.turn.id, turn.id);
    assert_eq!(stop_turn.turn.status, TurnStatus::Completed);
    Ok(())
}
