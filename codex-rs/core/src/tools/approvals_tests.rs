use super::*;
use codex_protocol::approvals::NetworkPolicyAmendment;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;

#[test]
fn approval_resolution_rejects_denied_network_policy_amendment() {
    let resolution = ApprovalResolution {
        decision: ReviewDecision::NetworkPolicyAmendment {
            network_policy_amendment: NetworkPolicyAmendment {
                host: "denied.example.com".to_string(),
                action: NetworkPolicyRuleAction::Deny,
            },
        },
        source: ApprovalResolutionSource::User,
    };
    assert!(matches!(
        resolution.into_tool_result(),
        Err(ToolError::Rejected(rejection)) if rejection == "rejected by user"
    ));
}

#[test]
fn guardian_cwd_preserves_drive_shaped_local_posix_path() {
    let native_cwd = AbsolutePathBuf::try_from(std::path::PathBuf::from("/C:/workspace"))
        .expect("drive-shaped POSIX path should be absolute");
    let cwd = PathUri::from_abs_path(&native_cwd);

    assert_eq!(
        guardian_cwd(codex_exec_server::LOCAL_ENVIRONMENT_ID, cwd)
            .expect("local cwd should retain the host path convention"),
        native_cwd
    );
}

#[test]
fn guardian_cwd_rejects_foreign_remote_path() {
    let cwd = PathUri::parse("file:///C:/workspace").expect("valid Windows path URI");

    assert!(guardian_cwd(codex_exec_server::REMOTE_ENVIRONMENT_ID, cwd).is_err());
}

#[test]
fn monitor_metadata_survives_guardian_request_projection() {
    let cwd = test_path_buf("/tmp").abs();
    let monitor = CommandMonitorInfo {
        task_id: "bguardian".to_string(),
        description: "watch guardian command".to_string(),
        timeout_ms: 300_000,
        persistent: false,
    };
    let action = ApprovalAction::ExecCommand {
        id: "call-1".to_string(),
        environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        command: vec!["tail".to_string(), "-f".to_string(), "app.log".to_string()],
        monitor: Some(monitor.clone()),
        hook_command: "tail -f app.log".to_string(),
        cwd: PathUri::from_abs_path(&cwd),
        sandbox_permissions: SandboxPermissions::UseDefault,
        additional_permissions: None,
        justification: None,
        tty: false,
        proposed_execpolicy_amendment: None,
    };

    assert_eq!(
        action.into_guardian_request().expect("guardian request"),
        crate::guardian::GuardianApprovalRequest::ExecCommand {
            id: "call-1".to_string(),
            command: vec!["tail".to_string(), "-f".to_string(), "app.log".to_string()],
            monitor: Some(monitor),
            cwd,
            sandbox_permissions: SandboxPermissions::UseDefault,
            additional_permissions: None,
            justification: None,
            tty: false,
        }
    );
}
