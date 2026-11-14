use super::monitor_lifecycle::MONITOR_WORKER_SHUTDOWN_GRACE_PERIOD;
use super::*;
use crate::tools::handlers::unified_exec::preparation::ExecCommandLifetime;
use crate::tools::handlers::unified_exec::preparation::ParsedExecCommand;
use crate::unified_exec::TerminateMonitorResult;
use crate::unified_exec::clamp_yield_time;
use codex_exec_server::ExecOutputStream;
use codex_network_proxy::ManagedNetworkSandboxContext;
use core_test_support::assert_regex_match;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use tokio::sync::Notify;
use tokio::time::Duration;
use tokio::time::Instant;

#[test]
fn unified_exec_env_injects_defaults() {
    let env = apply_unified_exec_env(HashMap::new());
    let expected = HashMap::from([
        ("NO_COLOR".to_string(), "1".to_string()),
        ("TERM".to_string(), "dumb".to_string()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("COLORTERM".to_string(), String::new()),
        ("PAGER".to_string(), "cat".to_string()),
        ("GIT_PAGER".to_string(), "cat".to_string()),
        ("GH_PAGER".to_string(), "cat".to_string()),
        ("CODEX_CI".to_string(), "1".to_string()),
    ]);

    assert_eq!(env, expected);
}

#[test]
fn unified_exec_env_overrides_existing_values() {
    let mut base = HashMap::new();
    base.insert("NO_COLOR".to_string(), "0".to_string());
    base.insert("PATH".to_string(), "/usr/bin".to_string());

    let env = apply_unified_exec_env(base);

    assert_eq!(env.get("NO_COLOR"), Some(&"1".to_string()));
    assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
}

#[tokio::test]
async fn deterministic_process_ids_are_not_reused_after_release() {
    let manager = UnifiedExecProcessManager::default();
    let first = manager.allocate_process_id().await;
    manager.release_process_id(first).await;
    let second = manager.allocate_process_id().await;

    assert_eq!((first, second), (1000, 1001));
}

#[tokio::test]
async fn deterministic_process_ids_are_not_reused_after_removal() {
    let manager = UnifiedExecProcessManager::default();
    let first = manager.allocate_process_id().await;
    let _removed = manager.process_store.lock().await.remove(first);
    let second = manager.allocate_process_id().await;

    assert_eq!((first, second), (1000, 1001));
}

#[cfg(unix)]
#[tokio::test]
async fn deterministic_process_ids_are_not_reused_after_registered_process_stops() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        CancellationToken::new(),
        "call-registered-id".to_string(),
    );
    let manager = UnifiedExecProcessManager::default();
    let process_id = manager.allocate_process_id().await;
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let fixture = test_process_entry(
        process_id,
        Arc::clone(&process),
        ProcessPurpose::Terminal,
        &turn,
    );
    start_streaming_output(&process, &context, /*emit_output_deltas*/ false);
    manager
        .store_process(
            Arc::clone(&process),
            &context,
            &["test command".to_string()],
            fixture.hook_command,
            fixture.cwd,
            fixture.environment_id,
            fixture.permissions,
            /*plugin_attribution*/ None,
            Instant::now(),
            process_id,
            /*tty*/ false,
            /*network_approval*/ None,
            /*network_denial_monitor*/ None,
            /*metrics_sidecar*/ None,
            Arc::clone(&process.output_handles().output_buffer),
            Arc::new(AtomicBool::new(false)),
            ProcessPurpose::Terminal,
            /*monitor_done*/ None,
        )
        .await;
    assert!(manager.resolve_terminal(process_id).await.is_ok());

    assert!(manager.terminate_process(process_id).await);

    assert!(manager.process_store.lock().await.processes.is_empty());
    assert_eq!(manager.allocate_process_id().await, process_id + 1);
}

#[test]
fn env_overlay_for_exec_server_keeps_runtime_changes_only() {
    let local_policy_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/client-path".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        (
            CODEX_VERSION_ENV_VAR.to_string(),
            "client-version".to_string(),
        ),
        (
            CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
            "current-profile".to_string(),
        ),
        (
            codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
            "1".to_string(),
        ),
    ]);
    let request_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/sandbox-path".to_string()),
        ("OpenAI_Federation_Rule_Id".to_string(), "rule".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
        (
            CODEX_VERSION_ENV_VAR.to_string(),
            "client-version".to_string(),
        ),
        (
            CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
            "current-profile".to_string(),
        ),
        (
            codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
            "1".to_string(),
        ),
        (
            "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
            "1".to_string(),
        ),
    ]);

    assert_eq!(
        env_overlay_for_exec_server(&request_env, &local_policy_env),
        HashMap::from([
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                CODEX_VERSION_ENV_VAR.to_string(),
                "client-version".to_string()
            ),
            (
                CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
                "current-profile".to_string(),
            ),
            (
                codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
                "1".to_string(),
            ),
            (
                "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
                "1".to_string()
            ),
        ])
    );
}

#[test]
fn exec_env_policy_excludes_non_inheritable_and_runtime_variables() {
    let policy = ShellEnvironmentPolicy {
        r#set: HashMap::from([
            ("codex_version".to_string(), "stale-version".to_string()),
            (
                "codex_permission_profile".to_string(),
                "stale-profile".to_string(),
            ),
            (
                "openai_identity_token_file".to_string(),
                "/run/identity-token".to_string(),
            ),
            (
                "codex_apply_patch_preserve_line_endings".to_string(),
                "1".to_string(),
            ),
            (
                "codex_plugin_metrics_output".to_string(),
                "/stale/sidecar".to_string(),
            ),
            ("KEEP".to_string(), "value".to_string()),
        ]),
        ..Default::default()
    };

    assert_eq!(
        exec_env_policy_from_shell_policy(&policy),
        codex_exec_server::ExecEnvPolicy {
            inherit: policy.inherit,
            ignore_default_excludes: policy.ignore_default_excludes,
            exclude: vec![
                CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
                CODEX_VERSION_ENV_VAR.to_string(),
                codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
                PLUGIN_METRICS_OUTPUT_ENV_VAR.to_string(),
            ],
            r#set: HashMap::from([("KEEP".to_string(), "value".to_string())]),
            include_only: Vec::new(),
        }
    );
}

