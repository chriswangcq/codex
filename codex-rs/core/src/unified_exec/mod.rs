//! Unified Exec: interactive process execution orchestrated with approvals + sandboxing.
//!
//! Responsibilities
//! - Manages interactive processes (create, reuse, buffer output with caps).
//! - Uses the shared ToolOrchestrator to handle approval, sandbox selection, and
//!   retry semantics in a single, descriptive flow.
//! - Spawns the PTY from a sandbox-transformed `ExecRequest`; on sandbox denial,
//!   retries without sandbox when policy allows (no re‑prompt thanks to caching).
//! - Uses the shared `is_likely_sandbox_denied` heuristic to keep denial messages
//!   consistent with other exec paths.
//!
//! Flow at a glance (open process)
//! 1) Build a small request `{ command, cwd }`.
//! 2) Orchestrator: approval (bypass/cache/prompt) → select sandbox → run.
//! 3) Runtime: transform `SandboxTransformRequest` -> `ExecRequest` -> spawn PTY.
//! 4) If denial, orchestrator retries with `SandboxType::None`.
//! 5) Process handle is returned with streaming output + metadata.
//!
//! This keeps policy logic and user interaction centralized while the PTY/process
//! concerns remain isolated here. The implementation is split between:
//! - `process.rs`: PTY process lifecycle + output buffering.
//! - `process_state.rs`: shared exit/failure state for local and remote processes.
//! - `process_manager.rs`: orchestration (approvals, sandboxing, reuse) and request handling.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_network_proxy::NetworkProxy;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::CommandMonitorInfo;
use codex_tools::UnifiedExecShellMode;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use rand::Rng;
use rand::rng;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::network_approval::DeferredNetworkApproval;

mod async_watcher;
mod errors;
mod head_tail_buffer;
mod process;
pub(crate) use process::MonitorCaptureReceiver;
pub(crate) use process::MonitorOutputChunk;
pub(crate) use process::MonitorStreamOutput;
pub(crate) use process::OutputHandles;
#[cfg(test)]
pub(crate) use process::monitor_capture_channel;
mod process_manager;
mod process_state;

pub(crate) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    process_manager::set_deterministic_process_ids_for_tests(enabled);
}

pub(crate) use errors::UnifiedExecError;
pub(crate) use process::NoopSpawnLifecycle;
#[cfg(unix)]
pub(crate) use process::SpawnLifecycle;
pub(crate) use process::SpawnLifecycleHandle;
pub(crate) use process::UnifiedExecProcess;

pub(crate) const MIN_YIELD_TIME_MS: u64 = 250;
pub(crate) const WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS: u64 = 10_000;
// Minimum yield time for an empty `write_stdin`.
pub(crate) const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub(crate) const MAX_YIELD_TIME_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS: u64 = 300_000;
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_TOKENS: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 4;
pub(crate) const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;
pub(crate) const MAX_MONITOR_ARCHIVE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Session-scoped budget for command-monitor archive content. Per-task writers
/// retain their own 5 GiB cap, while this shared counter prevents completed
/// monitor archives from accumulating that cap once per task.
pub(crate) struct MonitorArchiveBudget {
    content_bytes: AtomicU64,
    cap: u64,
}

impl MonitorArchiveBudget {
    fn new(cap: u64) -> Self {
        Self {
            content_bytes: AtomicU64::new(0),
            cap,
        }
    }

