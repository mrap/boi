use boi::queue::Queue;
use boi::spec::{BoiSpec, BoiTask};

fn tmp_db(name: &str) -> String {
    let path = std::env::temp_dir().join(format!("boi-test-{}-{}.db", name, std::process::id()));
    path.to_str().unwrap().to_string()
}

fn make_spec_with_phases() -> BoiSpec {
    BoiSpec {
        title: "phases-persistence-test".to_string(),
        mode: Some("execute".to_string()),
        workspace: None,
        workspace_rationale: Some("test fixture".to_string()),
        initiative: None,
        context: None,
        outcomes: None,
        spec_phases: Some(vec!["plan-critique".to_string(), "critic".to_string()]),
        task_phases: Some(vec!["execute".to_string(), "code-review".to_string()]),
        context_files: None,
        phase_overrides: std::collections::HashMap::new(),
        worker_pool: None,
        max_cost_usd: None,
        key_artifacts: None,
        tasks: vec![BoiTask {
            id: "t-1".to_string(),
            title: "dummy task".to_string(),
            status: Default::default(),
            spec: Some("do something".to_string()),
            verify: None,
            verify_prompt: None,
            phases: None,
            depends: None,
        }],
    }
}

#[test]
fn test_task_phases_roundtrip() {
    let db = tmp_db("phases-roundtrip");
    let q = Queue::open(&db).unwrap();

    let spec = make_spec_with_phases();
    let spec_id = q.enqueue(&spec, None).unwrap();

    let rec = q.dequeue().unwrap().expect("should have a record");
    assert_eq!(rec.id, spec_id);

    let stored_task_phases: Vec<String> = serde_json::from_str(
        rec.task_phases.as_deref().expect("task_phases must not be NULL"),
    )
    .expect("task_phases must be valid JSON");
    assert_eq!(stored_task_phases, vec!["execute", "code-review"]);

    let stored_spec_phases: Vec<String> = serde_json::from_str(
        rec.spec_phases.as_deref().expect("spec_phases must not be NULL"),
    )
    .expect("spec_phases must be valid JSON");
    assert_eq!(stored_spec_phases, vec!["plan-critique", "critic"]);
}

#[test]
fn test_task_phases_null_when_unset() {
    let db = tmp_db("phases-null");
    let q = Queue::open(&db).unwrap();

    let mut spec = make_spec_with_phases();
    spec.task_phases = None;
    spec.spec_phases = None;

    q.enqueue(&spec, None).unwrap();
    let rec = q.dequeue().unwrap().expect("should have a record");

    assert!(rec.task_phases.is_none(), "task_phases must be NULL when unset");
    assert!(rec.spec_phases.is_none(), "spec_phases must be NULL when unset");
}

#[test]
fn test_reopen_db_preserves_phases() {
    let db = tmp_db("phases-reopen");

    {
        let q = Queue::open(&db).unwrap();
        let spec = make_spec_with_phases();
        q.enqueue(&spec, None).unwrap();
    }

    // Re-open simulates daemon restart
    let q2 = Queue::open(&db).unwrap();
    let rec = q2.dequeue().unwrap().expect("should survive restart");

    let task_phases: Vec<String> = serde_json::from_str(
        rec.task_phases.as_deref().expect("task_phases persisted after restart"),
    )
    .unwrap();
    assert_eq!(task_phases, vec!["execute", "code-review"]);
}