#[test]
fn exec_server_params_use_path_uri_and_env_policy_overlay_contract() {
    let cwd: codex_utils_absolute_path::AbsolutePathBuf = std::env::current_dir()
        .expect("current dir")
        .try_into()
        .expect("absolute path");
    let permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    let managed_network = ManagedNetworkSandboxContext {
        loopback_ports: vec![43123],
        allow_local_binding: false,
        ..Default::default()
    };
    let mut request = ExecRequest {
        command: vec!["bash".to_string(), "-lc".to_string(), "true".to_string()],
        cwd: cwd.clone().into(),
        env: HashMap::from([
            ("HOME".to_string(), "/client-home".to_string()),
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:43123".to_string(),
            ),
            ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string()),
            (
                "SSL_CERT_FILE".to_string(),
                "/client/custom-ca.pem".to_string(),
            ),
        ]),
        exec_server_env_config: Some(ExecServerEnvConfig {
            policy: codex_exec_server::ExecEnvPolicy {
                inherit: codex_protocol::config_types::ShellEnvironmentPolicyInherit::Core,
                ignore_default_excludes: false,
                exclude: Vec::new(),
                r#set: HashMap::new(),
                include_only: Vec::new(),
            },
            local_policy_env: HashMap::from([
                ("HOME".to_string(), "/client-home".to_string()),
                ("PATH".to_string(), "/client-path".to_string()),
                (
                    "HTTP_PROXY".to_string(),
                    "http://127.0.0.1:43123".to_string(),
                ),
                ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string()),
                (
                    "SSL_CERT_FILE".to_string(),
                    "/client/custom-ca.pem".to_string(),
                ),
            ]),
        }),
        exec_server_shell_snapshot: None,
        network: None,
        network_environment_id: None,
        expiration: crate::exec::ExecExpiration::DefaultTimeout,
        capture_policy: crate::exec::ExecCapturePolicy::ShellTool,
        sandbox: codex_sandboxing::SandboxType::None,
        windows_sandbox_policy_cwd: cwd.clone().into(),
        windows_sandbox_workspace_roots: vec![cwd],
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        permission_profile: permission_profile.clone(),
        windows_sandbox_filesystem_overrides: None,
        arg0: None,
        exec_server_sandbox: None,
        exec_server_enforce_managed_network: true,
        exec_server_managed_network: Some(managed_network.clone()),
        exec_server_network_proxy: None,
    };

    let proxy_settings_mode = codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve;
    let params_for_request = |request: &ExecRequest| {
        exec_server_params_for_request(
            /*process_id*/ 123,
            request,
            /*tool_ctx*/ None,
            proxy_settings_mode,
            /*tty*/ true,
        )
    };
    let params = params_for_request(&request);

    assert_regex_match(
        r"^123-[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$",
        params.process_id.as_str(),
    );
    assert_ne!(params.process_id, params_for_request(&request).process_id);
    assert_eq!(params.metadata, None);
    assert_eq!(params.cwd, request.cwd);
    assert!(params.enforce_managed_network);
    assert_eq!(params.managed_network, Some(managed_network));
    assert!(params.env_policy.is_some());
    assert_eq!(
        params.env,
        HashMap::from([
            ("PATH".to_string(), "/sandbox-path".to_string()),
            ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:43123".to_string(),
            ),
            ("CODEX_NETWORK_PROXY_ACTIVE".to_string(), "1".to_string(),),
        ])
    );
    request.exec_server_shell_snapshot = Some(codex_exec_server::ShellSnapshotRequest {
        scope_id: "attachment-1".to_string(),
        shell: codex_exec_server::ShellInfo {
            name: "bash".to_string(),
            path: "/bin/bash".to_string(),
        },
    });
    let mut snapshot_env = params.env;
    snapshot_env.remove("PATH");
    assert_eq!(params_for_request(&request).env, snapshot_env);
    request.exec_server_shell_snapshot = None;

    request.exec_server_sandbox = Some(
        codex_exec_server::FileSystemSandboxContext::from_permission_profile(
            permission_profile,
            request.cwd.clone(),
        ),
    );
    let first = params_for_request(&request);
    let second = params_for_request(&request);
    assert_eq!(
        first
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.windows_sandbox_proxy_settings_mode),
        Some(codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve)
    );
    assert!(first.process_id.as_str().starts_with("123-"));
    assert!(second.process_id.as_str().starts_with("123-"));
    assert_ne!(first.process_id, second.process_id);
}

#[cfg(windows)]
#[test]
fn initial_exec_yield_time_uses_windows_floor() {
    let above_max_yield_time_ms = crate::unified_exec::MAX_YIELD_TIME_MS + 1;

    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 1_000),
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    );
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 2_000),
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    );
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 5_000),
        crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS
    );
    assert_eq!(clamp_yield_time(/*yield_time_ms*/ 10_000), 10_000);
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ above_max_yield_time_ms),
        crate::unified_exec::MAX_YIELD_TIME_MS
    );
}

#[cfg(not(windows))]
#[test]
fn initial_exec_yield_time_has_no_platform_floor() {
    assert_eq!(clamp_yield_time(/*yield_time_ms*/ 1_000), 1_000);
    assert_eq!(
        clamp_yield_time(/*yield_time_ms*/ 1),
        crate::unified_exec::MIN_YIELD_TIME_MS
    );
}

#[tokio::test]
async fn output_collection_stays_bounded_across_repeated_drains() {
    let chunks: [&[u8]; 4] = [b"01234567", b"89ABCDEF", b"ghijklmnopq", b"rs"];
    let output_buffer = Arc::new(tokio::sync::Mutex::new(OutputBuffers::<10>::default()));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let output = OutputHandles {
        output_buffer: Arc::clone(&output_buffer),
        output_notify: Arc::clone(&output_notify),
        monitor_capture: Arc::new(std::sync::Mutex::new(None)),
        monitor_capture_rx: Arc::new(std::sync::Mutex::new(None)),
        output_closed: Arc::clone(&output_closed),
        output_closed_notify: Arc::clone(&output_closed_notify),
        cancellation_token: cancellation_token.clone(),
        collection_finished: CancellationToken::new(),
        drain_policy: OutputDrainPolicy::BoundedAfterExit,
    };

    let collect = UnifiedExecProcessManager::collect_output_until_deadline(
        &output,
        /*pause_state*/ None,
        Instant::now() + Duration::from_secs(5),
    );
    let produce = async {
        for chunk in chunks {
            output_buffer
                .lock()
                .await
                .push_chunk(ExecOutputStream::Stdout, chunk);
            output_notify.notify_one();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if output_buffer.lock().await.pending.retained_bytes() == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("collector should drain each chunk");
        }

        output_closed.store(true, Ordering::Release);
        cancellation_token.cancel();
        output_closed_notify.notify_waiters();
        output_notify.notify_waiters();
    };

    let (collected, ()) = tokio::join!(collect, produce);
    let mut expected = HeadTailBuffer::<10>::default();
    for chunk in chunks {
        expected.push_chunk(chunk);
    }
    assert_eq!(collected, expected);
    assert_eq!(output_buffer.lock().await.transcript, expected);
}

