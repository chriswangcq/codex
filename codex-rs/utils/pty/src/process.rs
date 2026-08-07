use core::fmt;
use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use anyhow::anyhow;
use portable_pty::MasterPty;
use portable_pty::PtySize;
use portable_pty::SlavePty;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessSignal {
    Interrupt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessTerminationMode {
    Regular,
    Force,
}

pub(crate) fn unsupported_signal(signal: ProcessSignal) -> io::Error {
    match signal {
        ProcessSignal::Interrupt => io::Error::new(
            io::ErrorKind::Unsupported,
            "process interrupt is not supported by this process backend",
        ),
    }
}

pub(crate) fn exit_code_from_status(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }

    -1
}

pub(crate) trait ChildTerminator: Send + Sync {
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()>;

    fn kill(&mut self) -> io::Result<()>;

    /// Force termination even when the platform normally preserves descendants
    /// after the root process has exited.
    fn force_kill(&mut self, _root_exited: bool) -> io::Result<()> {
        self.kill()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(value: TerminalSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[cfg(unix)]
pub(crate) trait PtyHandleKeepAlive: Send {}

#[cfg(unix)]
impl<T: Send + ?Sized> PtyHandleKeepAlive for T {}

pub(crate) enum PtyMasterHandle {
    Resizable(Box<dyn MasterPty + Send>),
    #[cfg(unix)]
    Opaque {
        raw_fd: RawFd,
        _handle: Box<dyn PtyHandleKeepAlive>,
    },
}

pub struct PtyHandles {
    pub _slave: Option<Box<dyn SlavePty + Send>>,
    pub(crate) _master: PtyMasterHandle,
}

impl fmt::Debug for PtyHandles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyHandles").finish()
    }
}

/// Callback used by driver-backed sessions to resize a PTY-like backend when
/// there is no local `PtyHandles` instance to resize directly.
type ResizeFn = Box<dyn FnMut(TerminalSize) -> anyhow::Result<()> + Send>;

/// Handle for driving an interactive process (PTY or pipe).
pub struct ProcessHandle {
    writer_tx: StdMutex<Option<mpsc::Sender<Vec<u8>>>>,
    killer: StdMutex<Option<Box<dyn ChildTerminator>>>,
    reader_handle: StdMutex<Option<JoinHandle<()>>>,
    reader_abort_handles: StdMutex<Vec<AbortHandle>>,
    writer_handle: StdMutex<Option<JoinHandle<()>>>,
    wait_handle: StdMutex<Option<JoinHandle<()>>>,
    exit_status: Arc<AtomicBool>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    exit_notify: Arc<Notify>,
    output_closed: Arc<AtomicBool>,
    output_closed_notify: Arc<Notify>,
    // PtyHandles must be preserved because the process will receive Control+C if the
    // slave is closed
    _pty_handles: StdMutex<Option<PtyHandles>>,
    // Optional resize hook for driver-backed sessions that proxy PTY control to
    // another backend instead of owning local PTY handles.
    resizer: StdMutex<Option<ResizeFn>>,
}

struct OutputClosedGuard {
    output_closed: Arc<AtomicBool>,
    output_closed_notify: Arc<Notify>,
    completed: bool,
}

impl OutputClosedGuard {
    fn mark_completed(&mut self) {
        self.completed = true;
    }
}

impl Drop for OutputClosedGuard {
    fn drop(&mut self) {
        if self.completed {
            self.output_closed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        self.output_closed_notify.notify_waiters();
    }
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessHandle").finish()
    }
}

impl ProcessHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        writer_tx: mpsc::Sender<Vec<u8>>,
        killer: Box<dyn ChildTerminator>,
        reader_handle: JoinHandle<()>,
        reader_abort_handles: Vec<AbortHandle>,
        writer_handle: JoinHandle<()>,
        wait_handle: JoinHandle<()>,
        exit_status: Arc<AtomicBool>,
        exit_code: Arc<StdMutex<Option<i32>>>,
        exit_notify: Arc<Notify>,
        pty_handles: Option<PtyHandles>,
        resizer: Option<ResizeFn>,
    ) -> Self {
        let output_closed = Arc::new(AtomicBool::new(false));
        let output_closed_notify = Arc::new(Notify::new());
        let reader_handle = tokio::spawn({
            let output_closed = Arc::clone(&output_closed);
            let output_closed_notify = Arc::clone(&output_closed_notify);
            async move {
                let mut guard = OutputClosedGuard {
                    output_closed,
                    output_closed_notify,
                    completed: false,
                };
                if reader_handle.await.is_ok() {
                    guard.mark_completed();
                }
            }
        });
        Self {
            writer_tx: StdMutex::new(Some(writer_tx)),
            killer: StdMutex::new(Some(killer)),
            reader_handle: StdMutex::new(Some(reader_handle)),
            reader_abort_handles: StdMutex::new(reader_abort_handles),
            writer_handle: StdMutex::new(Some(writer_handle)),
            wait_handle: StdMutex::new(Some(wait_handle)),
            exit_status,
            exit_code,
            exit_notify,
            output_closed,
            output_closed_notify,
            _pty_handles: StdMutex::new(pty_handles),
            resizer: StdMutex::new(resizer),
        }
    }

    /// Returns a channel sender for writing raw bytes to the child stdin.
    pub fn writer_sender(&self) -> mpsc::Sender<Vec<u8>> {
        if let Ok(writer_tx) = self.writer_tx.lock()
            && let Some(writer_tx) = writer_tx.as_ref()
        {
            return writer_tx.clone();
        }

        let (writer_tx, writer_rx) = mpsc::channel(1);
        drop(writer_rx);
        writer_tx
    }

    /// True if the child process has exited.
    pub fn has_exited(&self) -> bool {
        self.exit_status.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Returns the exit code if known.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code.lock().ok().and_then(|guard| *guard)
    }

    /// Wait until the child process waiter has observed its exit.
    pub async fn wait_for_exit(&self) {
        loop {
            let notified = self.exit_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.has_exited() {
                return;
            }
            notified.await;
        }
    }

    /// True once all stdout/stderr producers have stopped and their reader
    /// tasks have released the output channel senders.
    pub fn output_closed(&self) -> bool {
        self.output_closed
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Wait until every stdout/stderr reader has stopped producing output.
    pub async fn wait_for_output_closed(&self) {
        loop {
            let notified = self.output_closed_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.output_closed() {
                return;
            }
            notified.await;
        }
    }

    /// Retire the cached process-tree terminator after both the root waiter and
    /// all output readers have confirmed shutdown.
    ///
    /// Until this succeeds, the terminator is retained so a failed or timed-out
    /// checked termination can be retried. Retiring it also prevents a later
    /// drop from signaling a process group whose numeric ID may have been reused.
    pub fn confirm_termination(&self) -> io::Result<()> {
        if !self.has_exited() || !self.output_closed() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "process root or output readers are still running",
            ));
        }
        let _ = self
            .killer
            .lock()
            .map_err(|_| io::Error::other("failed to lock process terminator"))?
            .take();
        Ok(())
    }

    /// Resize the PTY in character cells.
    pub fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        {
            let handles = self
                ._pty_handles
                .lock()
                .map_err(|_| anyhow!("failed to lock PTY handles"))?;
            if let Some(handles) = handles.as_ref() {
                return match &handles._master {
                    PtyMasterHandle::Resizable(master) => master.resize(size.into()),
                    #[cfg(unix)]
                    PtyMasterHandle::Opaque { raw_fd, .. } => resize_raw_pty(*raw_fd, size),
                };
            }
        }

        let mut resizer = self
            .resizer
            .lock()
            .map_err(|_| anyhow!("failed to lock PTY resizer"))?;
        if let Some(resizer) = resizer.as_mut() {
            resizer(size)
        } else {
            Err(anyhow!("process is not attached to a PTY"))
        }
    }

    /// Close the child's stdin channel.
    pub fn close_stdin(&self) {
        if let Ok(mut writer_tx) = self.writer_tx.lock() {
            writer_tx.take();
        }
    }

    #[cfg(windows)]
    pub async fn release_pty_handles_after_root_exit(&self) {
        let has_pty_handles = self
            ._pty_handles
            .lock()
            .map(|handles| handles.is_some())
            .unwrap_or(false);
        if !has_pty_handles {
            return;
        }

        self.close_stdin();
        let writer_handle = self
            .writer_handle
            .lock()
            .ok()
            .and_then(|mut handle| handle.take());
        if let Some(writer_handle) = writer_handle {
            writer_handle.abort();
            let _ = writer_handle.await;
        }
        if let Ok(mut handles) = self._pty_handles.lock() {
            handles.take();
        }
    }

    /// Attempts to kill the child while leaving the reader/writer tasks alive
    /// so callers can still drain output until EOF.
    pub fn request_terminate(&self) {
        if let Ok(mut killer_opt) = self.killer.lock() {
            if self.has_exited() && self.output_closed() {
                // A normally completed process has no live output producer.
                // Retire cached numeric identifiers before they can be reused.
                killer_opt.take();
                return;
            }
            #[cfg(windows)]
            if self.has_exited() {
                // Windows waiters disable kill-on-close before publishing root
                // exit so an ordinary drop preserves background descendants.
                // Keep the terminator available in case a later checked stop
                // explicitly needs to force-kill that preserved process tree.
                return;
            }
            if let Some(mut killer) = killer_opt.take() {
                let _ = killer.kill();
            }
        }
    }

    /// Attempts to kill the child without stopping its waiter, propagating any
    /// error and retaining the terminator so the request can be retried.
    pub fn try_request_terminate(&self) -> io::Result<()> {
        // A shell can exit while a background descendant still owns stdout or
        // stderr. In that state the cached process-group/job terminator is still
        // needed even though the root waiter has completed. Only skip the kill
        // once both the root and every output producer are known to be gone.
        if self.has_exited() && self.output_closed() {
            return Ok(());
        }

        let mut killer_opt = self
            .killer
            .lock()
            .map_err(|_| io::Error::other("failed to lock process terminator"))?;
        let Some(killer) = killer_opt.as_mut() else {
            if self.has_exited() && self.output_closed() {
                return Ok(());
            }
            return Err(io::Error::other("process terminator is unavailable"));
        };
        killer.force_kill(self.has_exited())
    }

    pub fn signal(&self, signal: ProcessSignal) -> io::Result<()> {
        let Ok(mut killer_opt) = self.killer.lock() else {
            return Ok(());
        };
        let Some(killer) = killer_opt.as_mut() else {
            return Ok(());
        };

        let result = killer.signal(signal);
        #[cfg(windows)]
        if result.is_ok() {
            killer_opt.take();
        }
        result
    }

    /// Attempts to kill the child and abort helper tasks.
    pub fn terminate(&self) {
        self.request_terminate();

        if let Ok(mut h) = self.reader_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut handles) = self.reader_abort_handles.lock() {
            for handle in handles.drain(..) {
                handle.abort();
            }
        }
        if let Ok(mut h) = self.writer_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut h) = self.wait_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Adapts a closure into a `ChildTerminator` implementation.
