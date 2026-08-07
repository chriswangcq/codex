use rand::Rng;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

use crate::codex_thread::BackgroundTerminalInfo;
use crate::codex_thread::BackgroundTerminalOutput;
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::exec_env::create_env;
use crate::exec_env::inject_permission_profile_env;
use crate::exec_policy::ExecApprovalRequest;
use crate::sandboxing::ExecOptions;
use crate::sandboxing::ExecRequest;
use crate::sandboxing::ExecServerEnvConfig;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventStage;
use crate::tools::network_approval::DeferredNetworkApproval;
use crate::tools::network_approval::finish_deferred_network_approval;
use crate::tools::orchestrator::ToolOrchestrator;
use crate::tools::runtimes::is_managed_proxy_env_var;
use crate::tools::runtimes::unified_exec::UnifiedExecRequest as UnifiedExecToolRequest;
use crate::tools::runtimes::unified_exec::UnifiedExecRuntime;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES;
use crate::unified_exec::MAX_YIELD_TIME_MS;
use crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS;
use crate::unified_exec::MIN_YIELD_TIME_MS;
use crate::unified_exec::MonitorStopReason;
use crate::unified_exec::MonitorStreamOutput;
use crate::unified_exec::MonitorTaskStatus;
use crate::unified_exec::ProcessEntry;
use crate::unified_exec::ProcessPurpose;
use crate::unified_exec::ProcessStore;
use crate::unified_exec::TerminateMonitorResult;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinInteractionEvent;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::async_watcher::emit_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::emit_failed_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::spawn_exit_watcher;
use crate::unified_exec::async_watcher::start_streaming_output;
use crate::unified_exec::clamp_yield_time;
use crate::unified_exec::generate_chunk_id;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::MonitorStopGuard;
use crate::unified_exec::process::OutputHandles;
use crate::unified_exec::process::SpawnLifecycleHandle;
use crate::unified_exec::process::UnifiedExecProcess;
use codex_core_plugins::PluginCommandAttribution;
use codex_network_proxy::NetworkPolicyDecider;
use codex_network_proxy::NetworkProxy;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::SandboxErr;
use codex_protocol::protocol::CommandMonitorTerminationReason;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_sandboxing::SandboxCommand;
use codex_tools::ToolName;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
use codex_utils_path_uri::PathUri;

const UNIFIED_EXEC_ENV: [(&str, &str); 10] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("CODEX_CI", "1"),
];
const NETWORK_ACCESS_DENIED_MESSAGE: &str =
    "Network access was denied by the Codex sandbox network proxy.";
const LATE_NETWORK_DENIAL_GRACE_PERIOD: Duration = Duration::from_millis(100);
const INTERRUPT: &str = "\u{3}";
const BACKGROUND_TERMINAL_OUTPUT_TAIL_BYTES: usize = 8 * 1024;
const MONITOR_WORKER_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(2);

/// Test-only override for deterministic unified exec process IDs.
///
/// In production builds this value should remain at its default (`false`) and
/// must not be toggled.
static FORCE_DETERMINISTIC_PROCESS_IDS: AtomicBool = AtomicBool::new(false);

pub(super) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    FORCE_DETERMINISTIC_PROCESS_IDS.store(enabled, Ordering::Relaxed);
}

fn deterministic_process_ids_forced_for_tests() -> bool {
    FORCE_DETERMINISTIC_PROCESS_IDS.load(Ordering::Relaxed)
}

fn should_use_deterministic_process_ids() -> bool {
    cfg!(test) || deterministic_process_ids_forced_for_tests()
}

