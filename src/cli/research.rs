use crate::{config, fmt::ensure_db_dir, hooks, queue, spec};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

struct Brief {
    question: String,
    angles: Vec<String>,
    deliverable: String,
}

fn flush_section(
    section: u8,
    buf: &str,
    question: &mut String,
    angles: &mut Vec<String>,
    deliverable: &mut String,
) {
    let trimmed = buf.trim();
    match section {
        1 => *question = trimmed.to_string(),
        2 => {
            for line in trimmed.lines() {
                let line = line.trim().trim_start_matches('-').trim().to_string();
                if !line.is_empty() {
                    angles.push(line);
                }
            }
        }
        3 => *deliverable = trimmed.to_string(),
        _ => {}
    }
}

fn parse_brief(content: &str) -> Brief {
    // 0=none, 1=question, 2=angles, 3=deliverable, 4=other
    let mut current: u8 = 0;
    let mut question = String::new();
    let mut angles: Vec<String> = Vec::new();
    let mut deliverable = String::new();
    let mut buf = String::new();

    for line in content.lines() {
        let stripped = line.trim();
        if stripped.starts_with('#') {
            flush_section(current, &buf, &mut question, &mut angles, &mut deliverable);
            buf.clear();
            let heading = stripped.trim_start_matches('#').trim().to_lowercase();
            current = match heading.as_str() {
                "question" => 1,
                "angles" => 2,
                "deliverable" => 3,
                _ => 4,
            };
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    flush_section(current, &buf, &mut question, &mut angles, &mut deliverable);

    Brief { question, angles, deliverable }
}

/// Extract top N probable entity tokens from text (capitalized non-stop-words, insertion order).
fn extract_entities(text: &str, n: usize) -> Vec<String> {
    const STOP: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being",
        "have", "has", "had", "do", "does", "did", "will", "would", "could",
        "should", "may", "might", "shall", "can", "i", "we", "you", "he",
        "she", "it", "they", "this", "that", "these", "those", "what", "how",
        "why", "when", "where", "which", "who", "and", "or", "but", "if",
        "in", "on", "at", "to", "for", "of", "with", "by", "from", "about",
        "into", "through", "during", "before", "after", "above", "below",
        "between", "each", "few", "more", "most", "other", "some", "such",
        "no", "not", "only", "same", "so", "than", "too", "very", "just",
        "both", "there", "here", "their", "our", "its", "my", "your",
    ];

    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered: Vec<String> = Vec::new();

    for word in text.split_whitespace() {
        let clean: String = word.chars().filter(|c| c.is_alphanumeric() || *c == '-').collect();
        if clean.is_empty() {
            continue;
        }
        if clean.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
            && !STOP.contains(&clean.to_lowercase().as_str())
            && !seen.contains(&clean)
        {
            seen.insert(clean.clone());
            ordered.push(clean);
            if ordered.len() >= n {
                break;
            }
        }
    }

    // Fall back to top-frequency meaningful words if not enough capitalized tokens
    if ordered.len() < n {
        let mut freq: HashMap<String, usize> = HashMap::new();
        for word in text.split_whitespace() {
            let lower: String = word.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
            if lower.len() > 3 && !STOP.contains(&lower.as_str()) {
                *freq.entry(lower).or_insert(0) += 1;
            }
        }
        let mut by_freq: Vec<(String, usize)> = freq.into_iter().collect();
        by_freq.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        for (word, _) in by_freq {
            if ordered.len() >= n {
                break;
            }
            let titled: String = {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                }
            };
            if !seen.contains(&titled) {
                seen.insert(titled.clone());
                ordered.push(titled);
            }
        }
    }

    ordered
}

