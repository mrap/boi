use crate::spec;
use std::path::Path;

const BRAIN_CHAR_LIMIT: usize = 32_000;

/// Load CLAUDE.md from `path`, returning its content.
pub fn load_brain(path: &Path) -> Result<String, String> {
    let claude_md = path.join("CLAUDE.md");
    std::fs::read_to_string(&claude_md)
        .map_err(|e| format!("failed to read {}: {}", claude_md.display(), e))
}

fn truncate_to_char_limit(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    // Truncate at a char boundary
    match s.char_indices().nth(limit) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

pub fn build_prompt(spec_content: &str, task: &spec::BoiTask, brain: Option<&Path>) -> String {
    let task_spec = task.spec.as_deref().unwrap_or("(no spec provided)");
    let task_verify = task.verify.as_deref().unwrap_or("(no verify command)");

    let brain_section = brain
        .and_then(|p| load_brain(p).ok())
        .map(|content| {
            let truncated = truncate_to_char_limit(&content, BRAIN_CHAR_LIMIT).to_string();
            format!("## System Context\n\n{}\n\n", truncated)
        })
        .unwrap_or_default();

    format!(
        "{}You are a BOI worker. Execute exactly one task from this spec.\n\n\
        FULL SPEC:\n{}\n\n\
        YOUR TASK: {} — {}\n\n\
        SPEC:\n{}\n\n\
        VERIFY:\n{}\n\n\
        Execute the task. Do NOT modify the spec file — status is tracked externally.",
        brain_section, spec_content, task.id, task.title, task_spec, task_verify
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec;
    use std::fs;
    use std::path::PathBuf;

    fn make_task() -> spec::BoiTask {
        spec::BoiTask {
            id: "t-1".to_string(),
            title: "Setup Cargo".to_string(),
            status: spec::TaskStatus::Pending,
            depends: None,
            spec: Some("Run cargo init".to_string()),
            verify: Some("test -f Cargo.toml".to_string()),
            verify_prompt: None,
            phases: None,
        }
    }

    fn tmp_brain_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("boi_brain_test_{}", name));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_build_prompt_contains_task_fields() {
        let task = make_task();
        let prompt = build_prompt("title: Test\ntasks: []", &task, None);
        assert!(prompt.contains("t-1"));
        assert!(prompt.contains("Setup Cargo"));
        assert!(prompt.contains("Run cargo init"));
        assert!(prompt.contains("test -f Cargo.toml"));
    }

    #[test]
    fn test_brain_inject_present_in_prompt() {
        let dir = tmp_brain_dir("present");
        let brain_content = "# Project Rules\n\nDo not delete prod data.";
        fs::write(dir.join("CLAUDE.md"), brain_content).unwrap();

        let task = make_task();
        let prompt = build_prompt("title: Test\ntasks: []", &task, Some(&dir));

        assert!(prompt.starts_with("## System Context\n\n"));
        assert!(prompt.contains("Do not delete prod data."));
        assert!(prompt.contains("You are a BOI worker."));
    }

    #[test]
    fn test_brain_inject_truncation_works() {
        let dir = tmp_brain_dir("truncation");
        // 33_000 'a' chars — over the 32_000 limit
        let long_content = "a".repeat(33_000);
        fs::write(dir.join("CLAUDE.md"), &long_content).unwrap();

        let task = make_task();
        let prompt = build_prompt("title: Test\ntasks: []", &task, Some(&dir));

        // Brain section is present but content is truncated
        assert!(prompt.starts_with("## System Context\n\n"));
        let system_ctx_end = prompt.find("\n\nYou are a BOI worker.").unwrap();
        let brain_in_prompt = &prompt[..system_ctx_end];
        // Should be <= limit + small header overhead ("## System Context\n\n" = 20 chars)
        assert!(brain_in_prompt.len() <= BRAIN_CHAR_LIMIT + 100);
    }

    #[test]
    fn test_brain_inject_missing_brain_fails_gracefully() {
        let dir = tmp_brain_dir("missing");
        // No CLAUDE.md written — directory exists but file is absent

        // load_brain should return an error
        assert!(load_brain(&dir).is_err());

        // build_prompt should still produce a valid prompt without a brain section
        let task = make_task();
        let prompt = build_prompt("title: Test\ntasks: []", &task, Some(&dir));
        assert!(!prompt.starts_with("## System Context"));
        assert!(prompt.contains("You are a BOI worker."));
    }
}
