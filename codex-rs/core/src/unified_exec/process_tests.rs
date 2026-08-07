use super::head_tail_buffer::HeadTailBuffer;
use super::process::MonitorOutputChunk;
use super::process::MonitorStreamOutput;
use super::process::OutputHandles;
use super::process::UnifiedExecProcess;
use super::process::monitor_capture_channel;
use crate::unified_exec::UnifiedExecError;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    terminate_error: Option<String>,
    terminate_never: bool,
    wake_tx: watch::Sender<u64>,
}

impl MockExecProcess {
    async fn read(&self) -> Result<ReadResponse, ExecServerError> {
        Ok(self
            .read_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            }))
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        if self.terminate_never {
            std::future::pending::<()>().await;
        }
        if let Some(message) = &self.terminate_error {
            return Err(ExecServerError::Protocol(message.clone()));
        }
        Ok(())
    }
}

impl ExecProcess for MockExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(MockExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { Ok(self.write_response.clone()) })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(MockExecProcess::terminate(self))
    }
}

pub(super) async fn remote_process(
    write_status: WriteStatus,
    terminate_error: Option<String>,
    sandbox_type: codex_sandboxing::SandboxType,
) -> UnifiedExecProcess {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error,
            terminate_never: false,
            wake_tx,
        }),
        sandbox_type: Some(sandbox_type),
    };

    UnifiedExecProcess::from_exec_server_started(started, /*capture_monitor_output*/ false)
        .await
        .expect("remote process should start")
}

pub(crate) async fn blocking_terminate_remote_process(
    capture_monitor_output: bool,
) -> UnifiedExecProcess {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "blocking-terminate".to_string().into(),
            write_response: WriteResponse {
                status: WriteStatus::Accepted,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error: None,
            terminate_never: true,
            wake_tx,
        }),
        sandbox_type: Some(codex_sandboxing::SandboxType::None),
    };

    UnifiedExecProcess::from_exec_server_started(started, capture_monitor_output)
        .await
        .expect("remote process should start")
}

#[tokio::test]
async fn remote_write_unknown_process_marks_process_exited() {
    let process = remote_process(
        WriteStatus::UnknownProcess,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_write_closed_stdin_marks_process_exited() {
    let process = remote_process(
        WriteStatus::StdinClosed,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn fail_and_terminate_preserves_failure_message() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    process.fail_and_terminate("network denied".to_string());
    process.fail_and_terminate("second failure".to_string());

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
    let error = process
        .terminate_confirmed()
        .await
        .expect_err("a synthetic failure cannot confirm remote process termination");
    assert!(error.to_string().contains("network denied"));
}

#[tokio::test(start_paused = true)]
async fn remote_terminate_confirmed_requires_lifecycle_confirmation_after_ack() {
    let process = remote_process(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("expected terminate failure");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());

    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::None,
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("a terminate ACK without Exited and Closed is not confirmation");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());
}

#[tokio::test(start_paused = true)]
async fn remote_terminate_confirmed_times_out_and_preserves_running_state() {
    let process = blocking_terminate_remote_process(/*capture_monitor_output*/ false).await;

    let err = tokio::time::timeout(
        super::process::TERMINATE_CONFIRMATION_TIMEOUT + tokio::time::Duration::from_secs(1),
        process.terminate_confirmed(),
    )
    .await
    .expect("terminate_confirmed should enforce its own timeout")
    .expect_err("blocking termination should fail");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());
}

#[tokio::test]
async fn local_terminate_confirmed_waits_for_observed_driver_exit() {
    let (writer_tx, _writer_rx) = mpsc::channel(1);
    let (stdout_tx, stdout_rx) = broadcast::channel(1);
    drop(stdout_tx);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let exit_tx = Arc::new(std::sync::Mutex::new(Some(exit_tx)));
    let terminator_exit_tx = Arc::clone(&exit_tx);
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: Some(Box::new(move |_mode| {
            if let Some(exit_tx) = terminator_exit_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = exit_tx.send(137);
            }
            Ok(())
        })),
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        codex_sandboxing::SandboxType::None,
        Box::new(crate::unified_exec::NoopSpawnLifecycle),
        /*capture_monitor_output*/ false,
    )
    .await
    .expect("local process should start");

    process
        .terminate_confirmed()
        .await
        .expect("observed local exit should confirm termination");

    assert!(process.has_exited());
    assert_eq!(process.exit_code(), Some(137));
    assert!(process.output_task_finished());
    assert!(process.output_completed_normally());
}