fn make_angle_spec(question: &str, angle: &str, idx: usize) -> spec::BoiSpec {
    let q_short: String = question.chars().take(60).collect();
    spec::BoiSpec {
        title: format!("Research: {} — angle: {}", q_short, angle),
        mode: Some("execute".to_string()),
        workspace: None,
        initiative: None,
        context: Some(format!("Research angle: {}\n\nQuestion: {}", angle, question)),
        outcomes: None,
        spec_phases: None,
        task_phases: None,
        context_files: None,
        phase_overrides: HashMap::new(),
        worker_pool: None,
        tasks: vec![spec::BoiTask {
            id: format!("T{:04}", idx + 1),
            title: format!("Investigate: {}", angle),
            status: spec::TaskStatus::Pending,
            depends: None,
            spec: Some(format!(
                "Research the following angle of the question:\n\n\
                 Question: {}\n\n\
                 Angle: {}\n\n\
                 Produce a concise findings document covering:\n\
                 - Key facts and data points\n\
                 - Relevant examples or evidence\n\
                 - Implications for the main question\n\
                 Write findings to research-angle-{}.md",
                question, angle, idx + 1
            )),
            verify: Some(format!("test -f research-angle-{}.md", idx + 1)),
            verify_prompt: None,
            phases: None,
        }],
    }
}

fn make_synthesis_spec(question: &str, angle_count: usize) -> spec::BoiSpec {
    let q_short: String = question.chars().take(80).collect();
    spec::BoiSpec {
        title: format!("Research synthesis: {}", q_short),
        mode: Some("execute".to_string()),
        workspace: None,
        initiative: None,
        context: Some(format!(
            "Synthesize findings from {} research angles for: {}",
            angle_count, question
        )),
        outcomes: None,
        spec_phases: None,
        task_phases: None,
        context_files: None,
        phase_overrides: HashMap::new(),
        worker_pool: None,
        tasks: vec![spec::BoiTask {
            id: "T0001".to_string(),
            title: "Synthesize angle findings".to_string(),
            status: spec::TaskStatus::Pending,
            depends: None,
            spec: Some(format!(
                "Read all research-angle-*.md files produced by prior angle specs.\n\n\
                 Question: {}\n\n\
                 Synthesize into a unified analysis:\n\
                 - Common themes across angles\n\
                 - Contradictions or tensions\n\
                 - Overall answer to the question\n\
                 Write synthesis to research-synthesis.md",
                question
            )),
            verify: Some("test -f research-synthesis.md".to_string()),
            verify_prompt: None,
            phases: None,
        }],
    }
}

fn make_deliverable_spec(question: &str, deliverable: &str) -> spec::BoiSpec {
    let q_short: String = question.chars().take(80).collect();
    let deliverable_desc = if deliverable.is_empty() {
        "A final research report answering the question."
    } else {
        deliverable
    };
    spec::BoiSpec {
        title: format!("Research deliverable: {}", q_short),
        mode: Some("execute".to_string()),
        workspace: None,
        initiative: None,
        context: Some(format!("Produce final deliverable for research: {}", question)),
        outcomes: None,
        spec_phases: None,
        task_phases: None,
        context_files: None,
        phase_overrides: HashMap::new(),
        worker_pool: None,
        tasks: vec![spec::BoiTask {
            id: "T0001".to_string(),
            title: "Produce deliverable".to_string(),
            status: spec::TaskStatus::Pending,
            depends: None,
            spec: Some(format!(
                "Read research-synthesis.md.\n\n\
                 Question: {}\n\n\
                 Deliverable: {}\n\n\
                 Produce the final deliverable from the synthesis. \
                 Write to research-deliverable.md",
                question, deliverable_desc
            )),
            verify: Some("test -f research-deliverable.md".to_string()),
            verify_prompt: None,
            phases: None,
        }],
    }
}