struct ClosureTerminator {
    inner: Option<Box<dyn FnMut(ProcessTerminationMode) -> io::Result<()> + Send + Sync>>,
    #[cfg(windows)]
    interrupt_terminates: bool,
}

impl ChildTerminator for ClosureTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()> {
        #[cfg(windows)]
        if self.interrupt_terminates {
            return self.kill();
        }

        Err(unsupported_signal(signal))
    }

    fn kill(&mut self) -> io::Result<()> {
        if let Some(inner) = self.inner.as_mut() {
            return (inner)(ProcessTerminationMode::Regular);
        }
        Ok(())
    }

    fn force_kill(&mut self, _root_exited: bool) -> io::Result<()> {
        if let Some(inner) = self.inner.as_mut() {
            return (inner)(ProcessTerminationMode::Force);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn resize_raw_pty(raw_fd: RawFd, size: TerminalSize) -> anyhow::Result<()> {
    let mut winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { libc::ioctl(raw_fd, libc::TIOCSWINSZ, &mut winsize) };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Combine split stdout/stderr receivers into a single broadcast receiver.
pub fn combine_output_receivers(
    mut stdout_rx: mpsc::Receiver<Vec<u8>>,
    mut stderr_rx: mpsc::Receiver<Vec<u8>>,
) -> broadcast::Receiver<Vec<u8>> {
    let (combined_tx, combined_rx) = broadcast::channel(256);
    tokio::spawn(async move {
        let mut stdout_open = true;
        let mut stderr_open = true;

        loop {
            tokio::select! {
                stdout = stdout_rx.recv(), if stdout_open => match stdout {
                    Some(chunk) => {
                        let _ = combined_tx.send(chunk);
                    }
                    None => {
                        stdout_open = false;
                    }
                },
                stderr = stderr_rx.recv(), if stderr_open => match stderr {
                    Some(chunk) => {
                        let _ = combined_tx.send(chunk);
                    }
                    None => {
                        stderr_open = false;
                    }
                },
                else => break,
            }
        }
    });
    combined_rx
}

/// Return value from PTY or pipe spawn helpers.
#[derive(Debug)]
pub struct SpawnedProcess {
    pub session: ProcessHandle,
    pub stdout_rx: mpsc::Receiver<Vec<u8>>,
    pub stderr_rx: mpsc::Receiver<Vec<u8>>,
    pub exit_rx: oneshot::Receiver<i32>,
}

/// Driver-backed process handles for non-standard spawn backends.
pub struct ProcessDriver {
    pub writer_tx: mpsc::Sender<Vec<u8>>,
    pub stdout_rx: broadcast::Receiver<Vec<u8>>,
    pub stderr_rx: Option<broadcast::Receiver<Vec<u8>>>,
    pub exit_rx: oneshot::Receiver<i32>,
    pub terminator: Option<Box<dyn FnMut(ProcessTerminationMode) -> io::Result<()> + Send + Sync>>,
    pub writer_handle: Option<JoinHandle<()>>,
    pub resizer: Option<ResizeFn>,
    /// Whether this Windows process is attached to a pseudo-console.
    #[cfg(windows)]
    pub tty: bool,
}

/// Build a `SpawnedProcess` from a driver that supplies stdin/output/exit channels.
pub fn spawn_from_driver(driver: ProcessDriver) -> SpawnedProcess {
    let ProcessDriver {
        writer_tx,
        stdout_rx: stdout_driver_rx,
        stderr_rx: mut stderr_driver_rx,
        exit_rx,
        terminator,
        writer_handle,
        resizer,
        #[cfg(windows)]
        tty,
    } = driver;

    #[cfg(windows)]
    let interrupt_terminates = terminator.is_some() && !tty;

    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(256);
    let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(256);
    let (exit_seen_tx, exit_seen_rx) = watch::channel(false);
    let spawn_stream_reader =
        |mut output_rx: broadcast::Receiver<Vec<u8>>,
         output_tx: mpsc::Sender<Vec<u8>>,
         mut exit_seen_rx: watch::Receiver<bool>| {
            tokio::spawn(async move {
                loop {
                    let recv_result = if *exit_seen_rx.borrow() {
                        // Once exit has been observed, we no longer want a timer here. Some
                        // backends publish the exit code before their final stdout/stderr bytes
                        // have been forwarded through the broadcast channel, so a fixed grace
                        // period can still drop the tail of the stream under load.
                        //
                        // Instead, keep waiting until the driver closes the broadcast sender.
                        // That makes the shutdown contract explicit: the backend is responsible
                        // for dropping its sender when it has truly finished forwarding output.
                        output_rx.recv().await
                    } else {
                        tokio::select! {
                            _ = exit_seen_rx.changed() => {
                                continue;
                            }
                            result = output_rx.recv() => result,
                        }
                    };
                    match recv_result {
                        Ok(chunk) => {
                            if output_tx.send(chunk).await.is_err() {
                                return false;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return true,
                    }
                }
            })
        };
    let stdout_reader_handle =
        spawn_stream_reader(stdout_driver_rx, stdout_tx, exit_seen_rx.clone());
    let stderr_reader_handle = stderr_driver_rx
        .take()
        .map(|rx| spawn_stream_reader(rx, stderr_tx, exit_seen_rx));
    let mut reader_abort_handles = vec![stdout_reader_handle.abort_handle()];
    if let Some(handle) = stderr_reader_handle.as_ref() {
        reader_abort_handles.push(handle.abort_handle());
    }
    let reader_handle = tokio::spawn(async move {
        let mut completed = matches!(stdout_reader_handle.await, Ok(true));
        if let Some(handle) = stderr_reader_handle {
            completed &= matches!(handle.await, Ok(true));
        }
        assert!(
            completed,
            "driver output reader task did not complete normally"
        );
    });

    let writer_handle = writer_handle.unwrap_or_else(|| tokio::spawn(async {}));

    let (exit_tx, exit_rx_out) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let exit_notify = Arc::new(Notify::new());
    let wait_exit_notify = Arc::clone(&exit_notify);
    let wait_handle = tokio::spawn(async move {
        let code = exit_rx.await.unwrap_or(-1);
        if let Ok(mut guard) = wait_exit_code.lock() {
            *guard = Some(code);
        }
        wait_exit_status.store(true, std::sync::atomic::Ordering::SeqCst);
        wait_exit_notify.notify_waiters();
        let _ = exit_seen_tx.send(true);
        let _ = exit_tx.send(code);
    });

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(ClosureTerminator {
            inner: terminator,
            #[cfg(windows)]
            interrupt_terminates,
        }),
        reader_handle,
        reader_abort_handles,
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        exit_notify,
        /*pty_handles*/ None,
        resizer,
    );

    SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx: exit_rx_out,
    }
}
