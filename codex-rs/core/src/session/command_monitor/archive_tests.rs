use super::super::output::MonitorBatch;
use super::super::output::PhysicalLineFramer;
use super::super::worker::handle_capture_chunk;
use super::*;
use pretty_assertions::assert_eq;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

#[tokio::test]
async fn output_file_creation_never_truncates_an_existing_task_file() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000000.output");
    let mut file = create_output_file(&path)
        .await
        .expect("first creation should succeed");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(temp_dir.path())
                .await
                .expect("archive directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            file.metadata()
                .await
                .expect("archive metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    file.write_all(b"preserve")
        .await
        .expect("fixture should be written");
    file.flush().await.expect("fixture should be flushed");

    let err = create_output_file(&path)
        .await
        .expect_err("second creation should fail");

    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        tokio::fs::read(&path)
            .await
            .expect("existing file should remain readable"),
        b"preserve"
    );
}

#[tokio::test]
async fn archive_keeps_both_streams_but_framer_consumes_only_stdout() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000001.output");
    let file = create_output_file(&path)
        .await
        .expect("output file should be created");
    let mut file = MonitorArchiveWriter::new(file, MonitorArchiveBudget::with_cap(u64::MAX));
    let mut lines = PhysicalLineFramer::default();
    let mut batch = MonitorBatch::default();
    let mut produced_output = false;

    handle_capture_chunk(
        MonitorOutputChunk::Output {
            bytes: b"stderr-only\n".to_vec(),
            is_stdout: false,
        },
        &mut file,
        &mut lines,
        &mut batch,
        &mut produced_output,
        true,
    )
    .await
    .expect("stderr should be archived");
    handle_capture_chunk(
        MonitorOutputChunk::Output {
            bytes: b"stdout-only\n".to_vec(),
            is_stdout: true,
        },
        &mut file,
        &mut lines,
        &mut batch,
        &mut produced_output,
        true,
    )
    .await
    .expect("stdout should be archived and framed");
    file.flush().await.expect("archive should flush");

    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("archive should be readable"),
        b"stderr-only\nstdout-only\n"
    );
    assert_eq!(batch.take(), Some("stdout-only".to_string()));
    assert!(produced_output);
}

#[tokio::test]
async fn stdout_close_flushes_a_partial_line_while_stderr_stays_open() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000002.output");
    let file = create_output_file(&path)
        .await
        .expect("output file should be created");
    let mut file = MonitorArchiveWriter::new(file, MonitorArchiveBudget::with_cap(u64::MAX));
    let mut lines = PhysicalLineFramer::default();
    let mut batch = MonitorBatch::default();
    let mut produced_output = false;

    handle_capture_chunk(
        MonitorOutputChunk::Output {
            bytes: b"partial".to_vec(),
            is_stdout: true,
        },
        &mut file,
        &mut lines,
        &mut batch,
        &mut produced_output,
        true,
    )
    .await
    .expect("stdout should be archived");
    handle_capture_chunk(
        MonitorOutputChunk::StdoutClosed,
        &mut file,
        &mut lines,
        &mut batch,
        &mut produced_output,
        true,
    )
    .await
    .expect("stdout close should flush the framer");

    assert_eq!(batch.take().as_deref(), Some("partial"));
    assert!(produced_output);
    file.flush().await.expect("archive should flush");
    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("archive should be readable"),
        b"partial"
    );
}

#[tokio::test]
async fn archive_gap_event_writes_one_bounded_marker() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000003.output");
    let file = create_output_file(&path)
        .await
        .expect("output file should be created");
    let mut writer = MonitorArchiveWriter::new(file, MonitorArchiveBudget::with_cap(u64::MAX));

    archive_capture_chunk(MonitorOutputChunk::ArchiveGap, &mut writer)
        .await
        .expect("the archive gap marker should be writable");
    archive_capture_chunk(MonitorOutputChunk::ArchiveGap, &mut writer)
        .await
        .expect("a duplicate archive gap should be harmless");
    writer.flush().await.expect("archive should flush");

    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("archive should be readable"),
        ARCHIVE_CAPTURE_GAP_MARKER,
        "one monitor writes at most one fixed-size gap marker"
    );
}

