#![allow(clippy::module_inception)]

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use codex_exec_server::ExecOutputStream;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEvent;
use codex_exec_server::ProcessSignal as ExecServerProcessSignal;
use codex_exec_server::ReadResponse as ExecReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteStatus;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::TruncationPolicy;
use codex_sandboxing::SandboxType;
use codex_sandboxing::is_likely_sandbox_denied;
use codex_sandboxing::record_filesystem_sandbox_violation;
use codex_utils_output_truncation::formatted_truncate_text;
use codex_utils_pty::ExecCommandSession;
use codex_utils_pty::ProcessSignal as PtyProcessSignal;
use codex_utils_pty::SpawnedPty;

use super::UNIFIED_EXEC_OUTPUT_MAX_TOKENS;
use super::UnifiedExecError;
use super::head_tail_buffer::HeadTailBuffer;
use super::process_state::ProcessState;

const EARLY_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(150);
const MONITOR_OUTPUT_DRAIN_GRACE_PERIOD: Duration = Duration::from_millis(500);
const MONITOR_CAPTURE_CHANNEL_CAPACITY: usize = 128;
pub(crate) const TERMINATE_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) trait SpawnLifecycle: std::fmt::Debug + Send + Sync {
    /// Returns file descriptors that must stay open across the child `exec()`.
    ///
    /// The returned descriptors must already be valid in the parent process and
    /// stay valid until `after_spawn()` runs, which is the first point where
    /// the parent may release its copies.
    fn inherited_fds(&self) -> Vec<i32> {
        Vec::new()
    }

    fn after_spawn(&mut self) {}
}

pub(crate) type SpawnLifecycleHandle = Box<dyn SpawnLifecycle>;

#[derive(Debug, Default)]
/// Spawn lifecycle that performs no extra setup around process launch.
pub(crate) struct NoopSpawnLifecycle;

impl SpawnLifecycle for NoopSpawnLifecycle {}

pub(crate) type OutputBuffer = Arc<Mutex<HeadTailBuffer>>;

#[derive(Clone)]
pub(crate) struct MonitorStreamOutput {
    combined: OutputBuffer,
    stdout: OutputBuffer,
    stderr: OutputBuffer,
}

impl MonitorStreamOutput {
    pub(crate) fn new(combined: OutputBuffer) -> Self {
        Self {
            combined,
            stdout: Arc::new(Mutex::new(HeadTailBuffer::default())),
            stderr: Arc::new(Mutex::new(HeadTailBuffer::default())),
        }
    }

    async fn push_chunk(&self, bytes: Vec<u8>, is_stdout: bool) {
        let buffer = if is_stdout {
            &self.stdout
        } else {
            &self.stderr
        };
        buffer.lock().await.push_chunk(bytes);
    }

