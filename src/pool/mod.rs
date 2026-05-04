use crate::worker::WorkerConfig;

pub mod local;
pub use local::LocalThreadPool;

/// Opaque identifier for a running job, returned by WorkerPool::spawn.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobId(pub String);

impl JobId {
    pub fn new(id: impl Into<String>) -> Self {
        JobId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for JobId {
    fn from(s: String) -> Self {
        JobId(s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
    Timeout,
    Unknown,
}

pub struct JobOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Pluggable backend for running BOI spec workers.
///
/// Every method operates on the same five-operation contract from
/// docs/extensibility/worker-pool-providers.md.  The `cleanup` method has a
/// default no-op implementation because it is optional per the design doc.
pub trait WorkerPool: Send + Sync {
    fn spawn(
        &self,
        spec_id: &str,
        spec_path: &str,
        workspace_path: &str,
        config: &WorkerConfig,
    ) -> anyhow::Result<JobId>;

    fn status(&self, job_id: &JobId) -> anyhow::Result<JobStatus>;

    fn collect(&self, job_id: &JobId) -> anyhow::Result<JobOutput>;

    fn cancel(&self, job_id: &JobId) -> anyhow::Result<()>;

    fn cleanup(&self, job_id: &JobId) -> anyhow::Result<()> {
        let _ = job_id;
        Ok(())
    }

    fn max_workers(&self) -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::WorkerConfig;
    use std::sync::{Arc, Mutex};

    struct MockPool {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl MockPool {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (MockPool { calls: calls.clone() }, calls)
        }
    }

    impl WorkerPool for MockPool {
        fn spawn(
            &self,
            spec_id: &str,
            _spec_path: &str,
            _workspace_path: &str,
            _config: &WorkerConfig,
        ) -> anyhow::Result<JobId> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("spawn:{}", spec_id));
            Ok(JobId::new(format!("job-{}", spec_id)))
        }

        fn status(&self, job_id: &JobId) -> anyhow::Result<JobStatus> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("status:{}", job_id.as_str()));
            Ok(JobStatus::Completed)
        }

        fn collect(&self, job_id: &JobId) -> anyhow::Result<JobOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("collect:{}", job_id.as_str()));
            Ok(JobOutput {
                exit_code: 0,
                stdout: "done".to_string(),
                stderr: String::new(),
            })
        }

        fn cancel(&self, job_id: &JobId) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("cancel:{}", job_id.as_str()));
            Ok(())
        }

        fn cleanup(&self, job_id: &JobId) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("cleanup:{}", job_id.as_str()));
            Ok(())
        }

        fn max_workers(&self) -> u32 {
            4
        }
    }

    #[test]
    fn mock_pool_records_spawn_status_collect_cleanup() {
        let (mock, calls) = MockPool::new();
        let pool: Box<dyn WorkerPool> = Box::new(mock);
        let config = WorkerConfig::default();

        let job_id = pool
            .spawn("spec-abc", "/tmp/spec.yaml", "/tmp/ws", &config)
            .unwrap();
        let status = pool.status(&job_id).unwrap();
        assert_eq!(status, JobStatus::Completed);
        let output = pool.collect(&job_id).unwrap();
        assert_eq!(output.exit_code, 0);
        pool.cleanup(&job_id).unwrap();

        let log = calls.lock().unwrap();
        assert_eq!(log.len(), 4);
        assert_eq!(log[0], "spawn:spec-abc");
        assert_eq!(log[1], "status:job-spec-abc");
        assert_eq!(log[2], "collect:job-spec-abc");
        assert_eq!(log[3], "cleanup:job-spec-abc");
    }

    #[test]
    fn mock_pool_cancel_records_call() {
        let (mock, calls) = MockPool::new();
        let pool: Box<dyn WorkerPool> = Box::new(mock);
        let config = WorkerConfig::default();

        let job_id = pool
            .spawn("spec-xyz", "/tmp/spec.yaml", "/tmp/ws", &config)
            .unwrap();
        pool.cancel(&job_id).unwrap();
        pool.cleanup(&job_id).unwrap();

        let log = calls.lock().unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0], "spawn:spec-xyz");
        assert_eq!(log[1], "cancel:job-spec-xyz");
        assert_eq!(log[2], "cleanup:job-spec-xyz");
    }

    #[test]
    fn mock_pool_trait_object_is_dyn_compatible() {
        fn accept_pool(_p: &dyn WorkerPool) {}
        let (mock, _) = MockPool::new();
        accept_pool(&mock);
    }

    #[test]
    fn mock_pool_default_cleanup_noop() {
        let jid = JobId::new("free-jid");
        // Default cleanup (trait default) returns Ok — call via concrete noop impl
        struct NoopPool;
        impl WorkerPool for NoopPool {
            fn spawn(&self, id: &str, _: &str, _: &str, _: &WorkerConfig) -> anyhow::Result<JobId> {
                Ok(JobId::new(id))
            }
            fn status(&self, _: &JobId) -> anyhow::Result<JobStatus> { Ok(JobStatus::Running) }
            fn collect(&self, _: &JobId) -> anyhow::Result<JobOutput> {
                Ok(JobOutput { exit_code: 0, stdout: String::new(), stderr: String::new() })
            }
            fn cancel(&self, _: &JobId) -> anyhow::Result<()> { Ok(()) }
            fn max_workers(&self) -> u32 { 1 }
            // cleanup intentionally not overridden — uses trait default
        }
        let pool: Box<dyn WorkerPool> = Box::new(NoopPool);
        assert!(pool.cleanup(&jid).is_ok());
    }
}