#[tokio::test]
async fn output_collection_preserves_omissions_from_drained_buffer() {
    let mut buffered_output = OutputBuffers::<10>::default();
    buffered_output.push_chunk(ExecOutputStream::Stdout, &[b'a'; 10]);
    buffered_output.push_chunk(ExecOutputStream::Stdout, b"overflow");
    let mut expected = HeadTailBuffer::<10>::default();
    expected.push_chunk(&[b'a'; 10]);
    expected.push_chunk(b"overflow");
    let output_buffer = Arc::new(tokio::sync::Mutex::new(buffered_output));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(true));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    cancellation_token.cancel();
    let output = OutputHandles {
        output_buffer,
        output_notify,
        monitor_capture: Arc::new(std::sync::Mutex::new(None)),
        monitor_capture_rx: Arc::new(std::sync::Mutex::new(None)),
        output_closed,
        output_closed_notify,
        cancellation_token,
        collection_finished: CancellationToken::new(),
        drain_policy: OutputDrainPolicy::BoundedAfterExit,
    };

    let collected = UnifiedExecProcessManager::collect_output_until_deadline(
        &output,
        /*pause_state*/ None,
        Instant::now() + Duration::from_secs(1),
    )
    .await;

    assert_eq!(collected, expected);
}

#[tokio::test]
async fn session_cleanup_removes_registered_monitor_archives() {
    let temp_dir = tempfile::tempdir().expect("create monitor archive directory");
    let output_file = temp_dir.path().join("b1234abcd.output");
    tokio::fs::write(&output_file, b"sensitive monitor output")
        .await
        .expect("write monitor archive");
    let manager = UnifiedExecProcessManager::default();
    let worker_done = CancellationToken::new();
    let done_on_drop = worker_done.clone();
    let done_guard = CancelTokenOnDrop(done_on_drop);
    let worker = tokio::spawn(async move { drop(done_guard) });
    register_monitor_cleanup_worker(
        &manager,
        "b1234abcd",
        output_file.clone(),
        worker_done,
        worker.abort_handle(),
    )
    .await;

    manager.cleanup_monitor_output_files().await;

    assert!(!output_file.exists());
    assert!(!temp_dir.path().exists());
}

#[tokio::test]
async fn session_cleanup_waits_for_monitor_worker_before_removing_archive() {
    let temp_dir = tempfile::tempdir().expect("create monitor archive directory");
    let output_file = temp_dir.path().join("b1234abcd.output");
    tokio::fs::write(&output_file, b"sensitive monitor output")
        .await
        .expect("write monitor archive");
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let worker_done = CancellationToken::new();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let done_on_drop = worker_done.clone();
    let done_guard = CancelTokenOnDrop(done_on_drop);
    let worker = tokio::spawn(async move {
        let _guard = done_guard;
        let _ = finish_rx.await;
    });
    register_monitor_cleanup_worker(
        &manager,
        "b1234abcd",
        output_file.clone(),
        worker_done,
        worker.abort_handle(),
    )
    .await;

    let cleanup = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.cleanup_monitor_output_files().await }
    });
    tokio::task::yield_now().await;
    assert!(output_file.exists());

    finish_tx
        .send(())
        .expect("worker completion signal should be delivered");
    cleanup.await.expect("cleanup task should complete");

    assert!(!output_file.exists());
    assert!(!temp_dir.path().exists());
}

struct CancelTokenOnDrop(CancellationToken);

impl Drop for CancelTokenOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn register_monitor_cleanup_worker(
    manager: &UnifiedExecProcessManager,
    task_id: &str,
    output_file: PathBuf,
    done: CancellationToken,
    abort_handle: tokio::task::AbortHandle,
) {
    let mut store = manager.process_store.lock().await;
    store
        .monitor_output_files
        .insert(task_id.to_string(), output_file.clone());
    store.monitor_output_dirs.insert(
        output_file
            .parent()
            .expect("monitor output should have a parent")
            .to_path_buf(),
    );
    store.monitor_workers.insert(
        task_id.to_string(),
        Arc::new(crate::unified_exec::MonitorWorkerControl { done, abort_handle }),
    );
}

#[tokio::test(start_paused = true)]
async fn session_cleanup_aborts_stuck_worker_before_deleting_archive() {
    let temp_dir = tempfile::tempdir().expect("create monitor archive directory");
    let output_file = temp_dir.path().join("b1234abcd.output");
    tokio::fs::write(&output_file, b"sensitive monitor output")
        .await
        .expect("write monitor archive");
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let worker_done = CancellationToken::new();
    let file_existed_when_worker_dropped = Arc::new(AtomicBool::new(false));
    let done_on_drop = worker_done.clone();
    let output_file_on_drop = output_file.clone();
    let observed_on_drop = Arc::clone(&file_existed_when_worker_dropped);
    struct OrderingGuard {
        done: CancellationToken,
        output_file: PathBuf,
        file_existed: Arc<AtomicBool>,
    }
    impl Drop for OrderingGuard {
        fn drop(&mut self) {
            self.file_existed
                .store(self.output_file.exists(), Ordering::Release);
            self.done.cancel();
        }
    }
    let ordering_guard = OrderingGuard {
        done: done_on_drop,
        output_file: output_file_on_drop,
        file_existed: observed_on_drop,
    };
    let worker = tokio::spawn(async move {
        let _guard = ordering_guard;
        std::future::pending::<()>().await;
    });
    register_monitor_cleanup_worker(
        &manager,
        "b1234abcd",
        output_file.clone(),
        worker_done,
        worker.abort_handle(),
    )
    .await;

    let cleanup = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.cleanup_monitor_output_files().await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(MONITOR_WORKER_SHUTDOWN_GRACE_PERIOD).await;
    cleanup.await.expect("cleanup task should complete");

    assert!(
        file_existed_when_worker_dropped.load(Ordering::Acquire),
        "worker must be confirmed dropped before archive deletion"
    );
    assert!(!output_file.exists());
    assert!(!temp_dir.path().exists());
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn cleanup_retains_unconfirmed_monitor_but_removes_terminal_sibling() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let temp_dir = tempfile::tempdir().expect("create monitor archive directory");
    let live_output_file = temp_dir.path().join("blive.output");
    let done_output_file = temp_dir.path().join("bdone.output");
    tokio::fs::write(&live_output_file, b"live monitor output")
        .await
        .expect("write live monitor archive");
    tokio::fs::write(&done_output_file, b"completed monitor output")
        .await
        .expect("write completed monitor archive");

    let manager = Arc::new(UnifiedExecProcessManager::default());
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ true,
        )
        .await,
    );
    let (purpose, stop_rx) = test_monitor_purpose("blive");
    {
        let mut store = manager.process_store.lock().await;
        store.processes.insert(
            1000,
            test_process_entry(1000, Arc::clone(&process), purpose.clone(), &turn),
        );
        store
            .monitor_statuses
            .insert("blive".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "blive".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process,
                process_id: 1000,
                command: "live command".to_string(),
                purpose,
            },
        );
        store
            .monitor_statuses
            .insert("bdone".to_string(), MonitorTaskStatus::Completed);
    }

    let live_worker_done = CancellationToken::new();
    let live_done_on_drop = live_worker_done.clone();
    let live_worker = tokio::spawn(async move {
        let _guard = CancelTokenOnDrop(live_done_on_drop);
        std::future::pending::<()>().await;
    });
    register_monitor_cleanup_worker(
        &manager,
        "blive",
        live_output_file.clone(),
        live_worker_done.clone(),
        live_worker.abort_handle(),
    )
    .await;

    let done_worker_done = CancellationToken::new();
    let done_on_drop = done_worker_done.clone();
    let done_worker = tokio::spawn(async move { drop(CancelTokenOnDrop(done_on_drop)) });
    register_monitor_cleanup_worker(
        &manager,
        "bdone",
        done_output_file.clone(),
        done_worker_done,
        done_worker.abort_handle(),
    )
    .await;
    done_worker
        .await
        .expect("completed monitor worker should finish");

    let first_shutdown_attempt = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.terminate_all_processes().await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(crate::unified_exec::TERMINATE_CONFIRMATION_TIMEOUT).await;
    first_shutdown_attempt
        .await
        .expect("initial shutdown termination should finish");

    let cleanup = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.cleanup_monitor_output_files().await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(crate::unified_exec::TERMINATE_CONFIRMATION_TIMEOUT).await;
    cleanup.await.expect("archive cleanup should finish");

    assert_eq!(*stop_rx.borrow(), None);
    assert!(!live_worker_done.is_cancelled());
    assert!(live_output_file.exists());
    assert!(!done_output_file.exists());
    assert!(temp_dir.path().exists());
    {
        let store = manager.process_store.lock().await;
        assert!(store.processes.contains_key(&1000));
        assert_eq!(
            store.monitor_statuses.get("blive"),
            Some(&MonitorTaskStatus::Running)
        );
        assert!(store.monitor_tasks.contains_key("blive"));
        assert!(store.monitor_workers.contains_key("blive"));
        assert!(!store.monitor_workers.contains_key("bdone"));
        assert_eq!(
            store.monitor_output_files.get("blive"),
            Some(&live_output_file)
        );
        assert!(!store.monitor_output_files.contains_key("bdone"));
        assert!(store.monitor_output_dirs.contains(temp_dir.path()));
    }

    live_worker.abort();
    let _ = live_worker.await;
}