pub fn cmd_research(
    brief_path: &PathBuf,
    threads: usize,
    project: Option<&str>,
    db_str: &str,
    hook_cfg: &hooks::HookConfig,
) {
    let content = match std::fs::read_to_string(brief_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read {:?}: {}", brief_path, e);
            std::process::exit(1);
        }
    };

    let mut brief = parse_brief(&content);

    // Generate angles if missing or insufficient
    if brief.angles.len() < threads {
        let needed = threads - brief.angles.len();
        let generated = extract_entities(&brief.question, needed);
        brief.angles.extend(generated);
    }
    brief.angles.truncate(threads);

    // Ultimate fallback
    while brief.angles.len() < threads {
        brief.angles.push(format!("Aspect {}", brief.angles.len() + 1));
    }

    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    let cfg = config::load();
    let context_paths: Vec<String> = cfg
        .context
        .as_ref()
        .and_then(|c| c.always_include.as_ref())
        .cloned()
        .unwrap_or_default();
    let project_context = if context_paths.is_empty() {
        None
    } else {
        let ctx = queue::read_context_files(&context_paths);
        if ctx.is_empty() { None } else { Some(ctx) }
    };

    // Dispatch N angle specs
    let mut angle_ids: Vec<String> = Vec::new();
    for (i, angle) in brief.angles.iter().enumerate() {
        let angle_spec = make_angle_spec(&brief.question, angle, i);
        let title = angle_spec.title.clone();
        let spec_id = match q.enqueue_with_context(&angle_spec, None, project_context.as_deref()) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("error: enqueue angle {} failed: {}", i + 1, e);
                std::process::exit(1);
            }
        };
        if let Some(p) = project {
            let _ = q.set_spec_fields(&spec_id, None, None, Some(p), None);
        }
        let _ = hooks::fire(hook_cfg, hooks::ON_DISPATCH, &serde_json::json!({
            "spec_id": spec_id,
            "title": title,
        }));
        angle_ids.push(spec_id);
    }

    // Dispatch synthesis spec
    // Queue only supports a single depends_on; use the last angle spec as the gate.
    // Angles run in parallel; synthesis waits for the last-dispatched angle.
    let synthesis_spec = make_synthesis_spec(&brief.question, angle_ids.len());
    let synthesis_title = synthesis_spec.title.clone();
    let synthesis_id = match q.enqueue_with_context(&synthesis_spec, None, project_context.as_deref()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: enqueue synthesis failed: {}", e);
            std::process::exit(1);
        }
    };
    if let Some(p) = project {
        let _ = q.set_spec_fields(&synthesis_id, None, None, Some(p), None);
    }
    if let Some(last_angle) = angle_ids.last() {
        if let Err(e) = q.set_depends_on(&synthesis_id, last_angle) {
            eprintln!("warning: could not set synthesis dependency: {}", e);
        }
    }
    let _ = hooks::fire(hook_cfg, hooks::ON_DISPATCH, &serde_json::json!({
        "spec_id": synthesis_id,
        "title": synthesis_title,
    }));

    // Dispatch deliverable spec (depends on synthesis)
    let deliverable_spec = make_deliverable_spec(&brief.question, &brief.deliverable);
    let deliverable_title = deliverable_spec.title.clone();
    let deliverable_id = match q.enqueue_with_context(&deliverable_spec, None, project_context.as_deref()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: enqueue deliverable failed: {}", e);
            std::process::exit(1);
        }
    };
    if let Some(p) = project {
        let _ = q.set_spec_fields(&deliverable_id, None, None, Some(p), None);
    }
    if let Err(e) = q.set_depends_on(&deliverable_id, &synthesis_id) {
        eprintln!("warning: could not set deliverable dependency: {}", e);
    }
    let _ = hooks::fire(hook_cfg, hooks::ON_DISPATCH, &serde_json::json!({
        "spec_id": deliverable_id,
        "title": deliverable_title,
    }));

    // Print dependency tree
    println!("Research DAG dispatched ({} angles + synthesis + deliverable):\n", angle_ids.len());
    for (i, (angle, id)) in brief.angles.iter().zip(angle_ids.iter()).enumerate() {
        println!("  [angle {}] {} — {}", i + 1, id, angle);
    }
    let last_angle_id = angle_ids.last().map(String::as_str).unwrap_or("");
    println!("  [synthesis]   {} — depends on {} (last angle)", synthesis_id, last_angle_id);
    println!("  [deliverable] {} — depends on {}", deliverable_id, synthesis_id);
    println!("\nQueue IDs:");
    for id in &angle_ids {
        println!("  {}", id);
    }
    println!("  {}", synthesis_id);
    println!("  {}", deliverable_id);
}
