//! BOI v2 binary entry point.
//!
//! `main` does only argument parsing and dispatches into `cli::run`. Every
//! command's logic lives in `src/cli/`; see `src/cli/mod.rs`. A failed command
//! is rendered by `cli::report_error` and the process exits non-zero — the
//! exit-code policy lives here so `cli::run` stays testable (it never calls
//! `std::process::exit`).

use std::process::ExitCode;

use clap::Parser;

use boi::cli::{self, Cli};

fn main() -> ExitCode {
    // Bootstrap provider secrets BEFORE tokio spawns its thread pool.
    // std::env::set_var is unsound in a multi-threaded context; this is the
    // only safe call site. See runtime::secrets for details.
    if let Ok(dir) = boi::cli::paths::secrets_dir() {
        if let Err(e) = boi::runtime::secrets::bootstrap_provider_env(&dir) {
            eprintln!("boi: secrets bootstrap failed: {e}");
            return ExitCode::FAILURE;
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            // A failure to build the tokio runtime is fatal and must be loud
            // (S6) — never a swallowed panic.
            eprintln!("boi: could not start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(async_main())
}

async fn async_main() -> ExitCode {
    let cli = Cli::parse();
    match cli::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}", cli::report_error(&err));
            ExitCode::FAILURE
        }
    }
}