#[tokio::test]
async fn terminate_monitor_reports_every_terminal_status_as_not_running() {
    for status in [
        MonitorTaskStatus::Completed,
        MonitorTaskStatus::Failed,
        MonitorTaskStatus::Killed,
    ] {
        let manager = UnifiedExecProcessManager::default();
        manager
            .process_store
            .lock()
            .await
            .monitor_statuses
            .insert("b1234abcd".to_string(), status);

        assert!(matches!(
            manager.terminate_monitor("b1234abcd").await,
            TerminateMonitorResult::NotRunning(actual) if actual == status
        ));
    }
}

#[tokio::test]
async fn terminate_monitor_reports_unknown_task() {
    let manager = UnifiedExecProcessManager::default();

    assert!(matches!(
        manager.terminate_monitor("bmissing").await,
        TerminateMonitorResult::NotFound
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn late_reap_does_not_overwrite_task_stop_winner() {
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    manager
        .process_store
        .lock()
        .await
        .monitor_statuses
        .insert("bwinner".to_string(), MonitorTaskStatus::Killed);

    manager
        .reap_monitor("bwinner", &process, MonitorTaskStatus::Completed)
        .await;

    assert_eq!(
        manager
            .process_store
            .lock()
            .await
            .monitor_statuses
            .get("bwinner"),
        Some(&MonitorTaskStatus::Killed)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn completion_claim_distinguishes_pending_stop_and_can_retry() {
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    let (purpose, _stop_rx) = test_monitor_purpose("bclaimretry");
    {
        let mut store = manager.process_store.lock().await;
        store
            .monitor_statuses
            .insert("bclaimretry".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "bclaimretry".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process: Arc::clone(&process),
                process_id: 1000,
                command: "claim retry command".to_string(),
                purpose,
            },
        );
    }
    let stop_guard = process.begin_monitor_stop();

    assert_eq!(
        manager
            .claim_monitor_completion("bclaimretry", &process, MonitorTaskStatus::Completed,)
            .await,
        MonitorCompletionClaim::StopPending
    );
    drop(stop_guard);
    assert_eq!(
        manager
            .claim_monitor_completion("bclaimretry", &process, MonitorTaskStatus::Completed,)
            .await,
        MonitorCompletionClaim::Claimed
    );
    assert_eq!(
        manager
            .claim_monitor_completion("bclaimretry", &process, MonitorTaskStatus::Completed,)
            .await,
        MonitorCompletionClaim::NotRunning
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reap_waits_for_stop_resolution_and_preserves_stop_winner() {
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let (purpose, _stop_rx) = test_monitor_purpose("breapwinner");
    {
        let mut store = manager.process_store.lock().await;
        store
            .monitor_statuses
            .insert("breapwinner".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "breapwinner".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process: Arc::clone(&process),
                process_id: 1000,
                command: "reap arbitration command".to_string(),
                purpose,
            },
        );
    }
    let stop_guard = process.begin_monitor_stop();
    let reap = tokio::spawn({
        let manager = Arc::clone(&manager);
        let process = Arc::clone(&process);
        async move {
            manager
                .reap_monitor("breapwinner", &process, MonitorTaskStatus::Failed)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(!reap.is_finished());
    {
        let mut store = manager.process_store.lock().await;
        store
            .monitor_statuses
            .insert("breapwinner".to_string(), MonitorTaskStatus::Killed);
        store.monitor_tasks.remove("breapwinner");
    }
    drop(stop_guard);
    reap.await.expect("reaper should observe the stop winner");

    assert_eq!(
        manager
            .process_store
            .lock()
            .await
            .monitor_statuses
            .get("breapwinner"),
        Some(&MonitorTaskStatus::Killed)
    );
}

fn test_monitor_purpose(
    task_id: &str,
) -> (ProcessPurpose, watch::Receiver<Option<MonitorStopReason>>) {
    let (stop_tx, stop_rx) = watch::channel(None);
    (
        ProcessPurpose::Monitor {
            info: codex_protocol::protocol::CommandMonitorInfo {
                task_id: task_id.to_string(),
                description: "test monitor".to_string(),
                timeout_ms: 1_000,
                persistent: false,
            },
            stop_tx,
        },
        stop_rx,
    )
}

fn test_process_entry(
    process_id: i32,
    process: Arc<UnifiedExecProcess>,
    purpose: ProcessPurpose,
    turn: &crate::session::turn_context::TurnContext,
) -> ProcessEntry {
    ProcessEntry {
        plugin_metrics_sidecar: None,
        environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        permissions: super::super::TerminalPermissions::for_launch(
            turn.initial_environments
                .primary()
                .expect("turn environment"),
            turn,
            super::super::TerminalSandboxSource::Native,
            crate::sandboxing::SandboxPermissions::UseDefault,
            /*additional_permissions*/ None,
            /*internal_permissions*/ None,
        ),

        process,
        call_id: format!("call-{process_id}"),
        process_id,
        cwd: PathUri::parse("file:///tmp").expect("test cwd should be valid"),
        initial_exec_command_active: Arc::new(AtomicBool::new(false)),
        hook_command: "test command".to_string(),
        tty: false,
        network_approval: None,
        session: std::sync::Weak::new(),
        last_used: Instant::now(),
        purpose,
    }
}

#[cfg(unix)]
async fn remote_process_with_root_exit(
    output_closed: bool,
) -> Arc<crate::unified_exec::UnifiedExecProcess> {
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::UnknownProcess,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    process
        .write(b"observe remote root exit")
        .await
        .expect_err("unknown remote process should mark the root exited");
    if output_closed {
        mark_remote_output_completed(&process).await;
    }
    assert!(process.has_exited());
    assert_eq!(
        process
            .output_handles()
            .output_closed
            .load(Ordering::Acquire),
        output_closed
    );
    process
}

#[cfg(unix)]
async fn mark_remote_output_completed(process: &Arc<crate::unified_exec::UnifiedExecProcess>) {
    process
        .output_handles()
        .output_closed
        .store(true, Ordering::Release);
    process
        .output_handles()
        .output_closed_notify
        .notify_waiters();
    // The mock remote reader otherwise remains parked forever. Aborting it
    // after publishing Closed models the transport task having observed the
    // complete stream and lets tests exercise the same full-exit predicate as
    // production.
    process.terminate();
    while !process.output_task_finished() {
        tokio::task::yield_now().await;
    }
    assert!(process.output_completed_normally());
}

#[cfg(unix)]
#[tokio::test]
async fn monitor_full_exit_requires_normal_transport_output_completion() {
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::UnknownProcess,
            Some("termination unavailable".to_string()),
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    process
        .write(b"mark root exited")
        .await
        .expect_err("unknown remote process should mark the root exited");
    process
        .output_handles()
        .output_closed
        .store(true, Ordering::Release);
    process
        .output_handles()
        .output_closed_notify
        .notify_waiters();
    assert!(process.has_exited());
    assert!(!process.output_task_finished());
    assert!(!monitor_process_has_fully_exited(&process));

    let manager = UnifiedExecProcessManager::default();
    let (purpose, _stop_rx) = test_monitor_purpose("babnormal");
    {
        let mut store = manager.process_store.lock().await;
        store
            .monitor_statuses
            .insert("babnormal".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "babnormal".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process: Arc::clone(&process),
                process_id: 1000,
                command: "abnormal output command".to_string(),
                purpose,
            },
        );
    }

    assert!(matches!(
        manager.terminate_monitor("babnormal").await,
        TerminateMonitorResult::StopFailed
    ));
    let store = manager.process_store.lock().await;
    assert_eq!(
        store.monitor_statuses.get("babnormal"),
        Some(&MonitorTaskStatus::Running)
    );
    assert!(store.monitor_tasks.contains_key("babnormal"));
}

#[cfg(unix)]
#[tokio::test]
async fn pending_monitor_cleanup_preserves_deterministic_id_history() {
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    let process_id = manager.allocate_process_id().await;
    manager.register_pending_monitor_process(process_id, Arc::clone(&process));

    finish_pending_monitor_start_cleanup(process, process_id, &manager).await;

    assert!(manager.pending_monitor_processes_snapshot().is_empty());
    assert!(manager.process_store.lock().await.processes.is_empty());
    assert_eq!(manager.allocate_process_id().await, process_id + 1);
}

#[cfg(unix)]
#[tokio::test]
async fn failed_pending_monitor_cleanup_keeps_stored_process_addressable() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            Some("termination unavailable".to_string()),
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let manager = UnifiedExecProcessManager::default();
    manager.register_pending_monitor_process(1000, Arc::clone(&process));
    manager.process_store.lock().await.processes.insert(
        1000,
        test_process_entry(1000, Arc::clone(&process), ProcessPurpose::Terminal, &turn),
    );

    finish_pending_monitor_start_cleanup(process, 1000, &manager).await;

    assert!(
        manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&1000)
    );
    assert_eq!(manager.allocate_process_id().await, 1001);
}

#[cfg(unix)]
#[tokio::test]
async fn delayed_expected_release_preserves_aba_reused_process_entry() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let old_process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let new_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let manager = UnifiedExecProcessManager::default();
    manager.register_pending_monitor_process(1000, Arc::clone(&old_process));
    manager.process_store.lock().await.processes.insert(
        1000,
        test_process_entry(
            1000,
            Arc::clone(&new_process),
            ProcessPurpose::Terminal,
            &turn,
        ),
    );

    manager
        .release_process_id_if_matches(1000, &old_process)
        .await;

    let store = manager.process_store.lock().await;
    assert!(
        store
            .processes
            .get(&1000)
            .is_some_and(|entry| Arc::ptr_eq(&entry.process, &new_process))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn task_stop_finds_short_lived_monitor_outside_process_store() {
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    let (purpose, stop_rx) = test_monitor_purpose("bshort123");
    {
        let mut store = manager.process_store.lock().await;
        store
            .monitor_statuses
            .insert("bshort123".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "bshort123".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process: Arc::clone(&process),
                process_id: 1000,
                command: "short command".to_string(),
                purpose,
            },
        );
    }
    assert_eq!(manager.allocate_process_id().await, 1001);

    let result = manager.terminate_monitor("bshort123").await;

    assert!(matches!(result, TerminateMonitorResult::Stopped { .. }));
    assert_eq!(*stop_rx.borrow(), Some(MonitorStopReason::User));
    let store = manager.process_store.lock().await;
    assert_eq!(
        store.monitor_statuses.get("bshort123"),
        Some(&MonitorTaskStatus::Killed)
    );
    assert!(!store.monitor_tasks.contains_key("bshort123"));
}

#[cfg(unix)]
#[tokio::test]
async fn task_stop_removes_monitor_entry_while_initial_exec_is_active() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    let (purpose, _stop_rx) = test_monitor_purpose("binitial");
    let entry = test_process_entry(1000, Arc::clone(&process), purpose.clone(), &turn);
    entry
        .initial_exec_command_active
        .store(true, Ordering::Release);
    {
        let mut store = manager.process_store.lock().await;
        store.processes.insert(1000, entry);
        store
            .monitor_statuses
            .insert("binitial".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "binitial".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process,
                process_id: 1000,
                command: "initial command".to_string(),
                purpose,
            },
        );
    }

    assert!(matches!(
        manager.terminate_monitor("binitial").await,
        TerminateMonitorResult::Stopped { .. }
    ));
    assert!(
        !manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&1000)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn list_processes_keeps_root_exited_monitor_until_output_closes() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let monitor_process = remote_process_with_root_exit(/*output_closed*/ false).await;
    let ordinary_process = remote_process_with_root_exit(/*output_closed*/ false).await;
    let manager = UnifiedExecProcessManager::default();
    let (monitor_purpose, _stop_rx) = test_monitor_purpose("bdescendant");
    {
        let mut store = manager.process_store.lock().await;
        store.processes.insert(
            1000,
            test_process_entry(1000, monitor_process, monitor_purpose, &turn),
        );
        store.processes.insert(
            1001,
            test_process_entry(1001, ordinary_process, ProcessPurpose::Terminal, &turn),
        );
    }

    let listed = manager.list_processes().await;

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].process_id, "1000");
    assert_eq!(
        listed[0]
            .monitor
            .as_ref()
            .map(|monitor| monitor.task_id.as_str()),
        Some("bdescendant")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn monitor_registration_retains_root_exited_process_until_output_closes() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = remote_process_with_root_exit(/*output_closed*/ false).await;
    assert!(process_should_be_stored_for_initial_response(
        &process, /*is_monitor*/ true
    ));
    let manager = UnifiedExecProcessManager::default();
    let (purpose, _stop_rx) = test_monitor_purpose("brootexit");
    manager.process_store.lock().await.processes.insert(
        1000,
        test_process_entry(1000, Arc::clone(&process), purpose, &turn),
    );

    assert!(matches!(
        manager
            .refresh_process_state(1000, &process, OutputRetention::UntilCollected)
            .await,
        ProcessStatus::Alive { .. }
    ));
    assert!(
        manager
            .process_store
            .lock()
            .await
            .processes
            .contains_key(&1000)
    );

    mark_remote_output_completed(&process).await;
    assert!(matches!(
        manager
            .refresh_process_state(1000, &process, OutputRetention::UntilCollected)
            .await,
        ProcessStatus::Exited { .. }
    ));
    assert!(
        manager
            .process_store
            .lock()
            .await
            .reserved_process_ids
            .contains(&1000)
    );
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn failed_task_stop_keeps_monitor_registered_and_running() {
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ true,
        )
        .await,
    );
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let (purpose, stop_rx) = test_monitor_purpose("bstuck123");
    {
        let mut store = manager.process_store.lock().await;
        store
            .monitor_statuses
            .insert("bstuck123".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "bstuck123".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process,
                process_id: 43,
                command: "stuck command".to_string(),
                purpose,
            },
        );
    }
    let stop = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.terminate_monitor("bstuck123").await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(crate::unified_exec::TERMINATE_CONFIRMATION_TIMEOUT).await;

    assert!(matches!(
        stop.await.expect("TaskStop should finish"),
        TerminateMonitorResult::StopFailed
    ));
    assert_eq!(*stop_rx.borrow(), None);
    let store = manager.process_store.lock().await;
    assert_eq!(
        store.monitor_statuses.get("bstuck123"),
        Some(&MonitorTaskStatus::Running)
    );
    assert!(store.monitor_tasks.contains_key("bstuck123"));
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn failed_capacity_termination_keeps_monitor_without_false_killed_status() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ true,
        )
        .await,
    );
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let (purpose, stop_rx) = test_monitor_purpose("bcapacity");
    let entry = test_process_entry(1000, Arc::clone(&process), purpose.clone(), &turn);
    {
        let mut store = manager.process_store.lock().await;
        store.processes.insert(1000, entry);
        store
            .monitor_statuses
            .insert("bcapacity".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "bcapacity".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process: Arc::clone(&process),
                process_id: 1000,
                command: "capacity command".to_string(),
                purpose: purpose.clone(),
            },
        );
    }
    let candidate = CapacityPruneCandidate {
        monitor_stop_guard: Some(process.begin_monitor_stop()),
        process_id: 1000,
        process: Arc::clone(&process),
        purpose,
    };
    let prune = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.finish_capacity_prune(candidate).await }
    });
    tokio::task::yield_now().await;
    assert_eq!(
        manager
            .claim_monitor_completion("bcapacity", &process, MonitorTaskStatus::Completed,)
            .await,
        MonitorCompletionClaim::StopPending
    );
    tokio::time::advance(crate::unified_exec::TERMINATE_CONFIRMATION_TIMEOUT).await;
    prune.await.expect("capacity cleanup should finish");

    assert_eq!(*stop_rx.borrow(), None);
    assert_eq!(manager.allocate_process_id().await, 1001);
    let store = manager.process_store.lock().await;
    assert!(store.processes.contains_key(&1000));
    assert!(!store.reserved_process_ids.contains(&1000));
    assert!(store.reserved_process_ids.contains(&1001));
    assert_eq!(
        store.monitor_statuses.get("bcapacity"),
        Some(&MonitorTaskStatus::Running)
    );
    assert!(store.monitor_tasks.contains_key("bcapacity"));
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_capacity_termination_keeps_monitor_addressable() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ true,
        )
        .await,
    );
    let manager = UnifiedExecProcessManager::default();
    let (purpose, stop_rx) = test_monitor_purpose("bcancelprune");
    manager.process_store.lock().await.processes.insert(
        1000,
        test_process_entry(1000, Arc::clone(&process), purpose.clone(), &turn),
    );
    let candidate = CapacityPruneCandidate {
        process_id: 1000,
        process: Arc::clone(&process),
        purpose,
        monitor_stop_guard: Some(process.begin_monitor_stop()),
    };
    let mut prune = Box::pin(manager.finish_capacity_prune(candidate));
    assert!(futures::poll!(&mut prune).is_pending());
    assert!(process.is_monitor_stop_pending());

    drop(prune);

    let terminal = manager
        .resolve_terminal(1000)
        .await
        .expect("cancelled cleanup must leave the process addressable");
    assert!(Arc::ptr_eq(&terminal.process, &process));
    assert!(!process.is_monitor_stop_pending());
    assert_eq!(*stop_rx.borrow(), None);
    assert!(
        manager
            .process_store
            .lock()
            .await
            .reserved_process_ids
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn completed_task_authority_wins_over_late_capacity_cleanup() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    let (purpose, stop_rx) = test_monitor_purpose("bcapacitydone");
    let entry = test_process_entry(1000, Arc::clone(&process), purpose.clone(), &turn);
    {
        let mut store = manager.process_store.lock().await;
        store.processes.insert(1000, entry);
        store
            .monitor_statuses
            .insert("bcapacitydone".to_string(), MonitorTaskStatus::Completed);
    }
    let candidate = CapacityPruneCandidate {
        monitor_stop_guard: Some(process.begin_monitor_stop()),
        process_id: 1000,
        process: Arc::clone(&process),
        purpose,
    };

    manager.finish_capacity_prune(candidate).await;

    assert_eq!(*stop_rx.borrow(), Some(MonitorStopReason::Capacity));
    assert_eq!(
        manager
            .process_store
            .lock()
            .await
            .monitor_statuses
            .get("bcapacitydone"),
        Some(&MonitorTaskStatus::Completed)
    );
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn failed_session_shutdown_termination_keeps_process_addressable() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            Some("termination denied".to_string()),
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let manager = Arc::new(UnifiedExecProcessManager::default());
    {
        let mut store = manager.process_store.lock().await;
        store.processes.insert(
            45,
            test_process_entry(45, process, ProcessPurpose::Terminal, &turn),
        );
    }
    manager.terminate_all_processes().await;

    let store = manager.process_store.lock().await;
    assert!(store.processes.contains_key(&45));
    assert!(!store.reserved_process_ids.contains(&45));
}

