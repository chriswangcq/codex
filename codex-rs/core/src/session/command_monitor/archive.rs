//! Bounded local archives; output retention never decides task state.

use crate::unified_exec::MonitorArchiveBudget;
use crate::unified_exec::MonitorOutputChunk;
use std::io;
use std::path::Path;
use std::sync::Arc;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

const MAX_ARCHIVE_BYTES: u64 = crate::unified_exec::MAX_MONITOR_ARCHIVE_BYTES;
const ARCHIVE_TRUNCATION_MARKER: &[u8] = b"\n[output truncated: exceeded 5GB disk cap]\n";
const ARCHIVE_CAPTURE_GAP_MARKER: &[u8] =
    b"\n[output omitted: monitor capture buffer was full while stopping]\n";

pub(super) struct MonitorArchiveWriter<W = tokio::fs::File> {
    file: W,
    bytes_written: u64,
    cap: u64,
    truncated: bool,
    capture_gap_written: bool,
    archive_budget: Arc<MonitorArchiveBudget>,
}

impl MonitorArchiveWriter<tokio::fs::File> {
    pub(super) fn new(file: tokio::fs::File, archive_budget: Arc<MonitorArchiveBudget>) -> Self {
        Self::with_cap_and_budget(file, MAX_ARCHIVE_BYTES, archive_budget)
    }
}

impl<W> MonitorArchiveWriter<W>
where
    W: AsyncWrite + Unpin,
{
    fn with_cap_and_budget(file: W, cap: u64, archive_budget: Arc<MonitorArchiveBudget>) -> Self {
        Self {
            file,
            bytes_written: 0,
            cap,
            truncated: false,
            capture_gap_written: false,
            archive_budget,
        }
    }

    pub(super) async fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        if self.truncated {
            return Ok(());
        }
        let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        let Some(mut reservation) = self.archive_budget.reserve(chunk_len) else {
            self.file.write_all(ARCHIVE_TRUNCATION_MARKER).await?;
            self.truncated = true;
            return Ok(());
        };
        if self.bytes_written.saturating_add(chunk_len) > self.cap {
            drop(reservation);
            self.file.write_all(ARCHIVE_TRUNCATION_MARKER).await?;
            self.truncated = true;
            return Ok(());
        }
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let written = self.file.write(remaining).await?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write command monitor archive chunk",
                ));
            }
            let written_u64 = u64::try_from(written).unwrap_or(u64::MAX);
            reservation.commit_written(written_u64);
            self.bytes_written = self.bytes_written.saturating_add(written_u64);
            remaining = &remaining[written..];
        }
        Ok(())
    }

    pub(super) async fn write_capture_gap_marker(&mut self) -> io::Result<()> {
        if !self.capture_gap_written {
            self.file.write_all(ARCHIVE_CAPTURE_GAP_MARKER).await?;
            self.capture_gap_written = true;
        }
        Ok(())
    }

    pub(super) async fn flush(&mut self) -> io::Result<()> {
        self.file.flush().await
    }
}

pub(super) async fn archive_capture_chunk(
    chunk: MonitorOutputChunk,
    archive_file: &mut MonitorArchiveWriter,
) -> io::Result<()> {
    match chunk {
        MonitorOutputChunk::Output { bytes, .. } => archive_file.write_chunk(&bytes).await,
        MonitorOutputChunk::StdoutClosed => Ok(()),
        MonitorOutputChunk::ArchiveGap => archive_file.write_capture_gap_marker().await,
    }
}

pub(super) async fn create_output_file(path: &Path) -> io::Result<tokio::fs::File> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "output file has no parent directory",
        )
    })?;
    tokio::fs::create_dir_all(parent).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).await
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