    pub(crate) async fn snapshot(&self) -> (String, String, String) {
        let stdout = self.stdout.lock().await.to_bytes_with_omission_marker();
        let stderr = self.stderr.lock().await.to_bytes_with_omission_marker();
        let combined = self.combined.lock().await.to_bytes_with_omission_marker();
        (
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
            String::from_utf8_lossy(&combined).into_owned(),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MonitorOutputChunk {
    Output { bytes: Vec<u8>, is_stdout: bool },
    StdoutClosed,
    ArchiveGap,
}

const MONITOR_CAPTURE_GAP_CLEAN: u8 = 0;
const MONITOR_CAPTURE_GAP_PENDING: u8 = 1;
const MONITOR_CAPTURE_GAP_REPORTED: u8 = 2;

#[derive(Debug, Default)]
struct MonitorCaptureGap {
    state: AtomicU8,
}

impl MonitorCaptureGap {
    fn record(&self) {
        let _ = self.state.compare_exchange(
            MONITOR_CAPTURE_GAP_CLEAN,
            MONITOR_CAPTURE_GAP_PENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn take_marker(&self) -> bool {
        self.state
            .compare_exchange(
                MONITOR_CAPTURE_GAP_PENDING,
                MONITOR_CAPTURE_GAP_REPORTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

#[derive(Clone)]
pub(crate) struct MonitorCaptureSender {
    sender: mpsc::Sender<MonitorOutputChunk>,
    gap: Arc<MonitorCaptureGap>,
}

pub(crate) struct MonitorCaptureReceiver {
    receiver: mpsc::Receiver<MonitorOutputChunk>,
    gap: Arc<MonitorCaptureGap>,
}

#[cfg(test)]
impl MonitorCaptureSender {
    pub(crate) async fn send(
        &self,
        chunk: MonitorOutputChunk,
    ) -> Result<(), mpsc::error::SendError<MonitorOutputChunk>> {
        self.sender.send(chunk).await
    }
}

pub(crate) fn monitor_capture_channel(
    capacity: usize,
) -> (MonitorCaptureSender, MonitorCaptureReceiver) {
    let (sender, receiver) = mpsc::channel(capacity);
    let gap = Arc::new(MonitorCaptureGap::default());
    (
        MonitorCaptureSender {
            sender,
            gap: Arc::clone(&gap),
        },
        MonitorCaptureReceiver { receiver, gap },
    )
}

impl MonitorCaptureReceiver {
    pub(crate) fn try_recv(&mut self) -> Result<MonitorOutputChunk, mpsc::error::TryRecvError> {
        match self.receiver.try_recv() {
            Ok(chunk) => Ok(chunk),
            Err(_) if self.gap.take_marker() => Ok(MonitorOutputChunk::ArchiveGap),
            Err(err) => Err(err),
        }
    }

    pub(crate) async fn recv(&mut self) -> Option<MonitorOutputChunk> {
        match self.receiver.try_recv() {
            Ok(chunk) => return Some(chunk),
            Err(mpsc::error::TryRecvError::Empty) => {}
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return self
                    .gap
                    .take_marker()
                    .then_some(MonitorOutputChunk::ArchiveGap);
            }
        }
        if self.gap.take_marker() {
            return Some(MonitorOutputChunk::ArchiveGap);
        }
        match self.receiver.recv().await {
            Some(chunk) => Some(chunk),
            None => self
                .gap
                .take_marker()
                .then_some(MonitorOutputChunk::ArchiveGap),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_full(&self) -> bool {
        self.receiver.capacity() == 0
    }
}

async fn send_monitor_capture(
    monitor_capture: &Arc<StdMutex<Option<MonitorCaptureSender>>>,
    monitor_stop_pending: &Arc<AtomicUsize>,
    monitor_stop_pending_notify: &Arc<Notify>,
    chunk: MonitorOutputChunk,
) {
    let capture = monitor_capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .cloned();
    let Some(capture) = capture else {
        return;
    };

    loop {
        if monitor_stop_pending.load(Ordering::Acquire) != 0 {
            // Once a stop starts, process termination must not depend on the
            // archive consumer making room in this bounded channel. Preserve
            // an explicit, once-per-monitor gap signal if output cannot fit.
            if matches!(
                capture.sender.try_send(chunk),
                Err(mpsc::error::TrySendError::Full(_))
            ) {
                capture.gap.record();
            }
            return;
        }

        let stop_changed = monitor_stop_pending_notify.notified();
        tokio::pin!(stop_changed);
        stop_changed.as_mut().enable();
        if monitor_stop_pending.load(Ordering::Acquire) != 0 {
            continue;
        }

        tokio::select! {
            permit = capture.sender.reserve() => {
                if let Ok(permit) = permit {
                    permit.send(chunk);
                }
                return;
            }
            _ = &mut stop_changed => {}
        }
    }
}

/// Shared output state exposed to polling and streaming consumers.
#[derive(Clone)]
pub(crate) struct OutputHandles {
    pub(crate) output_buffer: OutputBuffer,
    pub(crate) output_notify: Arc<Notify>,
    pub(crate) monitor_capture: Arc<StdMutex<Option<MonitorCaptureSender>>>,
    pub(crate) monitor_capture_rx: Arc<StdMutex<Option<MonitorCaptureReceiver>>>,
    pub(crate) monitor_stream_output: Option<MonitorStreamOutput>,
    pub(crate) output_closed: Arc<AtomicBool>,
    pub(crate) output_closed_notify: Arc<Notify>,
    pub(crate) cancellation_token: CancellationToken,
}

impl OutputHandles {
    pub(crate) fn take_monitor_capture(&self) -> Option<MonitorCaptureReceiver> {
        self.monitor_capture_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

struct OutputTaskGuard {
    output_closed: Arc<AtomicBool>,
    output_closed_notify: Arc<Notify>,
    output_task_finished: Arc<AtomicBool>,
    output_task_finished_notify: Arc<Notify>,
    monitor_capture: Arc<StdMutex<Option<MonitorCaptureSender>>>,
    completed: bool,
}

impl OutputTaskGuard {
    fn mark_completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for OutputTaskGuard {
    fn drop(&mut self) {
        self.monitor_capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if self.completed {
            self.output_closed.store(true, Ordering::Release);
        }
        self.output_task_finished.store(true, Ordering::Release);
        self.output_closed_notify.notify_waiters();
        self.output_task_finished_notify.notify_waiters();
    }
}

/// Transport-specific process handle used by unified exec.
enum ProcessHandle {
    Local(Box<ExecCommandSession>),
    ExecServer(Arc<dyn ExecProcess>),
}

/// Unified wrapper over directly spawned PTY sessions and exec-server-backed
/// processes.
pub(crate) struct UnifiedExecProcess {
    process_handle: ProcessHandle,
    output_tx: broadcast::Sender<Vec<u8>>,
    output: OutputHandles,
    output_drained: Arc<Notify>,
    interaction_lock: Arc<Mutex<()>>,
    state_tx: watch::Sender<ProcessState>,
    state_rx: watch::Receiver<ProcessState>,
    output_task: Option<JoinHandle<()>>,
    output_task_finished: Arc<AtomicBool>,
    output_task_finished_notify: Arc<Notify>,
    monitor_stop_pending: Arc<AtomicUsize>,
    monitor_stop_pending_notify: Arc<Notify>,
    sandbox_type: SandboxType,
    _spawn_lifecycle: Option<SpawnLifecycleHandle>,
}

pub(crate) struct MonitorStopGuard {
    process: Arc<UnifiedExecProcess>,
}

impl Drop for MonitorStopGuard {
    fn drop(&mut self) {
        if self
            .process
            .monitor_stop_pending
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            self.process.monitor_stop_pending_notify.notify_waiters();
        }
    }
}

impl std::fmt::Debug for UnifiedExecProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedExecProcess")
            .field("has_exited", &self.has_exited())
            .field("exit_code", &self.exit_code())
            .field("sandbox_type", &self.sandbox_type)
            .finish_non_exhaustive()
    }
}

impl UnifiedExecProcess {
    fn new(
        process_handle: ProcessHandle,
        sandbox_type: SandboxType,
        spawn_lifecycle: Option<SpawnLifecycleHandle>,
        capture_monitor_output: bool,
    ) -> Self {
        let (monitor_capture, monitor_capture_rx) = if capture_monitor_output {
            let (tx, rx) = monitor_capture_channel(MONITOR_CAPTURE_CHANNEL_CAPACITY);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let output_buffer = Arc::new(Mutex::new(HeadTailBuffer::default()));
        let monitor_stream_output =
            capture_monitor_output.then(|| MonitorStreamOutput::new(Arc::clone(&output_buffer)));
        let output = OutputHandles {
            output_buffer,
            output_notify: Arc::new(Notify::new()),
            monitor_capture: Arc::new(StdMutex::new(monitor_capture)),
            monitor_capture_rx: Arc::new(StdMutex::new(monitor_capture_rx)),
            monitor_stream_output,
            output_closed: Arc::new(AtomicBool::new(false)),
            output_closed_notify: Arc::new(Notify::new()),
            cancellation_token: CancellationToken::new(),
        };
        let output_drained = Arc::new(Notify::new());
        let output_task_finished = Arc::new(AtomicBool::new(false));
        let output_task_finished_notify = Arc::new(Notify::new());
        let (output_tx, _) = broadcast::channel(64);
        let (state_tx, state_rx) = watch::channel(ProcessState::default());

        Self {
            process_handle,
            output_tx,
            output,
            output_drained,
            interaction_lock: Arc::new(Mutex::new(())),
            state_tx,
            state_rx,
            output_task: None,
            output_task_finished,
            output_task_finished_notify,
            monitor_stop_pending: Arc::new(AtomicUsize::new(0)),
            monitor_stop_pending_notify: Arc::new(Notify::new()),
            sandbox_type,
            _spawn_lifecycle: spawn_lifecycle,
        }
    }

    pub(super) async fn write(&self, data: &[u8]) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .writer_sender()
                .send(data.to_vec())
                .await
                .map_err(|_| UnifiedExecError::WriteToStdin),
            ProcessHandle::ExecServer(process_handle) => {
                match process_handle.write(data.to_vec()).await {
                    Ok(response) => match response.status {
                        WriteStatus::Accepted => Ok(()),
                        WriteStatus::UnknownProcess | WriteStatus::StdinClosed => {
                            let state = self.state_rx.borrow().clone();
                            let _ = self.state_tx.send_replace(state.exited(state.exit_code));
                            self.output.cancellation_token.cancel();
                            Err(UnifiedExecError::WriteToStdin)
                        }
                        WriteStatus::Starting => Err(UnifiedExecError::WriteToStdin),
                    },
                    Err(err) => Err(UnifiedExecError::process_failed(err.to_string())),
                }
            }
        }
    }

    pub(super) fn output_handles(&self) -> &OutputHandles {
        &self.output
    }

    pub(super) fn output_receiver(&self) -> tokio::sync::broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.output.cancellation_token.clone()
    }

    pub(crate) fn close_monitor_capture(&self) {
        self.output
            .monitor_capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    pub(super) fn output_drained_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.output_drained)
    }

    pub(super) fn interaction_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.interaction_lock)
    }

    pub(crate) fn has_exited(&self) -> bool {
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => state.has_exited || process_handle.has_exited(),
            ProcessHandle::ExecServer(_) => state.has_exited,
        }
    }

    pub(crate) fn output_task_finished(&self) -> bool {
        self.output_task_finished.load(Ordering::Acquire)
    }

    pub(crate) fn output_completed_normally(&self) -> bool {
        let core_output_closed = self.output.output_closed.load(Ordering::Acquire);
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => {
                core_output_closed && process_handle.output_closed()
            }
            ProcessHandle::ExecServer(_) => core_output_closed,
        }
    }

    pub(crate) fn exit_code(&self) -> Option<i32> {
        let state = self.state_rx.borrow().clone();
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => {
                state.exit_code.or_else(|| process_handle.exit_code())
            }
            ProcessHandle::ExecServer(_) => state.exit_code,
        }
    }

    fn finish_termination(&self) {
        self.output.cancellation_token.cancel();
        if let Some(output_task) = &self.output_task {
            let monitor_capture_active = self
                .output
                .monitor_capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some();
            if monitor_capture_active {
                let abort_handle = output_task.abort_handle();
                let output_task_finished = Arc::clone(&self.output_task_finished);
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        tokio::time::sleep(MONITOR_OUTPUT_DRAIN_GRACE_PERIOD).await;
                        if !output_task_finished.load(Ordering::Acquire) {
                            abort_handle.abort();
                        }
                    });
                } else {
                    abort_handle.abort();
                }
            } else {
                output_task.abort();
            }
        }
    }

    async fn wait_for_output_task_finished(&self) {
        loop {
            let notified = self.output_task_finished_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.output_task_finished() {
                return;
            }
            notified.await;
        }
    }

    pub(super) fn terminate(&self) {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                let process_handle = Arc::clone(process_handle);
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        let _ = process_handle.terminate().await;
                    });
                }
            }
        }
        self.finish_termination();
    }

    /// Requests termination using the historical background-terminal contract:
    /// a remote process is considered stopped once its executor acknowledges the
    /// request. Command monitors use [`Self::terminate_confirmed`] instead because
    /// they must retain ownership until both process exit and output closure are
    /// observed.
    pub(crate) async fn terminate_acknowledged(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle.terminate(),
            ProcessHandle::ExecServer(process_handle) => {
                process_handle
                    .terminate()
                    .await
                    .map_err(|err| UnifiedExecError::process_failed(err.to_string()))?;
            }
        }
        self.signal_exit(self.exit_code());
        self.finish_termination();
        Ok(())
    }

    pub(crate) async fn terminate_confirmed(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => {
                tokio::time::timeout(TERMINATE_CONFIRMATION_TIMEOUT, async {
                    process_handle
                        .try_request_terminate()
                        .map_err(|err| UnifiedExecError::process_failed(err.to_string()))?;
                    process_handle.wait_for_exit().await;
                    #[cfg(windows)]
                    process_handle.release_pty_handles_after_root_exit().await;
                    process_handle.wait_for_output_closed().await;
                    self.wait_for_output_task_finished().await;
                    process_handle
                        .confirm_termination()
                        .map_err(|err| UnifiedExecError::process_failed(err.to_string()))?;
                    Ok::<(), UnifiedExecError>(())
                })
                .await
                .map_err(|_| {
                    UnifiedExecError::process_failed(
                        "timed out waiting for process termination and output closure".to_string(),
                    )
                })??;
            }
            ProcessHandle::ExecServer(process_handle) => {
                tokio::time::timeout(TERMINATE_CONFIRMATION_TIMEOUT, async {
                    if let Some(message) = self.failure_message() {
                        return Err(UnifiedExecError::process_failed(message));
                    }
                    let mut state_rx = self.state_rx.clone();
                    let terminate_request = process_handle.terminate();
                    tokio::pin!(terminate_request);
                    let mut terminate_acknowledged = false;
                    loop {
                        let output_closed_notified = self.output.output_closed_notify.notified();
                        tokio::pin!(output_closed_notified);
                        output_closed_notified.as_mut().enable();
                        let state = state_rx.borrow().clone();
                        if let Some(message) = state.failure_message {
                            return Err(UnifiedExecError::process_failed(message));
                        }
                        if terminate_acknowledged
                            && state.has_exited
                            && self.output_completed_normally()
                            && self.output_task_finished()
                        {
                            break;
                        }
                        tokio::select! {
                            result = terminate_request.as_mut(), if !terminate_acknowledged => {
                                result.map_err(|err| {
                                    UnifiedExecError::process_failed(err.to_string())
                                })?;
                                terminate_acknowledged = true;
                            }
                            changed = state_rx.changed() => {
                                if changed.is_err() {
                                    return Err(UnifiedExecError::process_failed(
                                        "exec-server process state stream closed before termination was confirmed"
                                            .to_string(),
                                    ));
                                }
                            }
                            _ = &mut output_closed_notified => {}
                        }
                    }
                    Ok::<(), UnifiedExecError>(())
                })
                .await
                .map_err(|_| {
                    UnifiedExecError::process_failed(
                        "timed out waiting for confirmed process termination and output closure"
                            .to_string(),
                    )
                })??;
            }
        }
        self.signal_exit(self.exit_code());
        self.finish_termination();
        Ok(())
    }

    pub(super) async fn interrupt(&self) -> Result<(), UnifiedExecError> {
        match &self.process_handle {
            ProcessHandle::Local(process_handle) => process_handle
                .signal(PtyProcessSignal::Interrupt)
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
            ProcessHandle::ExecServer(process_handle) => process_handle
                .signal(ExecServerProcessSignal::Interrupt)
                .await
                .map_err(|err| UnifiedExecError::process_failed(err.to_string())),
        }
    }

    pub(super) fn fail_and_terminate(&self, message: String) {
        let state = self.state_rx.borrow().clone();
        if state.failure_message.is_none() {
            let _ = self.state_tx.send_replace(state.failed(message));
        }
        self.terminate();
    }

    async fn snapshot_output(&self) -> Vec<Vec<u8>> {
        let guard = self.output.output_buffer.lock().await;
        guard.snapshot_chunks()
    }

    pub(crate) fn sandbox_type(&self) -> SandboxType {
        self.sandbox_type
    }

    pub(crate) fn failure_message(&self) -> Option<String> {
        self.state_rx.borrow().failure_message.clone()
    }

    pub(crate) fn begin_monitor_stop(self: &Arc<Self>) -> MonitorStopGuard {
        self.monitor_stop_pending.fetch_add(1, Ordering::AcqRel);
        self.monitor_stop_pending_notify.notify_waiters();
        MonitorStopGuard {
            process: Arc::clone(self),
        }
    }

    pub(crate) async fn wait_for_monitor_stop_resolution(&self) {
        loop {
            let notified = self.monitor_stop_pending_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.monitor_stop_pending.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn is_monitor_stop_pending(&self) -> bool {
        self.monitor_stop_pending.load(Ordering::Acquire) != 0
    }

    pub(super) async fn check_for_sandbox_denial(&self) -> Result<(), UnifiedExecError> {
        let _ = tokio::time::timeout(
            Duration::from_millis(20),
            self.output.output_notify.notified(),
        )
        .await;

        let collected_chunks = self.snapshot_output().await;
        let mut aggregated: Vec<u8> = Vec::new();
        for chunk in collected_chunks {
            aggregated.extend_from_slice(&chunk);
        }
        let aggregated_text = String::from_utf8_lossy(&aggregated).to_string();
        self.check_for_sandbox_denial_with_text(&aggregated_text)
            .await?;

        Ok(())
    }

    pub(super) async fn check_for_sandbox_denial_with_text(
        &self,
        text: &str,
    ) -> Result<(), UnifiedExecError> {
        let executor_reported_denial = self.state_rx.borrow().sandbox_denied;
        let sandbox_type = self.sandbox_type();
        if !self.has_exited() || (!executor_reported_denial && sandbox_type == SandboxType::None) {
            return Ok(());
        }

        let exit_code = self.exit_code().unwrap_or(-1);
        let exec_output = ExecToolCallOutput {
            exit_code,
            stderr: StreamOutput::new(text.to_string()),
            aggregated_output: StreamOutput::new(text.to_string()),
            ..Default::default()
        };
        let likely_sandbox_denial = is_likely_sandbox_denied(sandbox_type, &exec_output);
        if likely_sandbox_denial {
            record_filesystem_sandbox_violation(sandbox_type, &exec_output);
        }
        if executor_reported_denial || likely_sandbox_denial {
            let snippet = formatted_truncate_text(
                text,
                TruncationPolicy::Tokens(UNIFIED_EXEC_OUTPUT_MAX_TOKENS),
            );
            let message = if snippet.is_empty() {
                format!("Process exited with code {exit_code}")
            } else {
                snippet
            };
            return Err(UnifiedExecError::sandbox_denied(message, exec_output));
        }
        Ok(())
    }

    pub(super) async fn from_spawned(
        spawned: SpawnedPty,
        sandbox_type: SandboxType,
        spawn_lifecycle: SpawnLifecycleHandle,
        capture_monitor_output: bool,
    ) -> Result<Self, UnifiedExecError> {
        let SpawnedPty {
            session: process_handle,
            stdout_rx,
            stderr_rx,
            mut exit_rx,
        } = spawned;
        let mut managed = Self::new(
            ProcessHandle::Local(Box::new(process_handle)),
            sandbox_type,
            Some(spawn_lifecycle),
            capture_monitor_output,
        );
        managed.output_task = Some(Self::spawn_local_output_task(
            stdout_rx,
            stderr_rx,
            managed.output_handles().clone(),
            managed.output_tx.clone(),
            Arc::clone(&managed.output_task_finished),
            Arc::clone(&managed.output_task_finished_notify),
            Arc::clone(&managed.monitor_stop_pending),
            Arc::clone(&managed.monitor_stop_pending_notify),
        ));

        match exit_rx.try_recv() {
            Ok(exit_code) => {
                managed.signal_exit(Some(exit_code));
                managed.check_for_sandbox_denial().await?;
                return Ok(managed);
            }
            Err(TryRecvError::Closed) => {
                managed.signal_exit(/*exit_code*/ None);
                managed.check_for_sandbox_denial().await?;
                return Ok(managed);
            }
            Err(TryRecvError::Empty) => {}
        }

        if let Ok(exit_result) = tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, &mut exit_rx).await {
            managed.signal_exit(exit_result.ok());
            managed.check_for_sandbox_denial().await?;
            return Ok(managed);
        }

        tokio::spawn({
            let state_tx = managed.state_tx.clone();
            let cancellation_token = managed.output.cancellation_token.clone();
            async move {
                let exit_code = exit_rx.await.ok();
                let state = state_tx.borrow().clone();
                let _ = state_tx.send_replace(state.exited(exit_code));
                cancellation_token.cancel();
            }
        });

        Ok(managed)
    }

    pub(super) async fn from_exec_server_started(
        started: StartedExecProcess,
        capture_monitor_output: bool,
    ) -> Result<Self, UnifiedExecError> {
        let process_handle = ProcessHandle::ExecServer(Arc::clone(&started.process));
        // Older peers do not report this field. In that case, skip local
        // classification rather than attributing a violation to a guessed backend.
        let sandbox_type = started.sandbox_type.unwrap_or(SandboxType::None);
        let mut managed = Self::new(
            process_handle,
            sandbox_type,
            /*spawn_lifecycle*/ None,
            capture_monitor_output,
        );
        let output_handles = managed.output_handles().clone();
        managed.output_task = Some(Self::spawn_exec_server_output_task(
            started,
            output_handles,
            managed.output_tx.clone(),
            managed.state_tx.clone(),
            Arc::clone(&managed.output_task_finished),
            Arc::clone(&managed.output_task_finished_notify),
            Arc::clone(&managed.monitor_stop_pending),
            Arc::clone(&managed.monitor_stop_pending_notify),
        ));

        let mut state_rx = managed.state_rx.clone();
        if tokio::time::timeout(EARLY_EXIT_GRACE_PERIOD, async {
            loop {
                let state = state_rx.borrow().clone();
                if state.has_exited || state.failure_message.is_some() {
                    break;
                }
                if state_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
        {
            managed.check_for_sandbox_denial().await?;
        }

        Ok(managed)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_exec_server_output_task(
        started: StartedExecProcess,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<Vec<u8>>,
        state_tx: watch::Sender<ProcessState>,
        output_task_finished: Arc<AtomicBool>,
        output_task_finished_notify: Arc<Notify>,
        monitor_stop_pending: Arc<AtomicUsize>,
        monitor_stop_pending_notify: Arc<Notify>,
    ) -> JoinHandle<()> {
        let OutputHandles {
            output_buffer,
            output_notify,
            monitor_capture,
            monitor_stream_output,
            output_closed,
            output_closed_notify,
            cancellation_token,
            ..
        } = output_handles;
        let process = started.process;
        let mut events = process.subscribe_events();
        let output_task_guard = OutputTaskGuard {
            output_closed: Arc::clone(&output_closed),
            output_closed_notify: Arc::clone(&output_closed_notify),
            output_task_finished,
            output_task_finished_notify,
            monitor_capture: Arc::clone(&monitor_capture),
            completed: false,
        };
        tokio::spawn(async move {
            let mut output_task_guard = output_task_guard;
            let mut last_seq: u64 = 0;
            loop {
                let event = match events.recv().await {
                    Ok(event) => Some(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => None,
                    Err(broadcast::error::RecvError::Closed) => {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(
                            state.failed("exec-server process event stream closed".to_string()),
                        );
                        cancellation_token.cancel();
                        break;
                    }
                };
                let event_seq = event.as_ref().and_then(|event| match event {
                    ExecProcessEvent::Output(chunk) => Some(chunk.seq),
                    ExecProcessEvent::Exited { seq, .. } | ExecProcessEvent::Closed { seq } => {
                        Some(*seq)
                    }
                    ExecProcessEvent::Failed(_) => None,
                });
                let missing_sandbox_denial = matches!(
                    event.as_ref(),
                    Some(ExecProcessEvent::Exited {
                        sandbox_denied: None,
                        ..
                    })
                );
                if event.is_none()
                    || event_seq.is_some_and(|seq| seq > last_seq.saturating_add(1))
                    || missing_sandbox_denial
                {
                    let response = match process
                        .read(
                            Some(last_seq),
                            /*max_bytes*/ None,
                            /*wait_ms*/ Some(0),
                        )
                        .await
                    {
                        Ok(response) => response,
                        Err(err) => {
                            let state = state_tx.borrow().clone();
                            let _ = state_tx.send_replace(state.failed(err.to_string()));
                            cancellation_token.cancel();
                            break;
                        }
                    };
                    let ExecReadResponse {
                        chunks,
                        next_seq,
                        exited,
                        exit_code,
                        closed,
                        failure,
                        sandbox_denied,
                    } = response;
                    for chunk in chunks.into_iter().filter(|chunk| chunk.seq > last_seq) {
                        let is_stdout = chunk.stream == ExecOutputStream::Stdout;
                        let bytes = chunk.chunk.into_inner();
                        output_buffer.lock().await.push_chunk(bytes.clone());
                        if let Some(stream_output) = &monitor_stream_output {
                            stream_output.push_chunk(bytes.clone(), is_stdout).await;
                        }
                        send_monitor_capture(
                            &monitor_capture,
                            &monitor_stop_pending,
                            &monitor_stop_pending_notify,
                            MonitorOutputChunk::Output {
                                bytes: bytes.clone(),
                                is_stdout,
                            },
                        )
                        .await;
                        let _ = output_tx.send(bytes);
                        output_notify.notify_waiters();
                    }
                    last_seq = last_seq.max(next_seq.saturating_sub(1));
                    if let Some(message) = failure {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.failed(message));
                        cancellation_token.cancel();
                        break;
                    }
                    if sandbox_denied || exited {
                        let mut state = state_tx.borrow().clone();
                        state.sandbox_denied |= sandbox_denied;
                        let _ = state_tx.send_replace(if exited {
                            state.exited(exit_code)
                        } else {
                            state
                        });
                    }
                    if closed {
                        output_task_guard.mark_completed();
                        cancellation_token.cancel();
                        break;
                    }
                    continue;
                }

                let Some(event) = event else {
                    continue;
                };
                match event {
                    ExecProcessEvent::Output(chunk) => {
                        if chunk.seq <= last_seq {
                            continue;
                        }
                        last_seq = chunk.seq;
                        let is_stdout = chunk.stream == ExecOutputStream::Stdout;
                        let bytes = chunk.chunk.into_inner();
                        output_buffer.lock().await.push_chunk(bytes.clone());
                        if let Some(stream_output) = &monitor_stream_output {
                            stream_output.push_chunk(bytes.clone(), is_stdout).await;
                        }
                        send_monitor_capture(
                            &monitor_capture,
                            &monitor_stop_pending,
                            &monitor_stop_pending_notify,
                            MonitorOutputChunk::Output {
                                bytes: bytes.clone(),
                                is_stdout,
                            },
                        )
                        .await;
                        let _ = output_tx.send(bytes);
                        output_notify.notify_waiters();
                    }
                    ExecProcessEvent::Exited {
                        seq,
                        exit_code,
                        sandbox_denied,
                    } => {
                        if seq <= last_seq {
                            continue;
                        }
                        last_seq = seq;
                        let mut state = state_tx.borrow().clone();
                        state.sandbox_denied |= sandbox_denied.unwrap_or(false);
                        let _ = state_tx.send_replace(state.exited(Some(exit_code)));
                    }
                    ExecProcessEvent::Closed { seq } => {
                        if seq <= last_seq {
                            continue;
                        }
                        output_task_guard.mark_completed();
                        cancellation_token.cancel();
                        break;
                    }
                    ExecProcessEvent::Failed(message) => {
                        let state = state_tx.borrow().clone();
                        let _ = state_tx.send_replace(state.failed(message));
                        cancellation_token.cancel();
                        break;
                    }
                }
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_local_output_task(
        mut stdout_rx: mpsc::Receiver<Vec<u8>>,
        mut stderr_rx: mpsc::Receiver<Vec<u8>>,
        output_handles: OutputHandles,
        output_tx: broadcast::Sender<Vec<u8>>,
        output_task_finished: Arc<AtomicBool>,
        output_task_finished_notify: Arc<Notify>,
        monitor_stop_pending: Arc<AtomicUsize>,
        monitor_stop_pending_notify: Arc<Notify>,
    ) -> JoinHandle<()> {
        let OutputHandles {
            output_buffer,
            output_notify,
            monitor_capture,
            monitor_stream_output,
            output_closed,
            output_closed_notify,
            ..
        } = output_handles;
        let output_task_guard = OutputTaskGuard {
            output_closed: Arc::clone(&output_closed),
            output_closed_notify: Arc::clone(&output_closed_notify),
            output_task_finished,
            output_task_finished_notify,
            monitor_capture: Arc::clone(&monitor_capture),
            completed: false,
        };
        tokio::spawn(async move {
            let mut output_task_guard = output_task_guard;
            let mut stdout_open = true;
            let mut stderr_open = true;
            while stdout_open || stderr_open {
                let (chunk, is_stdout) = tokio::select! {
                    chunk = stdout_rx.recv(), if stdout_open => match chunk {
                        Some(chunk) => (chunk, true),
                        None => {
                            stdout_open = false;
                            send_monitor_capture(
                                &monitor_capture,
                                &monitor_stop_pending,
                                &monitor_stop_pending_notify,
                                MonitorOutputChunk::StdoutClosed,
                            )
                            .await;
                            continue;
                        }
                    },
                    chunk = stderr_rx.recv(), if stderr_open => match chunk {
                        Some(chunk) => (chunk, false),
                        None => {
                            stderr_open = false;
                            continue;
                        }
                    },
                };
                {
                    let mut guard = output_buffer.lock().await;
                    guard.push_chunk(chunk.clone());
                }
                if let Some(stream_output) = &monitor_stream_output {
                    stream_output.push_chunk(chunk.clone(), is_stdout).await;
                }
                send_monitor_capture(
                    &monitor_capture,
                    &monitor_stop_pending,
                    &monitor_stop_pending_notify,
                    MonitorOutputChunk::Output {
                        bytes: chunk.clone(),
                        is_stdout,
                    },
                )
                .await;
                let _ = output_tx.send(chunk);
                output_notify.notify_waiters();
            }
            output_task_guard.mark_completed();
        })
    }

    fn signal_exit(&self, exit_code: Option<i32>) {
        let state = self.state_rx.borrow().clone();
        let _ = self.state_tx.send_replace(state.exited(exit_code));
        self.output.cancellation_token.cancel();
    }
}

impl Drop for UnifiedExecProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}