#[cfg(unix)]
#[tokio::test]
async fn session_shutdown_accepts_ordinary_termination_without_output_closure() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let manager = UnifiedExecProcessManager::default();
    manager.process_store.lock().await.processes.insert(
        45,
        test_process_entry(45, Arc::clone(&process), ProcessPurpose::Terminal, &turn),
    );
    assert!(!process.has_exited());
    assert!(!process.output_completed_normally());

    tokio::time::timeout(Duration::from_secs(1), manager.terminate_all_processes())
        .await
        .expect("ordinary cleanup should require only executor acknowledgement");

    assert!(manager.process_store.lock().await.processes.is_empty());
}

#[tokio::test]
async fn network_denial_fallback_message_names_sandbox_network_proxy() {
    let message = network_denial_message_for_session(/*session*/ None, /*deferred*/ None).await;

    assert_eq!(
        message,
        "Network access was denied by the Codex sandbox network proxy."
    );
}

#[tokio::test(start_paused = true)]
async fn late_network_denial_grace_observes_cancellation_after_exit() -> anyhow::Result<()> {
    use crate::environment_selection::EnvironmentAttachment;
    use crate::environment_selection::TurnEnvironmentSnapshot;
    use crate::environment_selection::TurnEnvironmentState;
    use crate::guardian::GuardianNetworkAccessTrigger;
    use crate::tools::network_approval::NetworkApprovalSpec;
    use crate::tools::network_approval::begin_network_approval;
    use crate::tools::network_approval::build_blocked_request_observer;
    use codex_network_proxy::BlockedRequest;
    use codex_network_proxy::BlockedRequestArgs;
    use codex_network_proxy::EnvironmentNetworkPolicy;
    use codex_tools::ToolName;

    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    Arc::make_mut(&mut turn.config).permissions.network = None;
    let mut remote = turn
        .initial_environments
        .primary()
        .expect("primary environment")
        .clone();
    let remote_id = codex_exec_server::REMOTE_ENVIRONMENT_ID;
    let (attachment, _guard, _) = EnvironmentAttachment::new(
        remote_id,
        Arc::new(codex_exec_server::Environment::create_for_tests(Some(
            "ws://127.0.0.1:1".into(),
        ))?),
    );
    remote.attachment = attachment;
    remote.selection.environment_id = remote_id.to_string();
    remote.config_mut().permission_profile =
        codex_protocol::models::PermissionProfileSnapshot::legacy(
            codex_protocol::models::PermissionProfile::workspace_write(),
        );
    remote.config_mut().network_policy = Some(EnvironmentNetworkPolicy::from_config(
        &codex_network_proxy::NetworkProxyConfig {
            enabled: true,
            ..Default::default()
        },
        /*managed_allowed_domains_only*/ false,
    ));
    let environments = TurnEnvironmentSnapshot {
        environments: vec![TurnEnvironmentState::Ready(remote)],
    };
    let remote = environments.primary().expect("remote environment");

    for register_shared_listener in [false, true] {
        let approval = begin_network_approval(
            &session,
            &turn,
            &environments,
            /*managed_network_active*/ true,
            Some(NetworkApprovalSpec {
                network: None,
                environment_id: remote_id.to_string(),
                tool_name: ToolName::plain("exec_command"),
                command: "pwd".into(),
                trigger: GuardianNetworkAccessTrigger {
                    call_id: "exec".into(),
                    tool_name: "exec_command".into(),
                    command: vec!["pwd".into()],
                    cwd: remote.cwd().clone(),
                    sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
                    additional_permissions: None,
                    justification: None,
                    tty: None,
                },
                permission_profile: remote.permission_profile().clone(),
                network_policy: remote.config().network_policy.clone(),
            }),
        )
        .await
        .expect("build network proxy")
        .expect("network approval");
        let callback = approval
            .execution_proxy()
            .remote_policy_decider(/*allow_local_binding*/ true)
            .expect("remote policy callback");
        let finish = finish_deferred_network_approval_after_process_exit_for_session(
            Some(&session),
            approval.into_deferred(),
        );
        tokio::pin!(finish);
        assert!(futures::poll!(&mut finish).is_pending());

        let observer = register_shared_listener.then(|| {
            build_blocked_request_observer(Arc::clone(&session.services.network_approval))
        });
        drop(callback);
        if let Some(observer) = observer {
            // A shared listener started during the command and can report an untagged denial.
            tokio::time::advance(Duration::from_millis(10)).await;
            assert!(futures::poll!(&mut finish).is_pending());
            let blocked = BlockedRequest::new(BlockedRequestArgs {
                host: "blocked.example".into(),
                protocol: "http".into(),
                port: Some(80),
                reason: "not_allowed".into(),
                decision: Some("deny".into()),
                source: Some("decider".into()),
                client: None,
                method: None,
                mode: None,
            });
            let expected = crate::network_policy_decision::denied_network_policy_message(&blocked)
                .expect("blocked request must describe the denied host");
            observer.on_blocked_request(blocked).await;
            assert_eq!(finish.await, Err(expected));
        } else {
            assert_eq!(futures::poll!(&mut finish), std::task::Poll::Ready(Ok(())));
        }
    }

    Ok(())
}