#[tokio::test(start_paused = true)]
async fn local_terminate_confirmed_retries_when_root_exited_but_output_stayed_open() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let terminator_attempts = Arc::clone(&attempts);
    let (stdout_tx, stdout_rx) = broadcast::channel(1);
    let stdout_tx = Arc::new(std::sync::Mutex::new(Some(stdout_tx)));
    let terminator_stdout_tx = Arc::clone(&stdout_tx);
    let (writer_tx, _writer_rx) = mpsc::channel(1);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    exit_tx
        .send(0)
        .expect("driver exit receiver should be open");
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: Some(Box::new(move |_mode| {
            if terminator_attempts.fetch_add(1, Ordering::SeqCst) == 1 {
                terminator_stdout_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
            }
            Ok(())
        })),
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        codex_sandboxing::SandboxType::None,
        Box::new(crate::unified_exec::NoopSpawnLifecycle),
        /*capture_monitor_output*/ true,
    )
    .await
    .expect("local process should start");
    let mut capture_rx = process
        .output_handles()
        .take_monitor_capture()
        .expect("monitor capture should be available");

    assert!(
        process.has_exited(),
        "the root exit should already be observed"
    );
    let err = process
        .terminate_confirmed()
        .await
        .expect_err("open descendant output should prevent confirmation");
    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(
        !process
            .output_handles()
            .output_closed
            .load(Ordering::Acquire)
    );
    assert_eq!(process.exit_code(), Some(0));

    process
        .terminate_confirmed()
        .await
        .expect("retry should close descendant output and confirm termination");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(
        process
            .output_handles()
            .output_closed
            .load(Ordering::Acquire)
    );
    while capture_rx.recv().await.is_some() {}
}

#[tokio::test(start_paused = true)]
async fn local_terminate_confirmed_times_out_without_faking_exit_and_can_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let terminator_attempts = Arc::clone(&attempts);
    let (writer_tx, _writer_rx) = mpsc::channel(1);
    let (_stdout_tx, stdout_rx) = broadcast::channel(1);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let spawned = codex_utils_pty::spawn_from_driver(codex_utils_pty::ProcessDriver {
        writer_tx,
        stdout_rx,
        stderr_rx: None,
        exit_rx,
        terminator: Some(Box::new(move |_mode| {
            terminator_attempts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })),
        writer_handle: None,
        resizer: None,
        #[cfg(windows)]
        tty: false,
    });
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        codex_sandboxing::SandboxType::None,
        Box::new(crate::unified_exec::NoopSpawnLifecycle),
        /*capture_monitor_output*/ false,
    )
    .await
    .expect("local process should start");

    for expected_attempts in 1..=2 {
        let err = process
            .terminate_confirmed()
            .await
            .expect_err("unobserved local exit should not confirm termination");
        assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
        assert!(!process.has_exited());
        assert!(!process.cancellation_token().is_cancelled());
        assert_eq!(attempts.load(Ordering::SeqCst), expected_attempts);
    }

    drop(exit_tx);
}

