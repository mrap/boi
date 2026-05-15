use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Mutex,
    time::Instant,
};

use crate::{
    hooks::HookConfig,
    queue::Queue,
    telemetry::Telemetry,
    worker::{self, WorkerConfig},
};

use super::{JobId, JobOutput, JobStatus, WorkerPool};

struct WorkerEntry {
    handle: std::thread::JoinHandle<()>,
    spec_id: String,
    spawned_at: Instant,
    timeout_secs: u64,
    terminal: Option<JobStatus>,
}

/// Thread-pool backend: each `spawn()` call starts one OS thread running
/// `worker::run_worker()`, matching the existing daemon behavior exactly.
pub struct LocalThreadPool {
    queue_path: String,
    hook_config: HookConfig,
    max_workers_count: u32,
    workers: Mutex<HashMap<JobId, WorkerEntry>>,
}

impl LocalThreadPool {
    pub fn new(queue_path: impl Into<String>, hook_config: HookConfig, max_workers: u32) -> Self {
        LocalThreadPool {
            queue_path: queue_path.into(),
            hook_config,
            max_workers_count: max_workers,
            workers: Mutex::new(HashMap::new()),
        }
    }
}

// SAFETY: Mutex<HashMap<…>> is Send+Sync; JoinHandle<()> is Send.
// HookConfig and String are Send+Sync.
unsafe impl Send for LocalThreadPool {}
unsafe impl Sync for LocalThreadPool {}

impl WorkerPool for LocalThreadPool {
    fn spawn(
        &self,
        spec_id: &str,
        spec_path: &str,
        workspace_path: &str,
        config: &WorkerConfig,
    ) -> anyhow::Result<JobId> {
        let job_id = JobId::new(spec_id);
        let sid = spec_id.to_string();
        let spath = spec_path.to_string();
        let qpath = workspace_path.to_string();
        let hc = self.hook_config.clone();
        let timeout = config.task_timeout_secs;
        let wc = WorkerConfig {
            max_workers: 1,
            task_timeout_secs: config.task_timeout_secs,
            retry_count: config.retry_count,
            cleanup_on_failure: config.cleanup_on_failure,
            claude_bin: config.claude_bin.clone(),
            models: config.models.clone(),
            convergence_threshold: config.convergence_threshold,
        };
        let tel = Telemetry::new(PathBuf::from(&qpath));

        let handle = std::thread::spawn(move || {
            if let Err(e) = worker::run_worker(&sid, &spath, &qpath, &hc, &wc, &tel) {
                eprintln!("[pool/local] worker error for {}: {}", sid, e);
            }
        });

        let mut map = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(
            job_id.clone(),
            WorkerEntry {
                handle,
                spec_id: spec_id.to_string(),
                spawned_at: Instant::now(),
                timeout_secs: timeout,
                terminal: None,
            },
        );
        Ok(job_id)
    }

    fn status(&self, job_id: &JobId) -> anyhow::Result<JobStatus> {
        let mut map = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        let entry = match map.get_mut(job_id) {
            Some(e) => e,
            None => return Ok(JobStatus::Unknown),
        };

        if let Some(ref s) = entry.terminal {
            return Ok(s.clone());
        }

        if entry.spawned_at.elapsed().as_secs() > entry.timeout_secs {
            entry.terminal = Some(JobStatus::Timeout);
            return Ok(JobStatus::Timeout);
        }

        if entry.handle.is_finished() {
            let status = match Queue::open(&self.queue_path) {
                Ok(q) => match q.status(&entry.spec_id) {
                    Ok(Some(s)) if s.spec.status == "completed" => JobStatus::Completed,
                    _ => JobStatus::Failed,
                },
                Err(_) => JobStatus::Failed,
            };
            entry.terminal = Some(status.clone());
            Ok(status)
        } else {
            Ok(JobStatus::Running)
        }
    }

    fn collect(&self, job_id: &JobId) -> anyhow::Result<JobOutput> {
        let map = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map
            .get(job_id)
            .ok_or_else(|| anyhow::anyhow!("unknown job {}", job_id))?;

        let exit_code = match Queue::open(&self.queue_path) {
            Ok(q) => match q.status(&entry.spec_id) {
                Ok(Some(s)) if s.spec.status == "completed" => 0,
                _ => 1,
            },
            Err(_) => 1,
        };

        Ok(JobOutput {
            exit_code,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    fn cancel(&self, job_id: &JobId) -> anyhow::Result<()> {
        let map = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        let entry = match map.get(job_id) {
            Some(e) => e,
            None => return Ok(()),
        };

        if entry.terminal.is_some() {
            return Ok(());
        }

        // SIGTERM the spec's process group via the pid file written by spawn_claude.
        // ESRCH is silently ignored if the group no longer exists (idempotent).
        let pid_file = crate::spawn::pid_file_for(&entry.spec_id);
        if let Ok(content) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = content.trim().parse::<i32>() {
                // SAFETY: kill(-pid, SIGTERM) targets the process group whose PGID == pid.
                // ESRCH is returned (and ignored) when the group no longer exists.
                unsafe { libc::kill(-pid, libc::SIGTERM) };
            }
        }

        if let Ok(q) = Queue::open(&self.queue_path) {
            let _ = q.cancel(&entry.spec_id);
        }

        Ok(())
    }

    fn cleanup(&self, job_id: &JobId) -> anyhow::Result<()> {
        let mut map = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(job_id);
        Ok(())
    }

    fn max_workers(&self) -> u32 {
        self.max_workers_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TmpDb(String);

    impl TmpDb {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            TmpDb(format!("/tmp/boi_pool_local_test_{}.db", n))
        }
        fn path(&self) -> &str {
            &self.0
        }
    }

    impl Drop for TmpDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn pool(db: &TmpDb) -> LocalThreadPool {
        LocalThreadPool::new(db.path(), HookConfig::default(), 5)
    }

    #[test]
    fn status_unknown_job_returns_unknown() {
        let db = TmpDb::new();
        let p = pool(&db);
        let jid = JobId::new("nonexistent-job");
        assert_eq!(p.status(&jid).unwrap(), JobStatus::Unknown);
    }

    #[test]
    fn cancel_unknown_job_is_idempotent() {
        let db = TmpDb::new();
        let p = pool(&db);
        let jid = JobId::new("ghost-job");
        assert!(p.cancel(&jid).is_ok());
        assert!(p.cancel(&jid).is_ok());
    }

    #[test]
    fn cleanup_unknown_job_is_ok() {
        let db = TmpDb::new();
        let p = pool(&db);
        let jid = JobId::new("gone-job");
        assert!(p.cleanup(&jid).is_ok());
    }

    #[test]
    fn max_workers_reflects_constructor() {
        let db = TmpDb::new();
        let p = pool(&db);
        assert_eq!(p.max_workers(), 5);
    }

    #[test]
    fn cancel_then_cleanup_is_safe() {
        let db = TmpDb::new();
        let p = pool(&db);
        let jid = JobId::new("seq-job");
        p.cancel(&jid).unwrap();
        p.cleanup(&jid).unwrap();
        // Second cancel after cleanup should still be idempotent
        p.cancel(&jid).unwrap();
    }

    #[test]
    fn local_pool_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LocalThreadPool>();
    }
}