#[tokio::test]
async fn failed_initial_end_for_unstored_process_uses_fallback_output() {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        crate::session::step_context::StepContext::for_test(Arc::clone(&turn)),
        tokio_util::sync::CancellationToken::new(),
        "call-unified-denied".to_string(),
    );
    let environment = turn
        .initial_environments
        .primary()
        .expect("primary environment");
    let prepared = ParsedExecCommand::parse(
        r#"{"cmd":"echo before"}"#,
        &session,
        &context.step_context,
        environment,
    )
    .expect("parse command")
    .prepare(ExecCommandLifetime::Interactive)
    .expect("prepare command");
    let request = ExecCommandRequest {
        prepared,
        process_id: 123,
        network: None,
    };

    let output_buffer = Arc::new(tokio::sync::Mutex::new(OutputBuffers::default()));
    output_buffer
        .lock()
        .await
        .push_chunk(ExecOutputStream::Stdout, b"PARTIAL_TRANSCRIPT");

    emit_failed_initial_exec_end_if_unstored(
        /*process_stored*/ false,
        Some(codex_protocol::sandbox::SandboxType::WindowsMxc),
        &context,
        &request,
        request.prepared.cwd.clone(),
        /*plugin_attribution*/ None,
        output_buffer,
        "PRE_DENIAL_MARKER".to_string(),
        "Network access denied".to_string(),
        Duration::from_millis(7),
    )
    .await;

    let event = tokio::time::timeout(Duration::from_secs(1), rx_event.recv())
        .await
        .expect("timed out waiting for failed command execution item")
        .expect("event channel closed");
    let codex_protocol::protocol::EventMsg::ItemCompleted(completed_event) = event.msg else {
        panic!("expected ItemCompleted event");
    };
    let codex_protocol::items::TurnItem::CommandExecution(item) = completed_event.item else {
        panic!("expected CommandExecution item");
    };
    assert_eq!(item.id, "call-unified-denied");
    assert_eq!(
        item.sandbox_type,
        Some(codex_protocol::sandbox::SandboxType::WindowsMxc)
    );
    assert_eq!(
        item.status,
        codex_protocol::items::CommandExecutionStatus::Failed
    );
    assert_eq!(item.exit_code, Some(-1));
    assert_eq!(item.process_id.as_deref(), Some("123"));
    assert_eq!(
        item.aggregated_output.as_deref(),
        Some("PRE_DENIAL_MARKER\nNetwork access denied")
    );
}

