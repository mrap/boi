//! `boi spec <queue_id> tail <task_id> [--follow]` — Phase 7 worker
//! stdout tail.
//!
//! Resolves the on-disk log written by the host-side `WorkerEvent`
//! tee at `~/.boi/logs/<spec_id>/<task_id>.log`. In the distributed
//! mode the CLI consults etcd to find the claimant node and opens an
//! internal `Tail` RPC against it; in the single-node case the log
//! lives on the local filesystem and we tail it directly.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

fn log_path(queue_id: &str, task_id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".boi")
        .join("logs")
        .join(queue_id)
        .join(format!("{task_id}.log"))
}

pub fn cmd_tail(
    queue_id: &str,
    task_id: &str,
    follow: bool,
    since_bytes: u64,
    max_bytes: u64,
    print_offset: bool,
) {
    let path = log_path(queue_id, task_id);

    let mut file = match std::fs::OpenOptions::new().read(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: cannot open {}: {}", path.display(), e);
            std::process::exit(1);
        }
    };

    if let Err(e) = file.seek(SeekFrom::Start(since_bytes)) {
        eprintln!("error: seek: {}", e);
        std::process::exit(1);
    }

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut buf = [0u8; 8192];
    let mut emitted: u64 = 0;
    let mut offset: u64 = since_bytes;

    loop {
        let cap = if max_bytes > 0 {
            (max_bytes - emitted).min(buf.len() as u64) as usize
        } else {
            buf.len()
        };
        if cap == 0 {
            break;
        }
        match file.read(&mut buf[..cap]) {
            Ok(0) => {
                if follow {
                    sleep(Duration::from_millis(100));
                    continue;
                }
                break;
            }
            Ok(n) => {
                let _ = out.write_all(&buf[..n]);
                emitted += n as u64;
                offset += n as u64;
                if max_bytes > 0 && emitted >= max_bytes {
                    break;
                }
            }
            Err(e) => {
                eprintln!("error: read: {}", e);
                std::process::exit(1);
            }
        }
    }

    let _ = out.flush();
    if print_offset {
        eprintln!("offset={offset}");
    }
}
