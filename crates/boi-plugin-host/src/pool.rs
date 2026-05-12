//! Typed client for the pool plugin, plus the WorkerEvent tee.
//!
//! Per Q7: as the host reads `WorkerEvent` chunks off the Tail stream
//! it tees the raw bytes to `~/.boi/logs/{spec_id}/{task_id}.log` so
//! that a host restart can resume from the last persisted offset by
//! ack'ing that offset on reconnect.

use std::io;
use std::path::{Path, PathBuf};

use boi_proto::pool::v1 as pb;
pub use pb::pool_client::PoolClient;
pub use pb::worker_event::Kind as WorkerEventKind;
pub use pb::{
    CancelRequest, CancelResponse, HandshakeRequest, HandshakeResponse, SpawnRequest,
    SpawnResponse, TailAck, WorkerEvent,
};

use tokio::fs::{create_dir_all, OpenOptions};
use tokio::io::AsyncWriteExt;

pub struct PoolPlugin<T> {
    pub inner: PoolClient<T>,
}

impl<T> PoolPlugin<T> {
    pub fn new(inner: PoolClient<T>) -> Self {
        Self { inner }
    }
}

/// Resolve the per-task tee log path under `~/.boi/logs/`.
pub fn tee_log_path(home: &Path, spec_id: &str, task_id: &str) -> PathBuf {
    home.join(".boi")
        .join("logs")
        .join(sanitize(spec_id))
        .join(format!("{}.log", sanitize(task_id)))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Append a raw stdout/stderr chunk to the task tee file, creating
/// parent directories on demand. Returns the new file length.
pub async fn append_chunk(path: &Path, bytes: &[u8]) -> io::Result<u64> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent).await?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(bytes).await?;
    f.flush().await?;
    let meta = f.metadata().await?;
    Ok(meta.len())
}

/// Extract the byte payload from a `WorkerEvent` for tee purposes.
/// Non-data events (exit_code / status) return `None`.
pub fn payload_for_tee(event: &WorkerEvent) -> Option<&[u8]> {
    match event.kind.as_ref()? {
        WorkerEventKind::StdoutChunk(b) | WorkerEventKind::StderrChunk(b) => Some(b),
        _ => None,
    }
}

/// Retention policy for the per-spec log directory tee'd by
/// [`append_chunk`]. Per §16 Q7: rotate oldest task logs once the
/// per-spec on-disk total exceeds `max_bytes`, and unconditionally
/// drop any log whose mtime is older than `max_age`.
#[derive(Clone, Copy)]
pub struct RetentionPolicy {
    pub max_age_secs: u64,
    pub max_bytes_per_spec: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            // 7 days
            max_age_secs: 7 * 24 * 60 * 60,
            // 100 MiB
            max_bytes_per_spec: 100 * 1024 * 1024,
        }
    }
}

/// Enforce [`RetentionPolicy`] over `~/.boi/logs/<spec_id>/`, deleting
/// task logs oldest-mtime-first until both caps hold. Returns the
/// number of files removed.
pub fn enforce_retention(
    spec_dir: &Path,
    policy: RetentionPolicy,
) -> io::Result<u32> {
    use std::time::SystemTime;

    let now = SystemTime::now();
    let mut entries: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    let rd = match std::fs::read_dir(spec_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(now);
        entries.push((path, mtime, meta.len()));
    }
    // Oldest first.
    entries.sort_by_key(|(_p, mtime, _len)| *mtime);

    let mut removed = 0u32;

    // Age cap.
    entries.retain(|(p, mtime, _len)| {
        let age = now.duration_since(*mtime).map(|d| d.as_secs()).unwrap_or(0);
        if age > policy.max_age_secs {
            if std::fs::remove_file(p).is_ok() {
                removed += 1;
            }
            false
        } else {
            true
        }
    });

    // Byte cap — drop oldest first until under cap.
    let mut total: u64 = entries.iter().map(|(_p, _m, n)| *n).sum();
    let mut i = 0;
    while total > policy.max_bytes_per_spec && i < entries.len() {
        let (path, _mtime, len) = &entries[i];
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*len);
            removed += 1;
        }
        i += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn tee_log_path_under_home_boi_logs() {
        let p = tee_log_path(Path::new("/home/x"), "spec1", "task-A");
        assert_eq!(p, PathBuf::from("/home/x/.boi/logs/spec1/task-A.log"));
    }

    #[test]
    fn sanitizes_path_segments() {
        let p = tee_log_path(Path::new("/h"), "../evil", "..");
        assert_eq!(p, PathBuf::from("/h/.boi/logs/.._evil/...log"));
    }

    #[tokio::test]
    async fn append_chunk_creates_and_grows_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a/b/task.log");
        let n1 = append_chunk(&path, b"hello ").await.unwrap();
        let n2 = append_chunk(&path, b"world").await.unwrap();
        assert_eq!(n1, 6);
        assert_eq!(n2, 11);
        let body = tokio::fs::read(&path).await.unwrap();
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn payload_for_tee_extracts_stdout_and_stderr() {
        let ev = WorkerEvent {
            worker_id: "w".into(),
            offset: 0,
            kind: Some(WorkerEventKind::StdoutChunk(b"abc".to_vec())),
        };
        assert_eq!(payload_for_tee(&ev), Some(&b"abc"[..]));
        let ev = WorkerEvent {
            worker_id: "w".into(),
            offset: 0,
            kind: Some(WorkerEventKind::ExitCode(0)),
        };
        assert_eq!(payload_for_tee(&ev), None);
    }
}
