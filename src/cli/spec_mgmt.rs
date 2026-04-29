use crate::cli::status::render_single_spec;
use crate::fmt::ensure_db_dir;
use crate::queue;

pub enum SpecActionData {
    Show,
    ShowYaml,
    Add {
        title: String,
        spec: Option<String>,
        verify: Option<String>,
        depends: Vec<String>,
    },
    Skip {
        task_id: String,
    },
    Block {
        task_id: String,
        on: String,
    },
}

fn format_spec_yaml(spec: &queue::SpecRecord, tasks: &[queue::FullTaskRecord]) -> String {
    let mut out = String::new();
    out.push_str(&format!("title: \"{}\"\n", spec.title.replace('"', "\\\"")));
    out.push_str(&format!("mode: {}\n", spec.mode));
    if let Some(ws) = &spec.workspace {
        out.push_str(&format!("workspace: {}\n", ws));
    }
    if let Some(ctx) = &spec.context {
        out.push_str("context: |\n");
        for line in ctx.lines() {
            out.push_str(&format!("  {}\n", line));
        }
    }
    if !tasks.is_empty() {
        out.push_str("tasks:\n");
        for task in tasks {
            out.push_str(&format!("  - id: {}\n", task.id));
            out.push_str(&format!("    title: \"{}\"\n", task.title.replace('"', "\\\"")));
            out.push_str(&format!("    status: {}\n", task.status));
            let deps: Vec<String> =
                serde_json::from_str(&task.depends).unwrap_or_default();
            if !deps.is_empty() {
                out.push_str("    depends:\n");
                for dep in &deps {
                    out.push_str(&format!("      - {}\n", dep));
                }
            }
            if let Some(spec_content) = &task.spec_content {
                out.push_str("    spec: |\n");
                for line in spec_content.lines() {
                    out.push_str(&format!("      {}\n", line));
                }
            }
            if let Some(verify) = &task.verify_content {
                if verify.contains('\n') {
                    out.push_str("    verify: |\n");
                    for line in verify.lines() {
                        out.push_str(&format!("      {}\n", line));
                    }
                } else {
                    out.push_str(&format!("    verify: \"{}\"\n", verify.replace('"', "\\\"")));
                }
            }
        }
    }
    out
}

pub fn cmd_spec(queue_id: &str, action: SpecActionData, db_str: &str) {
    ensure_db_dir(db_str);
    let q = match queue::Queue::open(db_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("error: cannot open queue: {}", e);
            std::process::exit(1);
        }
    };

    match action {
        SpecActionData::Show => {
            match q.status(queue_id) {
                Ok(Some(_)) => {
                    print!("{}", render_single_spec(&q, queue_id));
                }
                Ok(None) => {
                    eprintln!("error: spec '{}' not found", queue_id);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        SpecActionData::ShowYaml => {
            let spec_status = match q.status(queue_id) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    eprintln!("error: spec '{}' not found", queue_id);
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            };
            let tasks = match q.get_tasks_full(queue_id) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            };
            print!("{}", format_spec_yaml(&spec_status.spec, &tasks));
        }
        SpecActionData::Add {
            title,
            spec,
            verify,
            depends,
        } => {
            match q.add_task(
                queue_id,
                "",
                &title,
                spec.as_deref(),
                verify.as_deref(),
                &depends,
            ) {
                Ok(task_id) => println!("added {} to {}", task_id, queue_id),
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        SpecActionData::Skip { task_id } => match q.skip_task(queue_id, &task_id) {
            Ok(()) => println!("skipped {} in {}", task_id, queue_id),
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        },
        SpecActionData::Block { task_id, on } => {
            match q.block_task(queue_id, &task_id, &on) {
                Ok(()) => println!("blocked {} on {} in {}", task_id, on, queue_id),
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}
