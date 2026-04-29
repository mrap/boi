use crate::cli::status::render_single_spec;
use crate::fmt::ensure_db_dir;
use crate::queue;

pub enum SpecActionData {
    Show,
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