#[tokio::test]
async fn remote_process_preserves_executor_sandbox_type() {
    let process = remote_process(
        WriteStatus::Accepted,
        /*terminate_error*/ None,
        codex_sandboxing::SandboxType::LinuxSeccomp,
    )
    .await;

    assert_eq!(
        process.sandbox_type(),
        codex_sandboxing::SandboxType::LinuxSeccomp
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_terminate_confirmed_kills_descendant_after_shell_exit_and_closes_output()
-> anyhow::Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let pid_file = temp_dir.path().join("background.pid");
    let mut env: std::collections::HashMap<String, String> = std::env::vars().collect();
    env.insert(
        "CODEX_MONITOR_TEST_PID_FILE".to_string(),
        pid_file.to_string_lossy().into_owned(),
    );
    let spawned = codex_utils_pty::spawn_pipe_process(
        "/bin/sh",
        &[
            "-c".to_string(),
            "sleep 300 & bg=$!; printf '%s' \"$bg\" >\"$CODEX_MONITOR_TEST_PID_FILE\"".to_string(),
        ],
        std::path::Path::new("."),
        &env,
        &None,
        &[],
    )
    .await?;
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        codex_sandboxing::SandboxType::None,
        Box::new(crate::unified_exec::NoopSpawnLifecycle),
        /*capture_monitor_output*/ true,
    )
    .await?;
    let mut capture_rx = process
        .output_handles()
        .take_monitor_capture()
        .expect("monitor capture should be available");
    let background_pid = tokio::time::timeout(tokio::time::Duration::from_secs(2), async {
        loop {
            if let Ok(contents) = tokio::fs::read_to_string(&pid_file).await
                && let Ok(pid) = contents.parse::<i32>()
            {
                break pid;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for background pid"))?;

    tokio::time::timeout(tokio::time::Duration::from_secs(2), async {
        while !process.has_exited() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for shell root exit"))?;
    assert!(unix_process_exists(background_pid)?);
    assert!(
        !process
            .output_handles()
            .output_closed
            .load(Ordering::Acquire),
        "the descendant should still hold the output pipes open"
    );

    process.terminate_confirmed().await?;

    assert!(
        process
            .output_handles()
            .output_closed
            .load(Ordering::Acquire)
    );
    tokio::time::timeout(tokio::time::Duration::from_secs(1), async {
        while capture_rx.recv().await.is_some() {}
    })
    .await
    .map_err(|_| anyhow::anyhow!("monitor capture did not close after termination"))?;
    let descendant_exited = tokio::time::timeout(tokio::time::Duration::from_secs(3), async {
        loop {
            if !unix_process_exists(background_pid).unwrap_or(false) {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    if !descendant_exited {
        unsafe {
            libc::kill(background_pid, libc::SIGKILL);
        }
    }
    assert!(
        descendant_exited,
        "background descendant {background_pid} survived confirmed termination"
    );

    // A fully exited process remains safe to terminate repeatedly.
    process.terminate_confirmed().await?;
    Ok(())
}

#[cfg(unix)]
fn unix_process_exists(pid: i32) -> std::io::Result<bool> {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else if err.raw_os_error() == Some(libc::EPERM) {
        Ok(true)
    } else {
        Err(err)
    }
}

#[tokio::test]
async fn monitor_capture_is_lossless_and_marks_stdout_beyond_head_tail_capacity() {
    let (capture_tx, mut capture_rx) = monitor_capture_channel(128);
    let output_buffer = Arc::new(Mutex::new(HeadTailBuffer::default()));
    let stream_output = MonitorStreamOutput::new(Arc::clone(&output_buffer));
    let output = OutputHandles {
        output_buffer,
        output_notify: Arc::new(Notify::new()),
        monitor_capture: Arc::new(std::sync::Mutex::new(Some(capture_tx))),
        monitor_capture_rx: Arc::new(std::sync::Mutex::new(None)),
        monitor_stream_output: Some(stream_output.clone()),
        output_closed: Arc::new(AtomicBool::new(false)),
        output_closed_notify: Arc::new(Notify::new()),
        cancellation_token: CancellationToken::new(),
    };
    let (stdout_tx, stdout_rx) = mpsc::channel(2);
    let (stderr_tx, stderr_rx) = mpsc::channel(2);
    let (output_tx, _) = broadcast::channel(2);
    let output_task_finished = Arc::new(AtomicBool::new(false));
    let task = UnifiedExecProcess::spawn_local_output_task(
        stdout_rx,
        stderr_rx,
        output,
        output_tx,
        Arc::clone(&output_task_finished),
        Arc::new(Notify::new()),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(Notify::new()),
    );
    let stdout = vec![b'o'; crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES + 4096];
    let stderr = b"stderr-only".to_vec();

    stdout_tx
        .send(stdout.clone())
        .await
        .expect("stdout producer should remain open");
    stderr_tx
        .send(stderr.clone())
        .await
        .expect("stderr producer should remain open");
    drop(stdout_tx);
    drop(stderr_tx);
    task.await.expect("output task should finish");
    assert!(output_task_finished.load(Ordering::Acquire));

    let mut captured_stdout = Vec::new();
    let mut captured_stderr = Vec::new();
    while let Some(chunk) = capture_rx.recv().await {
        match chunk {
            MonitorOutputChunk::Output { bytes, is_stdout } if is_stdout => {
                captured_stdout.extend(bytes);
            }
            MonitorOutputChunk::Output { bytes, .. } => captured_stderr.extend(bytes),
            MonitorOutputChunk::StdoutClosed => {}
            MonitorOutputChunk::ArchiveGap => {
                panic!("lossless capture must not report an archive gap")
            }
        }
    }
    assert_eq!(captured_stdout, stdout);
    assert_eq!(captured_stderr, stderr);
    let (final_stdout, final_stderr, final_combined) = stream_output.snapshot().await;
    assert!(final_stdout.starts_with('o'));
    assert!(final_stdout.contains("bytes omitted"));
    assert_eq!(final_stderr, "stderr-only");
    assert!(final_combined.contains("bytes omitted"));
    // stdout and stderr arrive on independent channels, so their relative
    // ordering is intentionally unspecified even though neither stream may be
    // lost from the combined capture.
    assert!(final_combined.contains("stderr-only"));
}

#[tokio::test]
async fn aborted_local_output_task_does_not_claim_output_eof() {
    let output_closed = Arc::new(AtomicBool::new(false));
    let output = OutputHandles {
        output_buffer: Arc::new(Mutex::new(HeadTailBuffer::default())),
        output_notify: Arc::new(Notify::new()),
        monitor_capture: Arc::new(std::sync::Mutex::new(None)),
        monitor_capture_rx: Arc::new(std::sync::Mutex::new(None)),
        monitor_stream_output: None,
        output_closed: Arc::clone(&output_closed),
        output_closed_notify: Arc::new(Notify::new()),
        cancellation_token: CancellationToken::new(),
    };
    let (_stdout_tx, stdout_rx) = mpsc::channel(1);
    let (_stderr_tx, stderr_rx) = mpsc::channel(1);
    let (output_tx, _) = broadcast::channel(1);
    let output_task_finished = Arc::new(AtomicBool::new(false));
    let task = UnifiedExecProcess::spawn_local_output_task(
        stdout_rx,
        stderr_rx,
        output,
        output_tx,
        Arc::clone(&output_task_finished),
        Arc::new(Notify::new()),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(Notify::new()),
    );

    task.abort();
    let _ = task.await;

    assert!(!output_closed.load(Ordering::Acquire));
    assert!(output_task_finished.load(Ordering::Acquire));
}

#[tokio::test]
async fn monitor_stop_unblocks_full_capture_channel() {
    let (capture_tx, mut capture_rx) = monitor_capture_channel(1);
    let output_closed = Arc::new(AtomicBool::new(false));
    let output = OutputHandles {
        output_buffer: Arc::new(Mutex::new(HeadTailBuffer::default())),
        output_notify: Arc::new(Notify::new()),
        monitor_capture: Arc::new(std::sync::Mutex::new(Some(capture_tx))),
        monitor_capture_rx: Arc::new(std::sync::Mutex::new(None)),
        monitor_stream_output: None,
        output_closed: Arc::clone(&output_closed),
        output_closed_notify: Arc::new(Notify::new()),
        cancellation_token: CancellationToken::new(),
    };
    let (stdout_tx, stdout_rx) = mpsc::channel(2);
    let (stderr_tx, stderr_rx) = mpsc::channel(1);
    let (output_tx, _) = broadcast::channel(1);
    let output_task_finished = Arc::new(AtomicBool::new(false));
    let monitor_stop_pending = Arc::new(AtomicUsize::new(0));
    let monitor_stop_pending_notify = Arc::new(Notify::new());
    let task = UnifiedExecProcess::spawn_local_output_task(
        stdout_rx,
        stderr_rx,
        output,
        output_tx,
        Arc::clone(&output_task_finished),
        Arc::new(Notify::new()),
        Arc::clone(&monitor_stop_pending),
        Arc::clone(&monitor_stop_pending_notify),
    );

    stdout_tx.send(b"first".to_vec()).await.unwrap();
    tokio::time::timeout(tokio::time::Duration::from_secs(1), async {
        while !capture_rx.is_full() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first capture chunk should fill the channel");
    stdout_tx.send(b"second".to_vec()).await.unwrap();
    tokio::task::yield_now().await;

    monitor_stop_pending.store(1, Ordering::Release);
    monitor_stop_pending_notify.notify_waiters();
    drop(stdout_tx);
    drop(stderr_tx);

    tokio::time::timeout(tokio::time::Duration::from_secs(1), task)
        .await
        .expect("monitor stop should release capture backpressure")
        .expect("output task should finish normally");
    assert!(output_task_finished.load(Ordering::Acquire));
    assert!(output_closed.load(Ordering::Acquire));

    assert_eq!(
        capture_rx.recv().await,
        Some(MonitorOutputChunk::Output {
            bytes: b"first".to_vec(),
            is_stdout: true,
        })
    );
    assert_eq!(
        capture_rx.recv().await,
        Some(MonitorOutputChunk::ArchiveGap),
        "a full capture channel during stop must leave an explicit archive gap"
    );
    assert_eq!(
        capture_rx.recv().await,
        None,
        "the bounded-loss marker must be emitted at most once"
    );
}