fn apply_unified_exec_env(mut env: HashMap<String, String>) -> HashMap<String, String> {
    for (key, value) in UNIFIED_EXEC_ENV {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

fn exec_env_policy_from_shell_policy(
    policy: &ShellEnvironmentPolicy,
) -> codex_exec_server::ExecEnvPolicy {
    let mut exclude = policy
        .exclude
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    exclude.push(CODEX_PERMISSION_PROFILE_ENV_VAR.to_string());
    let mut r#set = policy.r#set.clone();
    r#set.retain(|key, _| !key.eq_ignore_ascii_case(CODEX_PERMISSION_PROFILE_ENV_VAR));
    codex_exec_server::ExecEnvPolicy {
        inherit: policy.inherit.clone(),
        ignore_default_excludes: policy.ignore_default_excludes,
        exclude,
        r#set,
        include_only: policy
            .include_only
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
    }
}

fn env_overlay_for_exec_server(
    request_env: &HashMap<String, String>,
    local_policy_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    request_env
        .iter()
        .filter(|(key, value)| {
            key.as_str() == CODEX_PERMISSION_PROFILE_ENV_VAR
                || local_policy_env.get(*key) != Some(*value)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn exec_server_env_for_request(
    request: &ExecRequest,
) -> (
    Option<codex_exec_server::ExecEnvPolicy>,
    HashMap<String, String>,
) {
    if let Some(exec_server_env_config) = &request.exec_server_env_config {
        let mut env =
            env_overlay_for_exec_server(&request.env, &exec_server_env_config.local_policy_env);
        if request.exec_server_managed_network.is_some() {
            for (key, value) in &request.env {
                if is_managed_proxy_env_var(key, value) {
                    env.insert(key.clone(), value.clone());
                }
            }
        }
        (Some(exec_server_env_config.policy.clone()), env)
    } else {
        (None, request.env.clone())
    }
}

fn exec_server_params_for_request(
    process_id: i32,
    request: &ExecRequest,
    windows_sandbox_proxy_settings_mode: codex_sandboxing::WindowsSandboxProxySettingsMode,
    tty: bool,
) -> codex_exec_server::ExecParams {
    let (env_policy, env) = exec_server_env_for_request(request);
    let sandbox = request.exec_server_sandbox.clone().map(|mut sandbox| {
        sandbox.windows_sandbox_proxy_settings_mode = Some(windows_sandbox_proxy_settings_mode);
        sandbox
    });
    // Sandbox retries reuse the unified-exec ID but start a distinct executor process.
    let exec_server_process_id = if request.exec_server_sandbox.is_some() {
        format!("{process_id}-{}", Uuid::new_v4())
    } else {
        process_id.to_string()
    };
    codex_exec_server::ExecParams {
        process_id: exec_server_process_id.into(),
        argv: request.command.clone(),
        cwd: request.cwd.clone(),
        env_policy,
        env,
        tty,
        pipe_stdin: false,
        arg0: request.arg0.clone(),
        sandbox,
        enforce_managed_network: request.exec_server_enforce_managed_network,
        managed_network: request.exec_server_managed_network.clone(),
        network_proxy: request.exec_server_network_proxy.clone(),
    }
}

/// Borrowed process state prepared for a `write_stdin` or poll operation.
struct PreparedProcessHandles {
    process: Arc<UnifiedExecProcess>,
    output: OutputHandles,
    pause_state: Option<watch::Receiver<bool>>,
    session: Option<Arc<crate::session::session::Session>>,
    network_approval: Option<DeferredNetworkApproval>,
    call_id: String,
    hook_command: String,
    process_id: i32,
    tty: bool,
}

struct InitialExecCommandGuard {
    active: Arc<AtomicBool>,
}

impl Drop for InitialExecCommandGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

/// Ensures cancellation of the initial tool future cannot leave a monitor
/// process running with no consumer for its bounded capture channel.
struct PendingMonitorStartGuard {
    process: Option<Arc<UnifiedExecProcess>>,
    session: std::sync::Weak<crate::session::session::Session>,
    process_id: i32,
    worker_done: Option<CancellationToken>,
}

struct CapacityPruneCandidate {
    entry: ProcessEntry,
    monitor_stop_guard: Option<MonitorStopGuard>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MonitorCompletionClaim {
    Claimed,
    StopPending,
    NotRunning,
}

impl MonitorCompletionClaim {
    pub(crate) fn is_claimed(self) -> bool {
        self == Self::Claimed
    }

    pub(crate) fn is_stop_pending(self) -> bool {
        self == Self::StopPending
    }
}

impl PendingMonitorStartGuard {
    fn new(
        process: Arc<UnifiedExecProcess>,
        session: std::sync::Weak<crate::session::session::Session>,
        process_id: i32,
        worker_done: Option<CancellationToken>,
    ) -> Self {
        Self {
            process: Some(process),
            session,
            process_id,
            worker_done,
        }
    }

    fn disarm(&mut self) {
        self.process = None;
        self.worker_done = None;
    }
}

impl Drop for PendingMonitorStartGuard {
    fn drop(&mut self) {
        let Some(process) = self.process.take() else {
            return;
        };
        if let Some(worker_done) = self.worker_done.take() {
            worker_done.cancel();
        }
        // Drop both channel endpoints. A producer that is currently blocked on
        // bounded send is released immediately, then process termination owns
        // the remaining lifecycle cleanup.
        let _ = process.output_handles().take_monitor_capture();
        process.close_monitor_capture();
        if let (Ok(runtime), Some(session)) = (
            tokio::runtime::Handle::try_current(),
            self.session.upgrade(),
        ) {
            let process_id = self.process_id;
            runtime.spawn(async move {
                finish_pending_monitor_start_cleanup(
                    process,
                    process_id,
                    &session.services.unified_exec_manager,
                )
                .await;
            });
        } else {
            // No async confirmation path is available. This is deliberately a
            // best-effort fallback: the process id remains owned by its current
            // reservation or registry entry and must not be presented as
            // reusable merely because termination was requested.
            process.terminate();
        }
    }
}

async fn finish_pending_monitor_start_cleanup(
    process: Arc<UnifiedExecProcess>,
    process_id: i32,
    manager: &UnifiedExecProcessManager,
) {
    match process.terminate_confirmed().await {
        Ok(()) => {
            manager
                .release_process_id_if_matches(process_id, &process)
                .await
        }
        Err(err) => {
            // Either ProcessEntry or the manager-owned pending registry keeps
            // the process addressable, and the id remains unavailable for a
            // later shutdown retry.
            warn!(
                process_id,
                error = %err,
                "command monitor startup cancellation could not confirm termination"
            );
        }
    }
}

async fn unregister_network_approval_for_entry(entry: &ProcessEntry) {
    if let Some(network_approval) = entry.network_approval.as_ref()
        && let Some(session) = entry.session.upgrade()
    {
        session
            .services
            .network_approval
            .unregister_call(network_approval.registration_id())
            .await;
    }
}

fn process_has_fully_exited_for_removal(entry: &ProcessEntry) -> bool {
    if entry.purpose.monitor_info().is_some() {
        monitor_process_has_fully_exited(&entry.process)
    } else {
        entry.process.has_exited()
    }
}

fn monitor_process_has_fully_exited(process: &UnifiedExecProcess) -> bool {
    process.has_exited() && process.output_completed_normally() && process.output_task_finished()
}

async fn terminate_for_process_purpose(
    process: &UnifiedExecProcess,
    purpose: &ProcessPurpose,
) -> Result<(), UnifiedExecError> {
    if purpose.monitor_info().is_some() {
        process.terminate_confirmed().await
    } else {
        process.terminate_acknowledged().await
    }
}

fn process_should_be_stored_for_initial_response(
    process: &UnifiedExecProcess,
    is_monitor: bool,
) -> bool {
    if is_monitor {
        !monitor_process_has_fully_exited(process)
    } else {
        !process.has_exited() && process.exit_code().is_none()
    }
}

async fn finish_network_approval_after_process_exit_for_entry(
    entry: &ProcessEntry,
) -> Result<(), String> {
    let session = entry.session.upgrade();
    finish_deferred_network_approval_after_process_exit_for_session(
        session.as_ref(),
        entry.network_approval.clone(),
    )
    .await
}

async fn finish_deferred_network_approval_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> Result<(), String> {
    let Some(session) = session else {
        return Ok(());
    };
    finish_deferred_network_approval(session.as_ref(), deferred)
        .await
        .map_err(network_approval_error_message)
}

fn network_approval_error_message(err: ToolError) -> String {
    match err {
        ToolError::Rejected(message) => message,
        ToolError::Codex(err) => err.to_string(),
    }
}

async fn network_denial_message_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> String {
    let Some(session) = session else {
        return NETWORK_ACCESS_DENIED_MESSAGE.to_string();
    };
    match finish_deferred_network_approval(session.as_ref(), deferred).await {
        Ok(()) => NETWORK_ACCESS_DENIED_MESSAGE.to_string(),
        Err(err) => network_approval_error_message(err),
    }
}

async fn wait_for_late_network_denial(network_cancelled: Option<CancellationToken>) -> bool {
    let Some(network_cancelled) = network_cancelled else {
        return false;
    };
    if network_cancelled.is_cancelled() {
        return true;
    }

    tokio::select! {
        _ = network_cancelled.cancelled() => true,
        _ = tokio::time::sleep(LATE_NETWORK_DENIAL_GRACE_PERIOD) => false,
    }
}

async fn finish_deferred_network_approval_after_process_exit_for_session(
    session: Option<&Arc<crate::session::session::Session>>,
    deferred: Option<DeferredNetworkApproval>,
) -> Result<(), String> {
    wait_for_late_network_denial(
        deferred
            .as_ref()
            .map(DeferredNetworkApproval::cancellation_token),
    )
    .await;
    finish_deferred_network_approval_for_session(session, deferred).await
}

fn fail_process_with_message(process: &UnifiedExecProcess, message: String) -> UnifiedExecError {
    if let Some(message) = process.failure_message() {
        process.terminate();
        return UnifiedExecError::process_failed(message);
    }

    process.fail_and_terminate(message.clone());
    UnifiedExecError::process_failed(process.failure_message().unwrap_or(message))
}

#[allow(clippy::too_many_arguments)]
async fn emit_failed_initial_exec_end_if_unstored(
    process_started_alive: bool,
    context: &UnifiedExecContext,
    request: &ExecCommandRequest,
    cwd: PathUri,
    plugin_attribution: Option<PluginCommandAttribution>,
    transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
    monitor_stream_output: Option<MonitorStreamOutput>,
    fallback_output: String,
    message: String,
    wall_time: Duration,
) {
    if process_started_alive {
        return;
    }

    emit_failed_exec_end_for_unified_exec(
        Arc::clone(&context.session),
        Arc::clone(&context.turn),
        context.call_id.clone(),
        request.command.clone(),
        cwd,
        Some(request.process_id.to_string()),
        plugin_attribution,
        transcript,
        fallback_output,
        message,
        wall_time,
        monitor_stream_output,
        request.monitor.clone(),
        /*monitor_termination_reason*/ None,
    )
    .await;
}

fn terminate_process_on_network_denial(
    process: Arc<UnifiedExecProcess>,
    session: std::sync::Weak<crate::session::session::Session>,
    deferred: DeferredNetworkApproval,
) -> tokio::task::JoinHandle<()> {
    let network_cancelled = deferred.cancellation_token();
    let process_exited = process.cancellation_token();
    tokio::spawn(async move {
        let denied = tokio::select! {
            _ = network_cancelled.cancelled() => true,
            _ = process_exited.cancelled() => {
                wait_for_late_network_denial(Some(network_cancelled.clone())).await
            }
        };
        if !denied {
            return;
        }
        let session = session.upgrade();
        let message = network_denial_message_for_session(session.as_ref(), Some(deferred)).await;
        process.fail_and_terminate(message);
    })
}

impl UnifiedExecProcessManager {
    fn register_pending_monitor_process(&self, process_id: i32, process: Arc<UnifiedExecProcess>) {
        let mut pending = self
            .pending_monitor_processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match pending.entry(process_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(process);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                panic!("allocated monitor process id must not already have a pending owner")
            }
        }
    }

    fn remove_pending_monitor_process_if_matches(
        &self,
        process_id: i32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> bool {
        let mut pending = self
            .pending_monitor_processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !pending
            .get(&process_id)
            .is_some_and(|process| Arc::ptr_eq(process, expected_process))
        {
            return false;
        }
        pending.remove(&process_id);
        true
    }

    fn pending_monitor_processes_snapshot(&self) -> Vec<(i32, Arc<UnifiedExecProcess>)> {
        self.pending_monitor_processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(process_id, process)| (*process_id, Arc::clone(process)))
            .collect()
    }

    pub(crate) async fn allocate_process_id(&self) -> i32 {
        loop {
            let mut store = self.process_store.lock().await;
            let pending = self
                .pending_monitor_processes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            let process_id = if should_use_deterministic_process_ids() {
                // test or deterministic mode
                store
                    .reserved_process_ids
                    .iter()
                    .chain(store.processes.keys())
                    .copied()
                    .chain(store.monitor_tasks.values().map(|task| task.process_id))
                    .chain(pending.keys().copied())
                    .max()
                    .map(|m| std::cmp::max(m, 999) + 1)
                    .unwrap_or(1000)
            } else {
                // production mode → random
                rand::rng().random_range(1_000..100_000)
            };

            if store.reserved_process_ids.contains(&process_id)
                || store.processes.contains_key(&process_id)
                || store
                    .monitor_tasks
                    .values()
                    .any(|task| task.process_id == process_id)
                || pending.contains_key(&process_id)
            {
                continue;
            }

            store.reserved_process_ids.insert(process_id);
            return process_id;
        }
    }

    pub(crate) async fn release_process_id(&self, process_id: i32) {
        let removed = {
            let mut store = self.process_store.lock().await;
            store.remove(process_id)
        };
        if let Some(entry) = removed {
            unregister_network_approval_for_entry(&entry).await;
        }
    }

    /// Releases delayed process ownership only when the id still names the
    /// expected Arc. This prevents a late cleanup from deleting an ABA-reused
    /// ProcessEntry.
    async fn release_process_id_if_matches(
        &self,
        process_id: i32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) {
        let (removed, stop_purposes) = {
            let mut store = self.process_store.lock().await;
            let matching_tasks = store
                .monitor_tasks
                .iter()
                .filter(|(_, task)| {
                    task.process_id == process_id && Arc::ptr_eq(&task.process, expected_process)
                })
                .map(|(task_id, task)| (task_id.clone(), task.purpose.clone()))
                .collect::<Vec<_>>();
            for (task_id, _) in &matching_tasks {
                store.monitor_tasks.remove(task_id);
                store
                    .monitor_statuses
                    .insert(task_id.clone(), MonitorTaskStatus::Killed);
            }
            let entry_matches = store
                .processes
                .get(&process_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.process, expected_process));
            let removed = entry_matches.then(|| store.remove(process_id)).flatten();
            let pending_removed =
                self.remove_pending_monitor_process_if_matches(process_id, expected_process);
            if pending_removed
                && !store.processes.contains_key(&process_id)
                && !store
                    .monitor_tasks
                    .values()
                    .any(|task| task.process_id == process_id)
            {
                store.reserved_process_ids.remove(&process_id);
            }
            (
                removed,
                matching_tasks
                    .into_iter()
                    .map(|(_, purpose)| purpose)
                    .collect::<Vec<_>>(),
            )
        };
        for purpose in stop_purposes {
            purpose.request_monitor_stop(MonitorStopReason::SessionShutdown);
        }
        if let Some(entry) = removed {
            unregister_network_approval_for_entry(&entry).await;
        }
    }

    pub(crate) async fn exec_command(
        &self,
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let cwd = request.cwd.clone();
        let process = self
            .open_session_with_sandbox(&request, cwd.clone(), context)
            .await;

        let (process, mut deferred_network_approval) = match process {
            Ok((process, deferred_network_approval)) => {
                (Arc::new(process), deferred_network_approval)
            }
            Err(err) => {
                self.release_process_id(request.process_id).await;
                return Err(err);
            }
        };
        if request.monitor.is_some() {
            self.register_pending_monitor_process(request.process_id, Arc::clone(&process));
        }
        let monitor_worker_done = request.monitor.as_ref().map(|_| CancellationToken::new());
        let mut pending_monitor_start = request.monitor.as_ref().map(|_| {
            PendingMonitorStartGuard::new(
                Arc::clone(&process),
                Arc::downgrade(&context.session),
                request.process_id,
                monitor_worker_done.clone(),
            )
        });
        let network_denial_monitor = deferred_network_approval.as_ref().map(|deferred| {
            terminate_process_on_network_denial(
                Arc::clone(&process),
                Arc::downgrade(&context.session),
                deferred.clone(),
            )
        });

        let transcript = Arc::new(tokio::sync::Mutex::new(HeadTailBuffer::default()));
        let event_ctx = ToolEventCtx::new(
            context.session.as_ref(),
            context.turn.as_ref(),
            &context.call_id,
            /*turn_diff_tracker*/ None,
        );
        let plugin_attribution = cwd.to_abs_path().ok().and_then(|cwd| {
            context
                .turn
                .plugin_attribution_for_command(&request.command, &cwd)
        });
        let emitter = if let Some(monitor) = request.monitor.clone() {
            ToolEmitter::monitor(
                &request.command,
                cwd.clone(),
                request.process_id.to_string(),
                plugin_attribution.clone(),
                monitor,
                /*monitor_termination_reason*/ None,
            )
        } else {
            ToolEmitter::unified_exec(
                &request.command,
                cwd.clone(),
                ExecCommandSource::UnifiedExecStartup,
                Some(request.process_id.to_string()),
                plugin_attribution.clone(),
            )
        };
        emitter.emit(event_ctx, ToolEventStage::Begin).await;

        start_streaming_output(
            &process,
            context,
            Arc::clone(&transcript),
            /*emit_output_deltas*/ request.monitor.is_none(),
        );
        let start = Instant::now();
        let (mut monitor_done_tx, mut monitor_done_rx) = if request.monitor.is_some() {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            (Some(done_tx), Some(done_rx))
        } else {
            (None, None)
        };
        let (purpose, monitor_stop_rx) = match request.monitor.clone() {
            Some(info) => {
                let (stop_tx, stop_rx) = watch::channel(None);
                (ProcessPurpose::Monitor { info, stop_tx }, Some(stop_rx))
            }
            None => (ProcessPurpose::Terminal, None),
        };
        // Persist live sessions before the initial yield wait so interrupting the
        // turn cannot drop the last Arc and terminate the background process.
        let process_started_alive =
            process_should_be_stored_for_initial_response(&process, request.monitor.is_some());
        let _initial_exec_command_guard = if process_started_alive {
            let initial_exec_command_active = Arc::new(AtomicBool::new(true));
            self.store_process(
                Arc::clone(&process),
                context,
                &request.command,
                request.hook_command.clone(),
                cwd.clone(),
                plugin_attribution.clone(),
                start,
                request.process_id,
                request.tty,
                deferred_network_approval.clone(),
                network_denial_monitor,
                Arc::clone(&transcript),
                Arc::clone(&initial_exec_command_active),
                purpose.clone(),
                monitor_done_rx.take(),
            )
            .await;
            Some(InitialExecCommandGuard {
                active: initial_exec_command_active,
            })
        } else {
            None
        };
        let yield_time_ms = clamp_yield_time(request.yield_time_ms);
        // For the initial exec_command call, we both stream output to events
        // (via start_streaming_output above) and collect a snapshot here for
        // the tool response body.
        let deadline = start + Duration::from_millis(yield_time_ms);
        let collected_output = if request.monitor.is_some() {
            HeadTailBuffer::default()
        } else {
            Self::collect_output_until_deadline(
                process.output_handles(),
                Some(context.session.subscribe_elicitation_pause_state()),
                deadline,
            )
            .await
        };
        let wall_time = Instant::now().saturating_duration_since(start);

        let original_token_count = usize::try_from(approx_tokens_from_byte_count(
            collected_output.total_bytes(),
        ))
        .unwrap_or(usize::MAX);
        let output_omitted_bytes = NonZeroUsize::new(collected_output.omitted_bytes());
        let collected = if request.monitor.is_some() {
            process
                .output_handles()
                .output_buffer
                .lock()
                .await
                .to_bytes_with_omission_marker()
        } else {
            collected_output.to_bytes_with_omission_marker()
        };
        let text = String::from_utf8_lossy(&collected).to_string();
        let chunk_id = generate_chunk_id();
        if deferred_network_approval
            .as_ref()
            .is_some_and(DeferredNetworkApproval::is_cancelled)
        {
            let message = network_denial_message_for_session(
                Some(&context.session),
                deferred_network_approval.take(),
            )
            .await;
            emit_failed_initial_exec_end_if_unstored(
                process_started_alive,
                context,
                &request,
                cwd.clone(),
                plugin_attribution.clone(),
                Arc::clone(&transcript),
                process.output_handles().monitor_stream_output.clone(),
                text.clone(),
                message.clone(),
                wall_time,
            )
            .await;
            if request.monitor.is_none() {
                self.release_process_id(request.process_id).await;
            }
            return Err(fail_process_with_message(process.as_ref(), message));
        }
        if let Some(message) = process.failure_message() {
            let finish_result = finish_deferred_network_approval_for_session(
                Some(&context.session),
                deferred_network_approval.take(),
            )
            .await;
            emit_failed_initial_exec_end_if_unstored(
                process_started_alive,
                context,
                &request,
                cwd.clone(),
                plugin_attribution.clone(),
                Arc::clone(&transcript),
                process.output_handles().monitor_stream_output.clone(),
                text.clone(),
                message.clone(),
                wall_time,
            )
            .await;
            if request.monitor.is_none() {
                self.release_process_id(request.process_id).await;
            }
            if let Err(message) = finish_result {
                return Err(fail_process_with_message(process.as_ref(), message));
            }
            return Err(UnifiedExecError::process_failed(message));
        }
        let process_id = request.process_id;
        let (response_process_id, exit_code) = if process_started_alive {
            match self
                .refresh_process_state(
                    process_id,
                    /*reserve_after_removal*/ request.monitor.is_some(),
                )
                .await
            {
                ProcessStatus::Alive {
                    exit_code,
                    process_id,
                    ..
                } => (Some(process_id), exit_code),
                ProcessStatus::Exited { exit_code, entry } => {
                    if let Err(message) =
                        finish_deferred_network_approval_after_process_exit_for_session(
                            Some(&context.session),
                            deferred_network_approval.take(),
                        )
                        .await
                    {
                        return Err(fail_process_with_message(entry.process.as_ref(), message));
                    }
                    process
                        .check_for_sandbox_denial_with_text(&text)
                        .await
                        .map_err(|err| {
                            err.with_output_collection_metadata(
                                original_token_count,
                                output_omitted_bytes,
                            )
                        })?;
                    (None, exit_code)
                }
                ProcessStatus::Unknown => {
                    return Err(UnifiedExecError::UnknownProcessId { process_id });
                }
            }
        } else {
            // Short-lived ordinary commands emit their completed item
            // immediately. Monitor commands delay the item until their stdout
            // framer has flushed its final partial batch.
            let finish_result = finish_deferred_network_approval_after_process_exit_for_session(
                Some(&context.session),
                deferred_network_approval.take(),
            )
            .await;
            if let Err(message) = finish_result {
                emit_failed_initial_exec_end_if_unstored(
                    process_started_alive,
                    context,
                    &request,
                    cwd.clone(),
                    plugin_attribution.clone(),
                    Arc::clone(&transcript),
                    process.output_handles().monitor_stream_output.clone(),
                    text.clone(),
                    message.clone(),
                    wall_time,
                )
                .await;
                if request.monitor.is_none() {
                    self.release_process_id(request.process_id).await;
                }
                return Err(fail_process_with_message(process.as_ref(), message));
            }
            let exit_code = process.exit_code();
            let exit = exit_code.unwrap_or(-1);
            if request.monitor.is_none() {
                emit_exec_end_for_unified_exec(
                    Arc::clone(&context.session),
                    Arc::clone(&context.turn),
                    context.call_id.clone(),
                    request.command.clone(),
                    cwd.clone(),
                    Some(process_id.to_string()),
                    plugin_attribution.clone(),
                    Arc::clone(&transcript),
                    text.clone(),
                    exit,
                    wall_time,
                    process.output_handles().monitor_stream_output.clone(),
                    /*monitor*/ None,
                    /*monitor_termination_reason*/ None,
                )
                .await;
            }

            if request.monitor.is_none() {
                self.release_process_id(request.process_id).await;
            }
            process
                .check_for_sandbox_denial_with_text(&text)
                .await
                .map_err(|err| {
                    err.with_output_collection_metadata(original_token_count, output_omitted_bytes)
                })?;
            (None, exit_code)
        };

        if let Some(monitor) = request.monitor.clone() {
            let output_file = context
                .turn
                .config
                .codex_home
                .join("monitor-tasks")
                .join(context.session.thread_id().to_string())
                .join(format!("{}.output", monitor.task_id))
                .into_path_buf();
            let (Some(worker_done), Some(monitor_stop_rx), Some(monitor_done_tx)) = (
                monitor_worker_done.clone(),
                monitor_stop_rx,
                monitor_done_tx.take(),
            ) else {
                return Err(fail_process_with_message(
                    process.as_ref(),
                    "monitor lifecycle channels were not initialized".to_string(),
                ));
            };
            let short_monitor_done = if process_started_alive {
                None
            } else {
                let Some(monitor_done) = monitor_done_rx.take() else {
                    return Err(fail_process_with_message(
                        process.as_ref(),
                        "short monitor completion channel was not retained".to_string(),
                    ));
                };
                Some(monitor_done)
            };
            let mut store = self.process_store.lock().await;
            store
                .monitor_statuses
                .insert(monitor.task_id.clone(), MonitorTaskStatus::Running);
            store
                .monitor_output_files
                .insert(monitor.task_id.clone(), output_file.clone());
            if let Some(output_dir) = output_file.parent() {
                store.monitor_output_dirs.insert(output_dir.to_path_buf());
            }
            store.monitor_tasks.insert(
                monitor.task_id.clone(),
                crate::unified_exec::MonitorTaskRegistration {
                    process: Arc::clone(&process),
                    process_id: request.process_id,
                    command: request.hook_command.clone(),
                    purpose: purpose.clone(),
                },
            );
            self.remove_pending_monitor_process_if_matches(request.process_id, &process);
            // A short-lived monitor has no ProcessEntry. Once its task
            // registration is visible, that registry owns the process id until
            // the monitor reaches a terminal state.
            store.reserved_process_ids.remove(&request.process_id);
            // Spawn while holding the registry lock: a very short-lived monitor
            // may immediately try to claim completion, but cannot outrun its own
            // task and worker registrations.
            let abort_handle = context.session.start_command_monitor(
                context.call_id.clone(),
                context.turn.sub_id.clone(),
                monitor.clone(),
                process.output_handles().clone(),
                output_file,
                monitor_stop_rx,
                Arc::clone(&process),
                monitor_done_tx,
                worker_done.clone(),
                Arc::clone(&self.monitor_archive_budget),
            );
            store.monitor_workers.insert(
                monitor.task_id.clone(),
                Arc::new(crate::unified_exec::MonitorWorkerControl {
                    done: worker_done,
                    abort_handle,
                }),
            );
            drop(store);
            if let Some(guard) = pending_monitor_start.as_mut() {
                guard.disarm();
            }
            if let Some(monitor_done) = short_monitor_done {
                let session = Arc::clone(&context.session);
                let turn = Arc::clone(&context.turn);
                let call_id = context.call_id.clone();
                let command = request.command.clone();
                let cwd = cwd.clone();
                let process_id = request.process_id;
                let plugin_attribution = plugin_attribution.clone();
                let transcript = Arc::clone(&transcript);
                let text = text.clone();
                let exit = exit_code.unwrap_or(-1);
                let monitor_stream_output = process.output_handles().monitor_stream_output.clone();
                tokio::spawn(async move {
                    let monitor_termination_reason = monitor_done
                        .await
                        .unwrap_or(Some(CommandMonitorTerminationReason::Stopped));
                    emit_exec_end_for_unified_exec(
                        session,
                        turn,
                        call_id,
                        command,
                        cwd,
                        Some(process_id.to_string()),
                        plugin_attribution,
                        transcript,
                        text,
                        exit,
                        wall_time,
                        monitor_stream_output,
                        Some(monitor),
                        monitor_termination_reason,
                    )
                    .await;
                });
            }
        }

        let response = ExecCommandToolOutput {
            event_call_id: context.call_id.clone(),
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy: context.turn.model_info.truncation_policy.into(),
            max_output_tokens: request.max_output_tokens,
            process_id: response_process_id,
            exit_code,
            original_token_count: Some(original_token_count),
            output_omitted_bytes,
            hook_command: Some(request.hook_command.clone()),
        };

        Ok(response)
    }

    pub(crate) async fn write_stdin(
        &self,
        request: WriteStdinRequest<'_>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let process_id = request.process_id;

        // Different terminal sessions can be polled concurrently, but reads and
        // writes against one terminal must not overlap because they share a
        // draining output buffer and process lifecycle.
        let locked_process = {
            let store = self.process_store.lock().await;
            let entry = store
                .processes
                .get(&process_id)
                .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
            Arc::clone(&entry.process)
        };
        let _interaction_guard = locked_process.interaction_lock().lock_owned().await;

        let PreparedProcessHandles {
            process,
            output,
            pause_state,
            session,
            network_approval,
            call_id,
            hook_command,
            process_id,
            tty,
            ..
        } = self
            .prepare_process_handles(process_id, &locked_process)
            .await?;
        let mut status_after_write = None;

        if !request.input.is_empty() {
            if !tty {
                if request.input == INTERRUPT {
                    process.interrupt().await?;
                } else {
                    return Err(UnifiedExecError::StdinClosed);
                }
            } else {
                match process.write(request.input.as_bytes()).await {
                    Ok(()) => {
                        // Give the remote process a brief window to react so that we are
                        // more likely to capture its output in the poll below.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(err) => {
                        let status = self
                            .refresh_process_state(process_id, /*reserve_after_removal*/ false)
                            .await;
                        if matches!(status, ProcessStatus::Exited { .. }) {
                            status_after_write = Some(status);
                        } else if matches!(err, UnifiedExecError::ProcessFailed { .. }) {
                            process.terminate();
                            self.release_process_id(process_id).await;
                            return Err(err);
                        } else {
                            return Err(err);
                        }
                    }
                }
            }
        }

        let yield_time_ms = {
            // Empty polls use configurable background timeout bounds. Non-empty
            // writes keep a fixed max cap so interactive stdin remains responsive.
            let time_ms = request.yield_time_ms.max(MIN_YIELD_TIME_MS);
            if request.input.is_empty() {
                time_ms.clamp(MIN_EMPTY_YIELD_TIME_MS, self.max_write_stdin_yield_time_ms)
            } else {
                time_ms.min(MAX_YIELD_TIME_MS)
            }
        };
        let start = Instant::now();
        let deadline = start + Duration::from_millis(yield_time_ms);
        let collected_output =
            Self::collect_output_until_deadline(&output, pause_state, deadline).await;
        let wall_time = Instant::now().saturating_duration_since(start);

        let original_token_count = usize::try_from(approx_tokens_from_byte_count(
            collected_output.total_bytes(),
        ))
        .unwrap_or(usize::MAX);
        let output_omitted_bytes = NonZeroUsize::new(collected_output.omitted_bytes());
        let collected = collected_output.to_bytes_with_omission_marker();
        let chunk_id = generate_chunk_id();
        if network_approval
            .as_ref()
            .is_some_and(DeferredNetworkApproval::is_cancelled)
        {
            let message =
                network_denial_message_for_session(session.as_ref(), network_approval.clone())
                    .await;
            self.release_process_id(process_id).await;
            return Err(fail_process_with_message(process.as_ref(), message));
        }
        if let Some(message) = process.failure_message() {
            let finish_result = finish_deferred_network_approval_for_session(
                session.as_ref(),
                network_approval.clone(),
            )
            .await;
            self.release_process_id(process_id).await;
            if let Err(message) = finish_result {
                return Err(fail_process_with_message(process.as_ref(), message));
            }
            return Err(UnifiedExecError::process_failed(message));
        }

        // After polling, refresh_process_state tells us whether the PTY is
        // still alive or has exited and been removed from the store; we thread
        // that through so the handler can tag or suppress TerminalInteraction
        // with an appropriate process_id and exit_code.
        let status = if let Some(status) = status_after_write {
            status
        } else {
            self.refresh_process_state(process_id, /*reserve_after_removal*/ false)
                .await
        };
        let (process_id, exit_code, event_call_id) = match status {
            ProcessStatus::Alive {
                exit_code,
                call_id,
                process_id,
            } => (Some(process_id), exit_code, call_id),
            ProcessStatus::Exited { exit_code, entry } => {
                let call_id = entry.call_id.clone();
                if let Err(message) =
                    finish_network_approval_after_process_exit_for_entry(&entry).await
                {
                    return Err(fail_process_with_message(entry.process.as_ref(), message));
                }
                (None, exit_code, call_id)
            }
            ProcessStatus::Unknown => {
                if process.has_exited() {
                    (None, process.exit_code(), call_id)
                } else {
                    return Err(UnifiedExecError::UnknownProcessId {
                        process_id: request.process_id,
                    });
                }
            }
        };

        let response = ExecCommandToolOutput {
            event_call_id,
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy: request.truncation_policy,
            max_output_tokens: request.max_output_tokens,
            process_id,
            exit_code,
            original_token_count: Some(original_token_count),
            output_omitted_bytes,
            hook_command: Some(hook_command),
        };

        let should_emit_interaction = !request.input.is_empty() || response.process_id.is_some();
        if should_emit_interaction
            && let Some(WriteStdinInteractionEvent { session, turn }) = request.interaction_event
        {
            let interaction = TerminalInteractionEvent {
                call_id: response.event_call_id.clone(),
                process_id: response
                    .process_id
                    .unwrap_or(request.process_id)
                    .to_string(),
                stdin: request.input.to_string(),
            };
            session
                .send_event(turn.as_ref(), EventMsg::TerminalInteraction(interaction))
                .await;
        }

        Ok(response)
    }

    async fn refresh_process_state(
        &self,
        process_id: i32,
        reserve_after_removal: bool,
    ) -> ProcessStatus {
        let mut store = self.process_store.lock().await;
        let Some(entry) = store.processes.get_mut(&process_id) else {
            return ProcessStatus::Unknown;
        };

        let exit_code = entry.process.exit_code();
        let process_id = entry.process_id;

        if process_has_fully_exited_for_removal(entry) {
            let Some(entry) = store.remove(process_id) else {
                return ProcessStatus::Unknown;
            };
            if reserve_after_removal {
                store.reserved_process_ids.insert(process_id);
            }
            ProcessStatus::Exited {
                exit_code,
                entry: Box::new(entry),
            }
        } else {
            ProcessStatus::Alive {
                exit_code,
                call_id: entry.call_id.clone(),
                process_id,
            }
        }
    }

    async fn prepare_process_handles(
        &self,
        process_id: i32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> Result<PreparedProcessHandles, UnifiedExecError> {
        let mut store = self.process_store.lock().await;
        let entry = store
            .processes
            .get_mut(&process_id)
            .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
        if !Arc::ptr_eq(&entry.process, expected_process) {
            return Err(UnifiedExecError::UnknownProcessId { process_id });
        }
        entry.last_used = Instant::now();
        let output = entry.process.output_handles().clone();
        let pause_state = entry
            .session
            .upgrade()
            .map(|session| session.subscribe_elicitation_pause_state());
        let session = entry.session.upgrade();

        Ok(PreparedProcessHandles {
            process: Arc::clone(&entry.process),
            output,
            pause_state,
            session,
            network_approval: entry.network_approval.clone(),
            call_id: entry.call_id.clone(),
            hook_command: entry.hook_command.clone(),
            process_id: entry.process_id,
            tty: entry.tty,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_process(
        &self,
        process: Arc<UnifiedExecProcess>,
        context: &UnifiedExecContext,
        command: &[String],
        hook_command: String,
        cwd: PathUri,
        plugin_attribution: Option<PluginCommandAttribution>,
        started_at: Instant,
        process_id: i32,
        tty: bool,
        network_approval: Option<DeferredNetworkApproval>,
        network_denial_monitor: Option<tokio::task::JoinHandle<()>>,
        transcript: Arc<tokio::sync::Mutex<HeadTailBuffer>>,
        initial_exec_command_active: Arc<AtomicBool>,
        purpose: ProcessPurpose,
        monitor_done: Option<
            tokio::sync::oneshot::Receiver<Option<CommandMonitorTerminationReason>>,
        >,
    ) {
        let entry = ProcessEntry {
            process: Arc::clone(&process),
            call_id: context.call_id.clone(),
            process_id,
            cwd: cwd.clone(),
            initial_exec_command_active,
            hook_command,
            tty,
            network_approval,
            session: Arc::downgrade(&context.session),
            last_used: started_at,
            purpose: purpose.clone(),
        };
        let pruned_candidate = {
            let mut store = self.process_store.lock().await;
            let pruned_candidate = Self::prune_processes_if_needed(&mut store);
            if let Some(pruned_candidate) = pruned_candidate.as_ref() {
                // Keep the identity reserved while confirmed termination runs;
                // on failure the exact entry is reinserted and remains
                // addressable instead of becoming an orphan.
                store
                    .reserved_process_ids
                    .insert(pruned_candidate.entry.process_id);
            }
            store.processes.insert(process_id, entry);
            // Once registration is visible, the process table—not the pending
            // allocation set—is the authority for this id. allocate_process_id
            // checks both collections.
            store.reserved_process_ids.remove(&process_id);
            pruned_candidate
        };
        // prune_processes_if_needed runs while holding process_store; do async
        // network-approval cleanup only after dropping that lock.
        if let Some(pruned_candidate) = pruned_candidate {
            self.finish_capacity_prune(pruned_candidate).await;
        }

        spawn_exit_watcher(
            Arc::clone(&process),
            Arc::clone(&context.session),
            Arc::clone(&context.turn),
            context.call_id.clone(),
            command.to_vec(),
            cwd,
            process_id,
            plugin_attribution,
            transcript,
            started_at,
            network_denial_monitor,
            purpose.monitor_info().cloned(),
            monitor_done,
        );
    }

    async fn finish_capacity_prune(&self, candidate: CapacityPruneCandidate) {
        let CapacityPruneCandidate {
            entry: pruned_entry,
            monitor_stop_guard,
        } = candidate;
        let stopped = process_has_fully_exited_for_removal(&pruned_entry)
            || pruned_entry.process.terminate_confirmed().await.is_ok();
        if stopped {
            pruned_entry
                .purpose
                .request_monitor_stop(MonitorStopReason::Capacity);
            {
                let mut store = self.process_store.lock().await;
                store.reserved_process_ids.remove(&pruned_entry.process_id);
                if let Some(task_id) = pruned_entry
                    .purpose
                    .monitor_info()
                    .map(|info| info.task_id.clone())
                    && store.monitor_statuses.get(&task_id) == Some(&MonitorTaskStatus::Running)
                    && store
                        .monitor_tasks
                        .get(&task_id)
                        .is_some_and(|task| Arc::ptr_eq(&task.process, &pruned_entry.process))
                {
                    store
                        .monitor_statuses
                        .insert(task_id.clone(), MonitorTaskStatus::Killed);
                    store.monitor_tasks.remove(&task_id);
                }
            }
            drop(monitor_stop_guard);
            unregister_network_approval_for_entry(&pruned_entry).await;
        } else {
            let mut store = self.process_store.lock().await;
            let process_id = pruned_entry.process_id;
            store.processes.entry(process_id).or_insert(pruned_entry);
            // The process table makes the restored id unavailable. Keeping the
            // temporary prune reservation as well would leak one reservation
            // for every failed termination attempt.
            store.reserved_process_ids.remove(&process_id);
            drop(store);
            drop(monitor_stop_guard);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_session_with_exec_env(
        &self,
        process_id: i32,
        command: SandboxCommand,
        options: ExecOptions,
        attempt: &SandboxAttempt<'_>,
        network: Option<&NetworkProxy>,
        network_proxy_launch: Option<codex_network_proxy::RemoteNetworkProxyLaunchConfig>,
        environment_id: Option<&str>,
        exec_server_env_config: Option<ExecServerEnvConfig>,
        windows_sandbox_proxy_settings_mode: codex_sandboxing::WindowsSandboxProxySettingsMode,
        tty: bool,
        capture_monitor_output: bool,
        spawn_lifecycle: SpawnLifecycleHandle,
        environment: &codex_exec_server::Environment,
    ) -> Result<UnifiedExecProcess, ToolError> {
        let mut request = if environment.is_remote() {
            attempt.env_for_exec_server(command, options)
        } else {
            attempt.env_for(command, options, network, environment_id)
        }
        .map_err(ToolError::Codex)?;
        let network_policy_decider = network_proxy_launch
            .as_ref()
            .filter(|launch| launch.policy_decision_timeout_ms.is_some())
            .and_then(|_| network.and_then(NetworkProxy::remote_policy_decider));
        request.exec_server_network_proxy = network_proxy_launch;
        request.exec_server_env_config = exec_server_env_config;
        self.open_session_with_prepared_exec_env(
            process_id,
            &request,
            windows_sandbox_proxy_settings_mode,
            network_policy_decider,
            tty,
            capture_monitor_output,
            spawn_lifecycle,
            environment,
        )
        .await
        .map_err(|err| match err {
            UnifiedExecError::SandboxDenied { output, .. } => {
                ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                    output: Box::new(output),
                    network_policy_decision: None,
                }))
            }
            other => ToolError::Rejected(other.to_string()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open_session_with_prepared_exec_env(
        &self,
        process_id: i32,
        request: &ExecRequest,
        windows_sandbox_proxy_settings_mode: codex_sandboxing::WindowsSandboxProxySettingsMode,
        network_policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
        tty: bool,
        capture_monitor_output: bool,
        mut spawn_lifecycle: SpawnLifecycleHandle,
        environment: &codex_exec_server::Environment,
    ) -> Result<UnifiedExecProcess, UnifiedExecError> {
        let inherited_fds = spawn_lifecycle.inherited_fds();

        if environment.is_remote() {
            if !inherited_fds.is_empty() {
                return Err(UnifiedExecError::create_process(
                    "remote exec-server does not support inherited file descriptors".to_string(),
                ));
            }

            let backend = environment.get_exec_backend();
            let params = exec_server_params_for_request(
                process_id,
                request,
                windows_sandbox_proxy_settings_mode,
                tty,
            );
            let started = match network_policy_decider {
                Some(decider) => {
                    backend
                        .start_with_network_policy_decider(params, decider)
                        .await
                }
                None => backend.start(params).await,
            }
            .map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
            spawn_lifecycle.after_spawn();
            return UnifiedExecProcess::from_exec_server_started(started, capture_monitor_output)
                .await;
        }

        // TODO(anp): Keep PathUri through the local PTY/process launch boundary.
        let native_cwd = request
            .cwd
            .to_abs_path()
            .map_err(|_| UnifiedExecError::ForeignPath {
                path: request.cwd.clone(),
            })?;

        if request.command.is_empty() {
            return Err(UnifiedExecError::MissingCommandLine);
        }
        let network_proxy_restricting_sid = {
            #[cfg(target_os = "windows")]
            {
                if request.sandbox == codex_sandboxing::SandboxType::WindowsRestrictedToken {
                    request
                        .network
                        .as_ref()
                        .map(|network| {
                            network
                                .network_proxy_restricting_sid(
                                    request.network_environment_id.as_deref(),
                                )
                                .ok_or_else(|| {
                                    UnifiedExecError::create_process(
                                        "managed Windows proxy route is missing its restricting SID"
                                            .to_string(),
                                    )
                                })
                        })
                        .transpose()?
                } else {
                    None
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                None::<String>
            }
        };
        let windows_sandbox =
            if request.sandbox == codex_sandboxing::SandboxType::WindowsRestrictedToken {
                Some(codex_sandboxing::WindowsSandboxSpawnRequest {
                    permission_profile: &request.permission_profile,
                    workspace_roots: &request.windows_sandbox_workspace_roots,
                    windows_sandbox_level: request.windows_sandbox_level,
                    proxy_enforced: request.network.is_some(),
                    network_proxy_restricting_sid: network_proxy_restricting_sid.as_deref(),
                    proxy_settings_mode: windows_sandbox_proxy_settings_mode,
                    filesystem_overrides: request.windows_sandbox_filesystem_overrides.as_ref(),
                    use_private_desktop: request.windows_sandbox_private_desktop,
                })
            } else {
                None
            };
        let spawn_result = codex_sandboxing::spawn_process(codex_sandboxing::SpawnRequest {
            command: &request.command,
            cwd: native_cwd.as_path(),
            env: &request.env,
            arg0: &request.arg0,
            sandbox: request.sandbox,
            windows_sandbox,
            tty,
            stdin_open: tty,
            inherited_fds: &inherited_fds,
        })
        .await;
        spawn_lifecycle.after_spawn();
        let spawned =
            spawn_result.map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
        UnifiedExecProcess::from_spawned(
            spawned,
            request.sandbox,
            spawn_lifecycle,
            capture_monitor_output,
        )
        .await
    }

    pub(super) async fn open_session_with_sandbox(
        &self,
        request: &ExecCommandRequest,
        cwd: PathUri,
        context: &UnifiedExecContext,
    ) -> Result<(UnifiedExecProcess, Option<DeferredNetworkApproval>), UnifiedExecError> {
        let local_policy_env = create_env(
            &context.turn.config.permissions.shell_environment_policy,
            /*thread_id*/ None,
        );
        let mut env = local_policy_env.clone();
        env.insert(
            CODEX_THREAD_ID_ENV_VAR.to_string(),
            context.session.thread_id.to_string(),
        );
        let active_permission_profile = request.turn_environment.active_permission_profile();
        inject_permission_profile_env(&mut env, active_permission_profile.as_ref());
        let env = apply_unified_exec_env(env);
        let exec_server_env_config = ExecServerEnvConfig {
            policy: exec_env_policy_from_shell_policy(
                &context.turn.config.permissions.shell_environment_policy,
            ),
            local_policy_env,
        };
        let mut orchestrator = ToolOrchestrator::new();
        let mut runtime = UnifiedExecRuntime::new(self, request.shell_mode.clone());
        let exec_approval_requirement = context
            .session
            .services
            .exec_policy
            .create_exec_approval_requirement_for_command(ExecApprovalRequest {
                command: &request.command,
                approval_policy: context.turn.approval_policy(),
                permission_profile: request.turn_environment.permission_profile().clone(),
                windows_sandbox_level: context.turn.windows_sandbox_level,
                sandbox_permissions: if request.additional_permissions_preapproved {
                    crate::sandboxing::SandboxPermissions::UseDefault
                } else {
                    request.sandbox_permissions
                },
                prefix_rule: request.prefix_rule.clone(),
            })
            .await;
        let req = UnifiedExecToolRequest {
            command: request.command.clone(),
            shell_type: request.shell_type,
            hook_command: request.hook_command.clone(),
            process_id: request.process_id,
            cwd,
            sandbox_cwd: request.sandbox_cwd.clone(),
            turn_environment: request.turn_environment.clone(),
            env,
            exec_server_env_config: Some(exec_server_env_config),
            explicit_env_overrides: context
                .turn
                .config
                .permissions
                .shell_environment_policy
                .r#set
                .clone(),
            network: request.network.clone(),
            tty: request.tty,
            monitor: request.monitor.clone(),
            sandbox_permissions: request.sandbox_permissions,
            additional_permissions: request.additional_permissions.clone(),
            #[cfg(unix)]
            additional_permissions_preapproved: request.additional_permissions_preapproved,
            justification: request.justification.clone(),
            exec_approval_requirement,
        };
        let tool_ctx = ToolCtx {
            session: context.session.clone(),
            turn: context.turn.clone(),
            call_id: context.call_id.clone(),
            tool_name: ToolName::plain("exec_command"),
        };
        orchestrator
            .run(
                &mut runtime,
                &req,
                &tool_ctx,
                &context.turn,
                context.turn.approval_policy(),
            )
            .await
            .map(|result| (result.output, result.deferred_network_approval))
            .map_err(|err| match err {
                ToolError::Codex(err) => match err.details() {
                    CodexErrorDetails::Sandbox(SandboxErr::Denied { output, .. }) => {
                        let output = output.as_ref().clone();
                        let message = if output.aggregated_output.text.is_empty() {
                            let exit_code = output.exit_code;
                            format!("Process exited with code {exit_code}")
                        } else {
                            output.aggregated_output.text.clone()
                        };
                        UnifiedExecError::sandbox_denied(message, output)
                    }
                    _ => UnifiedExecError::create_process(format!("{err:?}")),
                },
                other => UnifiedExecError::create_process(format!("{other:?}")),
            })
    }

    pub(super) async fn collect_output_until_deadline(
        output: &OutputHandles,
        mut pause_state: Option<watch::Receiver<bool>>,
        mut deadline: Instant,
    ) -> HeadTailBuffer {
        const POST_EXIT_CLOSE_WAIT_CAP: Duration = Duration::from_millis(50);

        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
            ..
        } = output;
        let mut collected = HeadTailBuffer::default();
        let mut exit_signal_received = cancellation_token.is_cancelled();
        let mut post_exit_deadline: Option<Instant> = None;
        loop {
            Self::extend_deadlines_while_paused(
                &mut pause_state,
                &mut deadline,
                &mut post_exit_deadline,
            )
            .await;
            let drained_output: HeadTailBuffer;
            let has_drained_output: bool;
            let mut wait_for_output = None;
            {
                let mut guard = output_buffer.lock().await;
                drained_output = guard.drain();
                has_drained_output =
                    drained_output.retained_bytes() > 0 || drained_output.omitted_bytes() > 0;
                if !has_drained_output {
                    wait_for_output = Some(output_notify.notified());
                }
            }

            if !has_drained_output {
                exit_signal_received |= cancellation_token.is_cancelled();
                if exit_signal_received && output_closed.load(std::sync::atomic::Ordering::Acquire)
                {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining == Duration::ZERO {
                    break;
                }

                if exit_signal_received {
                    let now = Instant::now();
                    let close_wait_deadline = *post_exit_deadline
                        .get_or_insert_with(|| now + remaining.min(POST_EXIT_CLOSE_WAIT_CAP));
                    let close_wait_remaining = close_wait_deadline.saturating_duration_since(now);
                    if close_wait_remaining == Duration::ZERO {
                        break;
                    }
                    let notified = wait_for_output.unwrap_or_else(|| output_notify.notified());
                    let closed = output_closed_notify.notified();
                    tokio::pin!(notified);
                    tokio::pin!(closed);
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = &mut closed => {}
                        _ = tokio::time::sleep(close_wait_remaining) => break,
                        _ = Self::wait_for_pause_change(pause_state.as_ref()) => {}
                    }
                    continue;
                }

                let notified = wait_for_output.unwrap_or_else(|| output_notify.notified());
                tokio::pin!(notified);
                let exit_notified = cancellation_token.cancelled();
                tokio::pin!(exit_notified);
                tokio::select! {
                    _ = &mut notified => {}
                    _ = &mut exit_notified => exit_signal_received = true,
                    _ = tokio::time::sleep(remaining) => break,
                    _ = Self::wait_for_pause_change(pause_state.as_ref()) => {}
                }
                continue;
            }

            collected.push_buffer(drained_output);

            exit_signal_received |= cancellation_token.is_cancelled();
            if Instant::now() >= deadline {
                break;
            }
        }

        collected
    }

    async fn extend_deadlines_while_paused(
        pause_state: &mut Option<watch::Receiver<bool>>,
        deadline: &mut Instant,
        post_exit_deadline: &mut Option<Instant>,
    ) {
        let Some(receiver) = pause_state.as_mut() else {
            return;
        };
        if !*receiver.borrow() {
            return;
        }

        let paused_at = Instant::now();
        while *receiver.borrow() {
            if receiver.changed().await.is_err() {
                break;
            }
        }

        let paused_for = paused_at.elapsed();
        *deadline += paused_for;
        if let Some(post_exit_deadline) = post_exit_deadline.as_mut() {
            *post_exit_deadline += paused_for;
        }
    }

    async fn wait_for_pause_change(pause_state: Option<&watch::Receiver<bool>>) {
        match pause_state {
            Some(pause_state) => {
                let mut receiver = pause_state.clone();
                let _ = receiver.changed().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    fn prune_processes_if_needed(store: &mut ProcessStore) -> Option<CapacityPruneCandidate> {
        if store.processes.len() < MAX_UNIFIED_EXEC_PROCESSES {
            return None;
        }

        let mut meta: Vec<(i32, Instant, bool)> = store
            .processes
            .iter()
            .map(|(id, entry)| (*id, entry.last_used, entry.process.has_exited()))
            .collect();
        let mut found_locked_exited_process = false;

        while let Some(process_id) = Self::process_id_to_prune_from_meta(&meta) {
            let candidate_process = store
                .processes
                .get(&process_id)
                .map(|entry| Arc::clone(&entry.process));
            let candidate_has_exited = candidate_process
                .as_ref()
                .is_some_and(|process| process.has_exited());
            if found_locked_exited_process && !candidate_has_exited {
                // The store may temporarily exceed its soft cap while an exited
                // process is publishing its terminal event. Do not evict a live
                // process just because that exited process is briefly locked.
                return None;
            }

            // Do not prune processes while write_stdin or terminal event
            // publication holds their interaction lock.
            if let Some(interaction_lock) = candidate_process
                .as_ref()
                .map(|process| process.interaction_lock())
                && let Ok(_interaction_guard) = interaction_lock.try_lock_owned()
            {
                let entry = store.remove(process_id)?;
                // Raise the guard before releasing process_store. Completion
                // claims use that same lock, so no worker can observe the entry
                // as removed without also observing a pending capacity stop.
                let monitor_stop_guard = entry
                    .purpose
                    .monitor_info()
                    .map(|_| entry.process.begin_monitor_stop());
                return Some(CapacityPruneCandidate {
                    entry,
                    monitor_stop_guard,
                });
            }
            found_locked_exited_process |= candidate_has_exited
                || candidate_process.is_some_and(|process| process.has_exited());
            meta.retain(|(id, _, _)| *id != process_id);
        }

        None
    }

    // Centralized pruning policy so we can easily swap strategies later.
    fn process_id_to_prune_from_meta(meta: &[(i32, Instant, bool)]) -> Option<i32> {
        if meta.is_empty() {
            return None;
        }

        let mut by_recency = meta.to_vec();
        by_recency.sort_by_key(|(_, last_used, _)| Reverse(*last_used));
        let protected: HashSet<i32> = by_recency
            .iter()
            .take(8)
            .map(|(process_id, _, _)| *process_id)
            .collect();

        let mut lru = meta.to_vec();
        lru.sort_by_key(|(_, last_used, _)| *last_used);

        if let Some((process_id, _, _)) = lru
            .iter()
            .find(|(process_id, _, exited)| !protected.contains(process_id) && *exited)
        {
            return Some(*process_id);
        }

        lru.into_iter()
            .find(|(process_id, _, _)| !protected.contains(process_id))
            .map(|(process_id, _, _)| process_id)
    }

    pub(crate) async fn terminate_all_processes(&self) {
        let candidates = {
            let store = self.process_store.lock().await;
            store
                .processes
                .values()
                .map(|entry| {
                    let stop_guard = entry
                        .purpose
                        .monitor_info()
                        .map(|_| entry.process.begin_monitor_stop());
                    (
                        entry.process_id,
                        Arc::clone(&entry.process),
                        entry.purpose.clone(),
                        process_has_fully_exited_for_removal(entry),
                        stop_guard,
                    )
                })
                .collect::<Vec<_>>()
        };
        let stopped = futures::future::join_all(candidates.iter().map(
            |(_, process, _, fully_exited, _)| async move {
                *fully_exited || process.terminate_confirmed().await.is_ok()
            },
        ))
        .await;

        for ((process_id, process, purpose, _, stop_guard), stopped) in
            candidates.into_iter().zip(stopped)
        {
            if !stopped {
                drop(stop_guard);
                continue;
            }
            purpose.request_monitor_stop(MonitorStopReason::SessionShutdown);
            let entry = {
                let mut store = self.process_store.lock().await;
                if let Some(task_id) = purpose.monitor_info().map(|info| info.task_id.clone()) {
                    store
                        .monitor_statuses
                        .insert(task_id.clone(), MonitorTaskStatus::Killed);
                    store.monitor_tasks.remove(&task_id);
                }
                self.remove_pending_monitor_process_if_matches(process_id, &process);
                let matches = store
                    .processes
                    .get(&process_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.process, &process));
                matches.then(|| store.remove(process_id)).flatten()
            };
            drop(stop_guard);
            if let Some(entry) = entry {
                unregister_network_approval_for_entry(&entry).await;
            }
        }

        // Processes cancelled before durable registration have no ProcessEntry,
        // but the manager-owned pending registry keeps them terminable and
        // prevents process-id reuse.
        let pending_candidates = {
            let store = self.process_store.lock().await;
            self.pending_monitor_processes_snapshot()
                .into_iter()
                .filter(|(process_id, process)| {
                    !store
                        .processes
                        .get(process_id)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.process, process))
                })
                .collect::<Vec<_>>()
        };
        let stopped = futures::future::join_all(
            pending_candidates
                .iter()
                .map(|(_, process)| async move { process.terminate_confirmed().await.is_ok() }),
        )
        .await;
        for ((process_id, process), stopped) in pending_candidates.into_iter().zip(stopped) {
            if stopped {
                self.release_process_id_if_matches(process_id, &process)
                    .await;
            }
        }
    }

    /// Gives monitors whose first session-shutdown termination attempt failed
    /// one checked retry before archive cleanup decides which task state must
    /// remain intact. The stop guard makes the retry mutually exclusive with a
    /// natural-completion claim.
    async fn retry_unconfirmed_monitor_terminations_for_cleanup(&self) {
        let candidates = {
            let store = self.process_store.lock().await;
            store
                .monitor_tasks
                .iter()
                .map(|(task_id, task)| {
                    (
                        task_id.clone(),
                        task.clone(),
                        task.process.begin_monitor_stop(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let stopped = futures::future::join_all(candidates.iter().map(|(_, task, _)| async move {
            monitor_process_has_fully_exited(&task.process)
                || task.process.terminate_confirmed().await.is_ok()
        }))
        .await;

        for ((task_id, task, stop_guard), stopped) in candidates.into_iter().zip(stopped) {
            if !stopped {
                warn!(
                    task_id = %task_id,
                    "command monitor remained live after shutdown termination retry"
                );
                drop(stop_guard);
                continue;
            }

            task.purpose
                .request_monitor_stop(MonitorStopReason::SessionShutdown);
            let entry = {
                let mut store = self.process_store.lock().await;
                let matches = store
                    .monitor_tasks
                    .get(&task_id)
                    .is_some_and(|registered| Arc::ptr_eq(&registered.process, &task.process));
                if !matches {
                    None
                } else {
                    store
                        .monitor_statuses
                        .insert(task_id.clone(), MonitorTaskStatus::Killed);
                    store.monitor_tasks.remove(&task_id);

                    store
                        .processes
                        .get(&task.process_id)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.process, &task.process))
                        .then(|| store.remove(task.process_id))
                        .flatten()
                }
            };
            drop(stop_guard);
            if let Some(entry) = entry {
                unregister_network_approval_for_entry(&entry).await;
            }
        }

        let pending = self.pending_monitor_processes_snapshot();
        let stopped = futures::future::join_all(
            pending
                .iter()
                .map(|(_, process)| async move { process.terminate_confirmed().await.is_ok() }),
        )
        .await;
        for ((process_id, process), stopped) in pending.into_iter().zip(stopped) {
            if stopped {
                self.release_process_id_if_matches(process_id, &process)
                    .await;
            } else {
                warn!(
                    process_id,
                    "pending command monitor remained live after shutdown termination retry"
                );
            }
        }
    }

    /// Remove command-monitor archives after the session has finished using
    /// their model-visible paths. This is intentionally separate from
    /// `terminate_all_processes`: the background-terminal clean API may run
    /// while the session is still active.
    pub(crate) async fn cleanup_monitor_output_files(&self) {
        self.retry_unconfirmed_monitor_terminations_for_cleanup()
            .await;

        let candidates = {
            let store = self.process_store.lock().await;
            // Any remaining registration was not confirmed stopped, regardless
            // of its diagnostic status. A running status without a registration
            // is also retained conservatively so cleanup never turns a partial
            // startup into an untracked process.
            let retained_task_ids = store
                .monitor_tasks
                .keys()
                .cloned()
                .chain(
                    store
                        .monitor_statuses
                        .iter()
                        .filter(|(_, status)| **status == MonitorTaskStatus::Running)
                        .map(|(task_id, _)| task_id.clone()),
                )
                .collect::<HashSet<_>>();

            let removable_task_ids = store
                .monitor_output_files
                .keys()
                .chain(store.monitor_workers.keys())
                .filter(|task_id| !retained_task_ids.contains(*task_id))
                .cloned()
                .collect::<HashSet<_>>();
            removable_task_ids
                .into_iter()
                .map(|task_id| {
                    let output_file = store.monitor_output_files.get(&task_id).cloned();
                    let worker = store.monitor_workers.get(&task_id).cloned();
                    (task_id, output_file, worker)
                })
                .collect::<Vec<_>>()
        };
        let wait_for_workers = async {
            for (_, _, worker) in &candidates {
                if let Some(worker) = worker {
                    worker.done.cancelled().await;
                }
            }
        };
        if tokio::time::timeout(MONITOR_WORKER_SHUTDOWN_GRACE_PERIOD, wait_for_workers)
            .await
            .is_err()
        {
            warn!("timed out waiting for command monitor workers during session shutdown");
            for (_, _, worker) in &candidates {
                if let Some(worker) = worker
                    && !worker.done.is_cancelled()
                {
                    worker.abort_handle.abort();
                }
            }
            let wait_after_abort = async {
                for (_, _, worker) in &candidates {
                    if let Some(worker) = worker {
                        worker.done.cancelled().await;
                    }
                }
            };
            if tokio::time::timeout(MONITOR_WORKER_SHUTDOWN_GRACE_PERIOD, wait_after_abort)
                .await
                .is_err()
            {
                warn!("command monitor workers remained live after bounded abort wait");
            }
        }

        for (task_id, output_file, worker) in candidates {
            if worker
                .as_ref()
                .is_some_and(|worker| !worker.done.is_cancelled())
            {
                continue;
            }
            let file_removed = match output_file.as_ref() {
                None => true,
                Some(path) => match tokio::fs::remove_file(path).await {
                    Ok(()) => true,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
                    Err(err) => {
                        warn!(path = %path.display(), error = %err, "failed to remove command monitor output archive");
                        false
                    }
                },
            };
            let mut store = self.process_store.lock().await;
            if let Some(expected_worker) = worker.as_ref()
                && store
                    .monitor_workers
                    .get(&task_id)
                    .is_some_and(|current| Arc::ptr_eq(current, expected_worker))
            {
                store.monitor_workers.remove(&task_id);
            }
            if file_removed
                && output_file.as_ref().is_some_and(|expected| {
                    store.monitor_output_files.get(&task_id) == Some(expected)
                })
            {
                store.monitor_output_files.remove(&task_id);
            }
        }

        let output_dirs = {
            let store = self.process_store.lock().await;
            let retained_dirs = store
                .monitor_output_files
                .values()
                .filter_map(|path| path.parent().map(PathBuf::from))
                .collect::<HashSet<_>>();
            store
                .monitor_output_dirs
                .iter()
                .filter(|path| !retained_dirs.contains(*path))
                .cloned()
                .collect::<Vec<_>>()
        };
        for output_dir in output_dirs {
            let removed = match tokio::fs::remove_dir(&output_dir).await {
                Ok(()) => true,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
                Err(err) => {
                    warn!(path = %output_dir.display(), error = %err, "failed to remove command monitor archive directory");
                    false
                }
            };
            if removed {
                self.process_store
                    .lock()
                    .await
                    .monitor_output_dirs
                    .remove(&output_dir);
            }
        }
    }

    pub(crate) async fn list_processes(&self) -> Vec<BackgroundTerminalInfo> {
        let mut entries = {
            let store = self.process_store.lock().await;
            store
                .processes
                .values()
                .filter(|entry| !process_has_fully_exited_for_removal(entry))
                .map(|entry| {
                    (
                        entry.process_id,
                        entry.call_id.clone(),
                        entry.hook_command.clone(),
                        entry.cwd.clone(),
                        entry.purpose.monitor_info().cloned(),
                        Arc::clone(&entry.process),
                    )
                })
                .collect::<Vec<_>>()
        };
        entries.sort_by_key(|(process_id, ..)| *process_id);

        let mut terminals = Vec::with_capacity(entries.len());
        for (process_id, item_id, command, cwd, monitor, process) in entries {
            let output = if monitor.is_some() {
                let output = process.output_handles().output_buffer.lock().await;
                let bytes_total = u64::try_from(output.total_bytes()).unwrap_or(u64::MAX);
                let tail = output.tail_bytes(BACKGROUND_TERMINAL_OUTPUT_TAIL_BYTES);
                let truncated = bytes_total > u64::try_from(tail.len()).unwrap_or(u64::MAX);
                Some(BackgroundTerminalOutput {
                    tail,
                    bytes_total,
                    truncated,
                })
            } else {
                None
            };

            terminals.push(BackgroundTerminalInfo {
                item_id,
                process_id: process_id.to_string(),
                command,
                cwd,
                monitor,
                output,
            });
        }
        terminals
    }

    pub(crate) async fn terminate_monitor(&self, task_id: &str) -> TerminateMonitorResult {
        let candidate = {
            let store = self.process_store.lock().await;
            match store.monitor_statuses.get(task_id).copied() {
                Some(MonitorTaskStatus::Running) => {}
                Some(status) => return TerminateMonitorResult::NotRunning(status),
                None => return TerminateMonitorResult::NotFound,
            }
            store.monitor_tasks.get(task_id).and_then(|task| {
                let info = task.purpose.monitor_info()?.clone();
                Some((task.clone(), info, task.process.begin_monitor_stop()))
            })
        };
        let Some((task, info, stop_guard)) = candidate else {
            return TerminateMonitorResult::NotFound;
        };
        let monitor_fully_finished = monitor_process_has_fully_exited(&task.process);
        if !monitor_fully_finished && task.process.terminate_confirmed().await.is_err() {
            drop(stop_guard);
            return TerminateMonitorResult::StopFailed;
        }

        task.purpose.request_monitor_stop(MonitorStopReason::User);
        let entry = {
            let mut store = self.process_store.lock().await;
            if !store
                .monitor_tasks
                .get(task_id)
                .is_some_and(|registered| Arc::ptr_eq(&registered.process, &task.process))
            {
                None
            } else {
                store
                    .monitor_statuses
                    .insert(task_id.to_string(), MonitorTaskStatus::Killed);
                store.monitor_tasks.remove(task_id);
                self.remove_pending_monitor_process_if_matches(task.process_id, &task.process);
                let removable = store
                    .processes
                    .get(&task.process_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.process, &task.process));
                removable.then(|| store.remove(task.process_id)).flatten()
            }
        };
        drop(stop_guard);
        if let Some(entry) = entry {
            unregister_network_approval_for_entry(&entry).await;
        }
        TerminateMonitorResult::Stopped {
            info,
            command: task.command,
        }
    }

    /// Atomically claims completion before publishing its terminal notification.
    /// A concurrent stop attempt returns `StopPending`; the worker waits for that
    /// attempt to resolve and retries if the task is still running.
    pub(crate) async fn claim_monitor_completion(
        &self,
        task_id: &str,
        expected_process: &Arc<UnifiedExecProcess>,
        status: MonitorTaskStatus,
    ) -> MonitorCompletionClaim {
        let entry = {
            let mut store = self.process_store.lock().await;
            if store.monitor_statuses.get(task_id) != Some(&MonitorTaskStatus::Running) {
                return MonitorCompletionClaim::NotRunning;
            }
            let Some(task) = store.monitor_tasks.get(task_id) else {
                return MonitorCompletionClaim::NotRunning;
            };
            if !Arc::ptr_eq(&task.process, expected_process) {
                return MonitorCompletionClaim::NotRunning;
            }
            if expected_process.is_monitor_stop_pending() {
                return MonitorCompletionClaim::StopPending;
            }
            let process_id = task.process_id;
            store.monitor_tasks.remove(task_id);
            let entry = store
                .processes
                .get(&process_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.process, expected_process))
                .then(|| store.remove(process_id))
                .flatten();
            store.monitor_statuses.insert(task_id.to_string(), status);
            entry
        };
        if let Some(entry) = entry {
            unregister_network_approval_for_entry(&entry).await;
        }
        MonitorCompletionClaim::Claimed
    }

    /// Removes a monitor that has reached a terminal state without requiring a
    /// later `write_stdin` poll. A pending stop is allowed to resolve first, then
    /// the task/process identity and running authority are checked again before
    /// this fallback may publish a terminal status.
    pub(crate) async fn reap_monitor(
        &self,
        task_id: &str,
        expected_process: &Arc<UnifiedExecProcess>,
        status: MonitorTaskStatus,
    ) {
        let entry = loop {
            {
                let mut store = self.process_store.lock().await;
                if store.monitor_statuses.get(task_id) != Some(&MonitorTaskStatus::Running) {
                    return;
                }
                let Some(process_id) = store
                    .monitor_tasks
                    .get(task_id)
                    .filter(|task| Arc::ptr_eq(&task.process, expected_process))
                    .map(|task| task.process_id)
                else {
                    return;
                };
                if !expected_process.is_monitor_stop_pending() {
                    store.monitor_tasks.remove(task_id);
                    let entry = store
                        .processes
                        .get(&process_id)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.process, expected_process))
                        .then(|| store.remove(process_id))
                        .flatten();
                    store.monitor_statuses.insert(task_id.to_string(), status);
                    break entry;
                }
            }
            expected_process.wait_for_monitor_stop_resolution().await;
        };
        if let Some(entry) = entry {
            unregister_network_approval_for_entry(&entry).await;
        }
    }

    pub(crate) async fn terminate_process(&self, process_id: i32) -> bool {
        let (process, already_exited, purpose, monitor_stop_guard) = {
            let store = self.process_store.lock().await;
            let Some(entry) = store.processes.get(&process_id) else {
                return false;
            };
            let process = Arc::clone(&entry.process);
            // A monitor root can exit while descendants still own the PTY
            // pipes. In that state output capture is still live and the
            // process group must be terminated rather than treated as done.
            let already_exited = if entry.purpose.monitor_info().is_some() {
                monitor_process_has_fully_exited(&entry.process)
            } else {
                entry.process.has_exited()
            };
            let purpose = entry.purpose.clone();
            let monitor_stop_guard = purpose.monitor_info().map(|_| process.begin_monitor_stop());
            (process, already_exited, purpose, monitor_stop_guard)
        };

        if !already_exited
            && terminate_for_process_purpose(&process, &purpose)
                .await
                .is_err()
        {
            drop(monitor_stop_guard);
            return false;
        }
        purpose.request_monitor_stop(MonitorStopReason::User);
        let entry = {
            let mut store = self.process_store.lock().await;
            if let Some(task_id) = purpose.monitor_info().map(|info| info.task_id.clone())
                && store
                    .monitor_tasks
                    .get(&task_id)
                    .is_some_and(|task| Arc::ptr_eq(&task.process, &process))
            {
                store
                    .monitor_statuses
                    .insert(task_id.clone(), MonitorTaskStatus::Killed);
                store.monitor_tasks.remove(&task_id);
            }
            self.remove_pending_monitor_process_if_matches(process_id, &process);
            let removable = store.processes.get(&process_id).is_some_and(|entry| {
                Arc::ptr_eq(&entry.process, &process)
                    && (entry.purpose.monitor_info().is_some()
                        || !entry.initial_exec_command_active.load(Ordering::Acquire))
            });
            removable.then(|| store.remove(process_id)).flatten()
        };
        drop(monitor_stop_guard);
        if let Some(entry) = entry {
            unregister_network_approval_for_entry(&entry).await;
        }
        true
    }
}

enum ProcessStatus {
    Alive {
        exit_code: Option<i32>,
        call_id: String,
        process_id: i32,
    },
    Exited {
        exit_code: Option<i32>,
        entry: Box<ProcessEntry>,
    },
    Unknown,
}

#[cfg(test)]
#[path = "process_manager_tests.rs"]
mod tests;
