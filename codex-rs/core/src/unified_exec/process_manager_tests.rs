use super::*;
use crate::unified_exec::clamp_yield_time;
use codex_network_proxy::ManagedNetworkSandboxContext;
use pretty_assertions::assert_eq;
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

#[test]
fn env_overlay_for_exec_server_keeps_runtime_changes_only() {
    let local_policy_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/client-path".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        (
            CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
            "current-profile".to_string(),
        ),
    ]);
    let request_env = HashMap::from([
        ("HOME".to_string(), "/client-home".to_string()),
        ("PATH".to_string(), "/sandbox-path".to_string()),
        ("SHELL_SET".to_string(), "policy".to_string()),
        ("CODEX_THREAD_ID".to_string(), "thread-1".to_string()),
        (
            CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
            "current-profile".to_string(),
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
                CODEX_PERMISSION_PROFILE_ENV_VAR.to_string(),
                "current-profile".to_string(),
            ),
            (
                "CODEX_SANDBOX_NETWORK_DISABLED".to_string(),
                "1".to_string()
            ),
        ])
    );
}

#[test]
fn exec_env_policy_excludes_runtime_permission_profile() {
    let policy = ShellEnvironmentPolicy {
        r#set: HashMap::from([
            (
                "codex_permission_profile".to_string(),
                "stale-profile".to_string(),
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
            exclude: vec![CODEX_PERMISSION_PROFILE_ENV_VAR.to_string()],
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
        network: None,
        network_environment_id: None,
        expiration: crate::exec::ExecExpiration::DefaultTimeout,
        capture_policy: crate::exec::ExecCapturePolicy::ShellTool,
        sandbox: codex_sandboxing::SandboxType::None,
        windows_sandbox_policy_cwd: cwd.clone().into(),
        windows_sandbox_workspace_roots: vec![cwd],
        windows_sandbox_level: codex_protocol::config_types::WindowsSandboxLevel::Disabled,
        windows_sandbox_private_desktop: false,
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
            proxy_settings_mode,
            /*tty*/ true,
        )
    };
    let params = params_for_request(&request);

    assert_eq!(params.process_id.as_str(), "123");
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
    request.exec_server_sandbox = Some(
        codex_exec_server::FileSystemSandboxContext::from_permission_profile(permission_profile),
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
    let output_buffer = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    let output_notify = Arc::new(Notify::new());
    let output_closed = Arc::new(AtomicBool::new(false));
    let output_closed_notify = Arc::new(Notify::new());
    let cancellation_token = CancellationToken::new();
    let output = OutputHandles {
        output_buffer: Arc::clone(&output_buffer),
        output_notify: Arc::clone(&output_notify),
        monitor_capture: Arc::new(std::sync::Mutex::new(None)),
        monitor_capture_rx: Arc::new(std::sync::Mutex::new(None)),
        monitor_stream_output: None,
        output_closed: Arc::clone(&output_closed),
        output_closed_notify: Arc::clone(&output_closed_notify),
        cancellation_token: cancellation_token.clone(),
    };

    let collect = UnifiedExecProcessManager::collect_output_until_deadline(
        &output,
        /*pause_state*/ None,
        Instant::now() + Duration::from_secs(5),
    );
    let produce = async {
        for byte in [b'a', b'b', b'c'] {
            output_buffer.lock().await.push_chunk(
                vec![byte; crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES],
            );
            output_notify.notify_one();
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if output_buffer.lock().await.retained_bytes() == 0 {
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
    let mut expected = HeadTailBuffer::default();
    for byte in [b'a', b'b', b'c'] {
        expected.push_chunk(vec![
            byte;
            crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES
        ]);
    }
    assert_eq!(collected, expected);
}

#[tokio::test]
async fn output_collection_preserves_omissions_from_drained_buffer() {
    let mut buffered_output = HeadTailBuffer::default();
    buffered_output.push_chunk(vec![
        b'a';
        crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES
    ]);
    buffered_output.push_chunk(b"overflow".to_vec());
    let mut expected = HeadTailBuffer::default();
    expected.push_chunk(vec![
        b'a';
        crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES
    ]);
    expected.push_chunk(b"overflow".to_vec());
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
        monitor_stream_output: None,
        output_closed,
        output_closed_notify,
        cancellation_token,
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
            test_process_entry(1000, Arc::clone(&process), purpose.clone()),
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
) -> ProcessEntry {
    ProcessEntry {
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
async fn pending_monitor_cleanup_releases_id_only_after_confirmed_termination() {
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    manager.register_pending_monitor_process(1000, Arc::clone(&process));
    manager
        .process_store
        .lock()
        .await
        .reserved_process_ids
        .insert(1000);

    finish_pending_monitor_start_cleanup(process, 1000, &manager).await;

    let store = manager.process_store.lock().await;
    assert!(!store.reserved_process_ids.contains(&1000));
    assert!(!store.processes.contains_key(&1000));
}

#[cfg(unix)]
#[tokio::test]
async fn failed_pending_monitor_cleanup_keeps_stored_process_addressable() {
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
        test_process_entry(1000, Arc::clone(&process), ProcessPurpose::Terminal),
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
        test_process_entry(1000, Arc::clone(&new_process), ProcessPurpose::Terminal),
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
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    let (purpose, _stop_rx) = test_monitor_purpose("binitial");
    let entry = test_process_entry(1000, Arc::clone(&process), purpose.clone());
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
    let monitor_process = remote_process_with_root_exit(/*output_closed*/ false).await;
    let ordinary_process = remote_process_with_root_exit(/*output_closed*/ false).await;
    let manager = UnifiedExecProcessManager::default();
    let (monitor_purpose, _stop_rx) = test_monitor_purpose("bdescendant");
    {
        let mut store = manager.process_store.lock().await;
        store.processes.insert(
            1000,
            test_process_entry(1000, monitor_process, monitor_purpose),
        );
        store.processes.insert(
            1001,
            test_process_entry(1001, ordinary_process, ProcessPurpose::Terminal),
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
    let process = remote_process_with_root_exit(/*output_closed*/ false).await;
    assert!(process_should_be_stored_for_initial_response(
        &process, /*is_monitor*/ true
    ));
    let manager = UnifiedExecProcessManager::default();
    let (purpose, _stop_rx) = test_monitor_purpose("brootexit");
    manager.process_store.lock().await.processes.insert(
        1000,
        test_process_entry(1000, Arc::clone(&process), purpose),
    );

    assert!(matches!(
        manager.refresh_process_state(1000, true).await,
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
        manager.refresh_process_state(1000, true).await,
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
async fn failed_capacity_termination_reinserts_monitor_without_false_killed_status() {
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ true,
        )
        .await,
    );
    let manager = Arc::new(UnifiedExecProcessManager::default());
    let (purpose, stop_rx) = test_monitor_purpose("bcapacity");
    let entry = test_process_entry(1000, Arc::clone(&process), purpose.clone());
    {
        let mut store = manager.process_store.lock().await;
        store.reserved_process_ids.insert(1000);
        store
            .monitor_statuses
            .insert("bcapacity".to_string(), MonitorTaskStatus::Running);
        store.monitor_tasks.insert(
            "bcapacity".to_string(),
            crate::unified_exec::MonitorTaskRegistration {
                process: Arc::clone(&process),
                process_id: 1000,
                command: "capacity command".to_string(),
                purpose,
            },
        );
    }
    let candidate = CapacityPruneCandidate {
        monitor_stop_guard: Some(process.begin_monitor_stop()),
        entry,
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
async fn completed_task_authority_wins_over_late_capacity_cleanup() {
    let process = remote_process_with_root_exit(/*output_closed*/ true).await;
    let manager = UnifiedExecProcessManager::default();
    let (purpose, stop_rx) = test_monitor_purpose("bcapacitydone");
    let entry = test_process_entry(1000, Arc::clone(&process), purpose);
    manager
        .process_store
        .lock()
        .await
        .monitor_statuses
        .insert("bcapacitydone".to_string(), MonitorTaskStatus::Completed);
    let candidate = CapacityPruneCandidate {
        monitor_stop_guard: Some(process.begin_monitor_stop()),
        entry,
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
    let process = Arc::new(
        crate::unified_exec::blocking_terminate_remote_process(
            /*capture_monitor_output*/ false,
        )
        .await,
    );
    let manager = Arc::new(UnifiedExecProcessManager::default());
    {
        let mut store = manager.process_store.lock().await;
        store.processes.insert(
            45,
            test_process_entry(45, process, ProcessPurpose::Terminal),
        );
    }
    let shutdown = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.terminate_all_processes().await }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(crate::unified_exec::TERMINATE_CONFIRMATION_TIMEOUT).await;
    shutdown.await.expect("session termination should finish");

    let store = manager.process_store.lock().await;
    assert!(store.processes.contains_key(&45));
    assert!(!store.reserved_process_ids.contains(&45));
}

#[tokio::test]
async fn network_denial_fallback_message_names_sandbox_network_proxy() {
    let message = network_denial_message_for_session(/*session*/ None, /*deferred*/ None).await;

    assert_eq!(
        message,
        "Network access was denied by the Codex sandbox network proxy."
    );
}

#[tokio::test]
async fn late_network_denial_grace_observes_cancellation_after_exit() {
    let cancellation = CancellationToken::new();
    let cancellation_for_task = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancellation_for_task.cancel();
    });

    assert!(wait_for_late_network_denial(Some(cancellation)).await);
}

#[tokio::test]
async fn failed_initial_end_for_unstored_process_uses_fallback_output() {
    let (session, turn, rx_event) = crate::session::tests::make_session_and_context_with_rx().await;
    let context = UnifiedExecContext::new(
        Arc::clone(&session),
        Arc::clone(&turn),
        "call-unified-denied".to_string(),
    );
    let request = ExecCommandRequest {
        command: vec![
            "sh".to_string(),
            "-lc".to_string(),
            "echo before".to_string(),
        ],
        shell_type: crate::shell::ShellType::Sh,
        hook_command: "echo before".to_string(),
        process_id: 123,
        yield_time_ms: 1000,
        max_output_tokens: None,
        #[allow(deprecated)]
        cwd: turn.cwd.clone().into(),
        #[allow(deprecated)]
        sandbox_cwd: turn.cwd.clone().into(),
        turn_environment: turn
            .environments
            .primary()
            .cloned()
            .expect("primary environment"),
        shell_mode: codex_tools::UnifiedExecShellMode::Direct,
        network: None,
        tty: true,
        sandbox_permissions: crate::sandboxing::SandboxPermissions::UseDefault,
        additional_permissions: None,
        additional_permissions_preapproved: false,
        justification: None,
        prefix_rule: None,
        monitor: None,
    };

    let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
    transcript
        .lock()
        .await
        .push_chunk(b"PARTIAL_TRANSCRIPT".to_vec());

    emit_failed_initial_exec_end_if_unstored(
        /*process_started_alive*/ false,
        &context,
        &request,
        #[allow(deprecated)]
        turn.cwd.clone().into(),
        /*plugin_attribution*/ None,
        transcript,
        /*monitor_stream_output*/ None,
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
        let mut entry = test_process_entry(process_id, Arc::clone(&process), purpose);
        entry.last_used = if process_id == 1 {
            now - Duration::from_secs(2)
        } else {
            now
        };
        store.processes.insert(process_id, entry);
    }

    let candidate = UnifiedExecProcessManager::prune_processes_if_needed(&mut store)
        .expect("capacity should select the oldest monitor");

    assert_eq!(candidate.entry.process_id, 1);
    assert!(process.is_monitor_stop_pending());
    drop(candidate);
    assert!(!process.is_monitor_stop_pending());
}

#[cfg(unix)]
#[tokio::test]
async fn pruning_does_not_evict_live_process_while_exited_process_is_finalizing() {
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
                call_id: format!("call-{process_id}"),
                process_id,
                cwd: cwd.clone(),
                initial_exec_command_active: Arc::new(AtomicBool::new(false)),
                hook_command: format!("command-{process_id}"),
                tty: false,
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
            pruned.map(|candidate| candidate.entry.process_id),
            store.processes.len()
        ),
        (None, MAX_UNIFIED_EXEC_PROCESSES)
    );
}