    pub(crate) fn reserve(self: &Arc<Self>, amount: u64) -> Option<MonitorArchiveReservation> {
        let mut current = self.content_bytes.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(amount)?;
            if next > self.cap {
                return None;
            }
            match self.content_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(MonitorArchiveReservation {
                        budget: Arc::clone(self),
                        uncommitted_bytes: amount,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn with_cap(cap: u64) -> Arc<Self> {
        Arc::new(Self::new(cap))
    }

    #[cfg(test)]
    pub(crate) fn content_bytes(&self) -> u64 {
        self.content_bytes.load(Ordering::Acquire)
    }
}

impl Default for MonitorArchiveBudget {
    fn default() -> Self {
        Self::new(MAX_MONITOR_ARCHIVE_BYTES)
    }
}

pub(crate) struct MonitorArchiveReservation {
    budget: Arc<MonitorArchiveBudget>,
    uncommitted_bytes: u64,
}

impl MonitorArchiveReservation {
    /// Records bytes that actually reached the archive. Any reservation left
    /// uncommitted is returned to the session budget on drop.
    pub(crate) fn commit_written(&mut self, written: u64) {
        debug_assert!(written <= self.uncommitted_bytes);
        self.uncommitted_bytes = self.uncommitted_bytes.saturating_sub(written);
    }
}

impl Drop for MonitorArchiveReservation {
    fn drop(&mut self) {
        if self.uncommitted_bytes != 0 {
            self.budget
                .content_bytes
                .fetch_sub(self.uncommitted_bytes, Ordering::AcqRel);
        }
    }
}

pub(crate) struct UnifiedExecContext {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
}

impl UnifiedExecContext {
    pub fn new(session: Arc<Session>, turn: Arc<TurnContext>, call_id: String) -> Self {
        Self {
            session,
            turn,
            call_id,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExecCommandRequest {
    pub command: Vec<String>,
    pub shell_type: ShellType,
    pub hook_command: String,
    pub process_id: i32,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub cwd: PathUri,
    pub sandbox_cwd: PathUri,
    pub turn_environment: TurnEnvironment,
    pub shell_mode: UnifiedExecShellMode,
    pub network: Option<NetworkProxy>,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub additional_permissions_preapproved: bool,
    pub justification: Option<String>,
    pub prefix_rule: Option<Vec<String>>,
    pub monitor: Option<CommandMonitorInfo>,
}

#[derive(Debug)]
pub(crate) struct WriteStdinRequest<'a> {
    pub process_id: i32,
    pub input: &'a str,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub truncation_policy: TruncationPolicy,
    pub interaction_event: Option<WriteStdinInteractionEvent<'a>>,
}

pub(crate) struct WriteStdinInteractionEvent<'a> {
    pub session: &'a Arc<Session>,
    pub turn: &'a Arc<TurnContext>,
}

impl std::fmt::Debug for WriteStdinInteractionEvent<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WriteStdinInteractionEvent")
    }
}

#[derive(Default)]
pub(crate) struct ProcessStore {
    processes: HashMap<i32, ProcessEntry>,
    reserved_process_ids: HashSet<i32>,
    monitor_statuses: HashMap<String, MonitorTaskStatus>,
    monitor_output_files: HashMap<String, PathBuf>,
    monitor_output_dirs: HashSet<PathBuf>,
    monitor_workers: HashMap<String, Arc<MonitorWorkerControl>>,
    monitor_tasks: HashMap<String, MonitorTaskRegistration>,
}

impl ProcessStore {
    fn remove(&mut self, process_id: i32) -> Option<ProcessEntry> {
        self.reserved_process_ids.remove(&process_id);
        self.processes.remove(&process_id)
    }
}

pub(crate) struct UnifiedExecProcessManager {
    process_store: Mutex<ProcessStore>,
    /// Owns monitor processes from spawn until their durable ProcessEntry or
    /// monitor-task registration is committed. Failed checked termination also
    /// remains here so shutdown can retry without losing the last Arc.
    pending_monitor_processes: StdMutex<HashMap<i32, Arc<UnifiedExecProcess>>>,
    max_write_stdin_yield_time_ms: u64,
    monitor_archive_budget: Arc<MonitorArchiveBudget>,
}

impl UnifiedExecProcessManager {
    pub(crate) fn new(max_write_stdin_yield_time_ms: u64) -> Self {
        Self {
            process_store: Mutex::new(ProcessStore::default()),
            pending_monitor_processes: StdMutex::new(HashMap::new()),
            max_write_stdin_yield_time_ms: max_write_stdin_yield_time_ms
                .max(MIN_EMPTY_YIELD_TIME_MS),
            monitor_archive_budget: Arc::new(MonitorArchiveBudget::default()),
        }
    }
}

impl Default for UnifiedExecProcessManager {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
    }
}

struct ProcessEntry {
    process: Arc<UnifiedExecProcess>,
    call_id: String,
    process_id: i32,
    cwd: PathUri,
    initial_exec_command_active: Arc<std::sync::atomic::AtomicBool>,
    hook_command: String,
    tty: bool,
    network_approval: Option<DeferredNetworkApproval>,
    session: Weak<Session>,
    last_used: tokio::time::Instant,
    purpose: ProcessPurpose,
}

struct MonitorWorkerControl {
    done: CancellationToken,
    abort_handle: tokio::task::AbortHandle,
}

#[derive(Clone)]
struct MonitorTaskRegistration {
    process: Arc<UnifiedExecProcess>,
    process_id: i32,
    command: String,
    purpose: ProcessPurpose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MonitorStopReason {
    User,
    SessionShutdown,
    Capacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MonitorTaskStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

impl std::fmt::Display for MonitorTaskStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
        })
    }
}

pub(crate) enum TerminateMonitorResult {
    Stopped {
        info: CommandMonitorInfo,
        command: String,
    },
    StopFailed,
    NotRunning(MonitorTaskStatus),
    NotFound,
}

#[derive(Clone)]
enum ProcessPurpose {
    Terminal,
    Monitor {
        info: CommandMonitorInfo,
        stop_tx: watch::Sender<Option<MonitorStopReason>>,
    },
}

impl ProcessPurpose {
    fn monitor_info(&self) -> Option<&CommandMonitorInfo> {
        match self {
            Self::Terminal => None,
            Self::Monitor { info, .. } => Some(info),
        }
    }

    fn request_monitor_stop(&self, reason: MonitorStopReason) {
        if let Self::Monitor { stop_tx, .. } = self {
            let _ = stop_tx.send(Some(reason));
        }
    }
}

pub(crate) fn clamp_yield_time(yield_time_ms: u64) -> u64 {
    let yield_time_ms = if cfg!(windows) {
        yield_time_ms.max(WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS)
    } else {
        yield_time_ms
    };
    yield_time_ms.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
}

pub(crate) fn resolve_max_tokens(max_tokens: Option<usize>) -> usize {
    max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

pub(crate) fn format_output_omission_marker(omitted_bytes: usize) -> String {
    format!("... {omitted_bytes} bytes omitted ...")
}

pub(crate) fn generate_chunk_id() -> String {
    let mut rng = rng();
    (0..6)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

#[cfg(test)]
#[cfg(unix)]
#[path = "process_tests.rs"]
mod process_tests;
#[cfg(test)]
#[cfg(unix)]
pub(crate) use process::TERMINATE_CONFIRMATION_TIMEOUT;
#[cfg(test)]
#[cfg(unix)]
pub(crate) use process_tests::blocking_terminate_remote_process;
#[cfg(test)]
#[cfg(unix)]
#[path = "mod_tests.rs"]
mod tests;