#[tokio::test]
async fn archive_discards_the_crossing_chunk_and_writes_one_disk_cap_marker() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let path = temp_dir.path().join("b00000002.output");
    let file = create_output_file(&path)
        .await
        .expect("output file should be created");
    let mut writer = MonitorArchiveWriter::with_cap_and_budget(
        file,
        5,
        MonitorArchiveBudget::with_cap(u64::MAX),
    );

    writer
        .write_chunk(b"1234")
        .await
        .expect("chunk within cap should be written");
    writer
        .write_chunk(b"56")
        .await
        .expect("crossing chunk should be replaced by marker");
    writer
        .write_chunk(b"ignored")
        .await
        .expect("later chunks should be discarded");
    writer.flush().await.expect("archive should flush");

    let mut expected = b"1234".to_vec();
    expected.extend_from_slice(ARCHIVE_TRUNCATION_MARKER);
    assert_eq!(
        tokio::fs::read(path)
            .await
            .expect("archive should be readable"),
        expected
    );
}

#[tokio::test]
async fn multiple_archive_writers_share_one_session_content_budget() {
    let temp_dir = tempfile::tempdir().expect("temporary directory should be created");
    let first_path = temp_dir.path().join("b00000003.output");
    let second_path = temp_dir.path().join("b00000004.output");
    let first_file = create_output_file(&first_path)
        .await
        .expect("first output file should be created");
    let second_file = create_output_file(&second_path)
        .await
        .expect("second output file should be created");
    let budget = MonitorArchiveBudget::with_cap(5);
    let mut first = MonitorArchiveWriter::with_cap_and_budget(first_file, 5, Arc::clone(&budget));
    let mut second = MonitorArchiveWriter::with_cap_and_budget(second_file, 5, Arc::clone(&budget));

    let (first_result, second_result) =
        tokio::join!(first.write_chunk(b"1234"), second.write_chunk(b"5678"),);
    first_result.expect("one writer should reserve content bytes");
    second_result.expect("the other writer should write only its cap marker");
    first.flush().await.expect("first archive should flush");
    second.flush().await.expect("second archive should flush");

    assert_eq!(budget.content_bytes(), 4);
    let contents = [
        tokio::fs::read(first_path)
            .await
            .expect("first archive should be readable"),
        tokio::fs::read(second_path)
            .await
            .expect("second archive should be readable"),
    ];
    assert_eq!(
        contents
            .iter()
            .filter(|content| content.as_slice() == b"1234" || content.as_slice() == b"5678")
            .count(),
        1
    );
    assert_eq!(
        contents
            .iter()
            .filter(|content| content.as_slice() == ARCHIVE_TRUNCATION_MARKER)
            .count(),
        1
    );
}

struct FailAfterWriter {
    written: usize,
    fail_after: usize,
}

impl AsyncWrite for FailAfterWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.written >= self.fail_after {
            return Poll::Ready(Err(io::Error::other("injected archive write failure")));
        }
        let written = bytes.len().min(self.fail_after - self.written);
        self.written += written;
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn archive_write_failure_releases_only_unwritten_reservation() {
    let budget = MonitorArchiveBudget::with_cap(10);
    let mut failing = MonitorArchiveWriter::with_cap_and_budget(
        FailAfterWriter {
            written: 0,
            fail_after: 2,
        },
        10,
        Arc::clone(&budget),
    );

    failing
        .write_chunk(b"12345")
        .await
        .expect_err("fixture should fail after a partial write");

    assert_eq!(budget.content_bytes(), 2);
    let mut succeeding =
        MonitorArchiveWriter::with_cap_and_budget(tokio::io::sink(), 10, Arc::clone(&budget));
    succeeding
        .write_chunk(b"12345678")
        .await
        .expect("rolled-back bytes should remain available to another writer");
    assert_eq!(budget.content_bytes(), 10);
}