#[test]
fn pruning_prefers_exited_processes_outside_recently_used() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), true),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(2));
}

#[test]
fn pruning_falls_back_to_lru_when_no_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), false),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), false),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    assert_eq!(candidate, Some(1));
}

#[test]
fn pruning_protects_recent_processes_even_if_exited() {
    let now = Instant::now();
    let meta = vec![
        (1, now - Duration::from_secs(40), false),
        (2, now - Duration::from_secs(30), false),
        (3, now - Duration::from_secs(20), true),
        (4, now - Duration::from_secs(19), false),
        (5, now - Duration::from_secs(18), false),
        (6, now - Duration::from_secs(17), false),
        (7, now - Duration::from_secs(16), false),
        (8, now - Duration::from_secs(15), false),
        (9, now - Duration::from_secs(14), false),
        (10, now - Duration::from_secs(13), true),
    ];

    let candidate = UnifiedExecProcessManager::process_id_to_prune_from_meta(&meta);

    // (10) is exited but among the last 8; we should drop the LRU outside that set.
    assert_eq!(candidate, Some(1));
}

#[cfg(unix)]
#[tokio::test]
async fn capacity_prune_raises_monitor_stop_before_returning_candidate() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let (monitor_purpose, _stop_rx) = test_monitor_purpose("bcapacityguard");
    let now = Instant::now();
    let mut store = ProcessStore::default();
    let max_process_id =
        i32::try_from(MAX_UNIFIED_EXEC_PROCESSES).expect("process cap should fit in i32");
    for process_id in 1..=max_process_id {
        let purpose = if process_id == 1 {
            monitor_purpose.clone()
        } else {
            ProcessPurpose::Terminal
        };
        let mut entry = test_process_entry(process_id, Arc::clone(&process), purpose, &turn);
        entry.last_used = if process_id == 1 {
            now - Duration::from_secs(2)
        } else {
            now
        };
        store.processes.insert(process_id, entry);
    }

    let candidate = UnifiedExecProcessManager::prune_processes_if_needed(&mut store)
        .expect("capacity should select the oldest monitor");

    assert_eq!(candidate.process_id, 1);
    assert_eq!(store.processes.len(), MAX_UNIFIED_EXEC_PROCESSES);
    assert!(store.processes.contains_key(&1));
    assert!(process.is_monitor_stop_pending());
    drop(candidate);
    assert!(!process.is_monitor_stop_pending());
}

#[cfg(unix)]
#[tokio::test]
async fn pruning_does_not_evict_live_process_while_exited_process_is_finalizing() {
    let (_, turn) = crate::session::tests::make_session_and_context().await;
    let exited_process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let live_process = Arc::new(
        crate::unified_exec::process_tests::remote_process(
            codex_exec_server::WriteStatus::Accepted,
            /*terminate_error*/ None,
            codex_sandboxing::SandboxType::None,
        )
        .await,
    );
    let _interaction_guard = exited_process.interaction_lock().lock_owned().await;
    let now = Instant::now();
    let cwd = PathUri::parse("file:///tmp").expect("test cwd should be valid");
    let mut store = ProcessStore::default();
    let max_process_id =
        i32::try_from(MAX_UNIFIED_EXEC_PROCESSES).expect("process cap should fit in i32");

    for process_id in 1..=max_process_id {
        let is_exited = process_id == 1;
        store.processes.insert(
            process_id,
            ProcessEntry {
                process: if is_exited {
                    Arc::clone(&exited_process)
                } else {
                    Arc::clone(&live_process)
                },
                plugin_metrics_sidecar: None,
                call_id: format!("call-{process_id}"),
                process_id,
                cwd: cwd.clone(),
                initial_exec_command_active: Arc::new(AtomicBool::new(false)),
                hook_command: format!("command-{process_id}"),
                tty: false,
                environment_id: codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
                permissions: super::super::TerminalPermissions::for_launch(
                    turn.initial_environments
                        .primary()
                        .expect("turn environment"),
                    &turn,
                    super::super::TerminalSandboxSource::Native,
                    crate::sandboxing::SandboxPermissions::UseDefault,
                    /*additional_permissions*/ None,
                    /*internal_permissions*/ None,
                ),
                network_approval: None,
                session: std::sync::Weak::new(),
                last_used: if is_exited {
                    now - Duration::from_secs(1)
                } else {
                    now
                },
                purpose: ProcessPurpose::Terminal,
            },
        );
    }

    let pruned = UnifiedExecProcessManager::prune_processes_if_needed(&mut store);

    assert_eq!(
        (
            pruned.map(|candidate| candidate.process_id),
            store.processes.len()
        ),
        (None, MAX_UNIFIED_EXEC_PROCESSES)
    );
}
