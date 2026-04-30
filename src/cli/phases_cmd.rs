use crate::fmt::{ensure_db_dir, format_duration_ms, BOLD, CYAN, DIM, GREEN, RED, RESET};
use crate::phases::{self, PhaseLevel, PhaseRegistry};
use crate::telemetry::Telemetry;

/// Dump all phase invocations for a spec_id as a table.
pub fn cmd_phase_runs(spec_id: &str, full: bool, db_str: &str) {
    ensure_db_dir(db_str);
    let telemetry = Telemetry::new(std::path::PathBuf::from(db_str));
    let runs = telemetry.phase_runs_by_spec(spec_id);

    if runs.is_empty() {
        println!("no phase invocations found for {}", spec_id);
        return;
    }

    println!("{}{}Phase invocations for {}{}\n", BOLD, CYAN, spec_id, RESET);

    if !full {
        println!(
            "  {:<20} {:<12} {:<28} {:<9} {:<10}",
            "PHASE", "RUNTIME", "MODEL", "DURATION", "COST"
        );
        println!("  {}", "-".repeat(82));
        for r in &runs {
            let runtime = r.runtime.as_deref().unwrap_or("?");
            let model = r.model.as_deref().unwrap_or("?");
            let duration = r.duration_ms
                .map(format_duration_ms)
                .unwrap_or_else(|| "—".to_string());
            let cost = r.cost_usd
                .map(|c| format!("${:.4}", c))
                .unwrap_or_else(|| "—".to_string());
            let exit_color = match r.exit_status.as_deref() {
                Some("success") => GREEN,
                Some("timeout") | Some("nonzero") | Some("crashed") => RED,
                _ => DIM,
            };
            println!(
                "  {}{:<20}{} {:<12} {:<28} {:<9} {:<10}",
                exit_color, r.phase_name, RESET, runtime, model, duration, cost
            );
        }
        println!();
        return;
    }

    // Full view
    for (i, r) in runs.iter().enumerate() {
        if i > 0 { println!("  {}", "-".repeat(60)); }
        let phase_color = match r.exit_status.as_deref() {
            Some("success") => GREEN,
            Some(_) => RED,
            None => DIM,
        };
        println!("  {}phase:         {}{}{}", BOLD, phase_color, r.phase_name, RESET);
        println!("  inv_id:        {}{}{}", DIM, r.invocation_id, RESET);
        if let Some(ref v) = r.spec_id         { println!("  spec_id:       {}", v); }
        if let Some(ref v) = r.task_id         { println!("  task_id:       {}", v); }
        if let Some(ref v) = r.phase_level     { println!("  level:         {}", v); }
        if let Some(ref v) = r.mode            { println!("  mode:          {}", v); }
        if let Some(ref v) = r.runtime         { println!("  runtime:       {}", v); }
        if let Some(ref v) = r.model           { println!("  model:         {}", v); }
        if let Some(ref v) = r.effort          { println!("  effort:        {}", v); }
        if let Some(v) = r.thinking_enabled    { println!("  thinking:      {}", v); }
        if let Some(v) = r.thinking_budget_tokens { println!("  think_tokens:  {}", v); }
        if let Some(v) = r.extended_thinking   { println!("  ext_thinking:  {}", v); }
        if let Some(ref v) = r.prompt_template_path { println!("  prompt_tmpl:   {}", v); }
        if let Some(v) = r.prompt_length_chars  { println!("  prompt_chars:  {}", v); }
        if let Some(v) = r.prompt_length_tokens { println!("  prompt_tokens: {}", v); }
        if let Some(v) = r.timeout_secs         { println!("  timeout:       {}s", v); }
        println!("  bare_flag:     {}", r.bare_flag);
        if let Some(ref v) = r.brain_dir        { println!("  brain_dir:     {}", v); }
        if let Some(ref v) = r.api_key_env_used { println!("  api_key_env:   {}", v); }
        if let Some(ref args) = r.cli_args      { println!("  cli_args:      {:?}", args); }
        if let Some(ref v) = r.http_endpoint    { println!("  http_endpoint: {}", v); }
        if let Some(ref v) = r.started_at       { println!("  started_at:    {}", v); }
        if let Some(ref v) = r.completed_at     { println!("  completed_at:  {}", v); }
        if let Some(v) = r.duration_ms  { println!("  duration:      {}", format_duration_ms(v)); }
        if let Some(v) = r.startup_ms   { println!("  startup_ms:    {}ms", v); }
        if let Some(v) = r.inference_ms { println!("  inference_ms:  {}ms", v); }
        if let Some(v) = r.input_tokens          { println!("  in_tokens:     {}", v); }
        if let Some(v) = r.output_tokens         { println!("  out_tokens:    {}", v); }
        if let Some(v) = r.cache_read_tokens     { println!("  cache_read:    {}", v); }
        if let Some(v) = r.cache_creation_tokens { println!("  cache_create:  {}", v); }
        if let Some(v) = r.cost_usd              { println!("  cost_usd:      ${:.6}", v); }
        if let Some(ref v) = r.exit_status       { println!("  exit_status:   {}", v); }
        if let Some(ref v) = r.exit_reason       { println!("  exit_reason:   {}", v); }
        if let Some(v) = r.retry_index           { println!("  retry_index:   {}", v); }
        if let Some(ref v) = r.branch_sha        { println!("  branch_sha:    {}", v); }
        if let Some(ref v) = r.host_os           { println!("  host_os:       {}", v); }
        if let Some(ref v) = r.host_arch         { println!("  host_arch:     {}", v); }
        if let Some(ref v) = r.daemon_version    { println!("  daemon_ver:    {}", v); }
    }
    println!();
}

/// List all registered phases (core + user).
pub fn cmd_phases_list() {
    let mut registry = PhaseRegistry::new();
    let user_dir = phases::user_phases_dir();
    registry.load_user_phases(&user_dir);

    let all = registry.list();

    println!("Registered phases ({} total):\n", all.len());
    println!(
        "  {:<20} {:<8} {:<10} {:<7} {:<10} SOURCE",
        "NAME", "LEVEL", "CLAUDE", "ADDTSK", "FAILSPEC"
    );
    println!("  {}", "-".repeat(70));

    for phase in &all {
        let level_str = match phase.level {
            PhaseLevel::Spec => "spec",
            PhaseLevel::Task => "task",
        };
        let source = if registry.is_user_override(&phase.name) {
            "override"
        } else if registry.core_names().contains(&phase.name.as_str()) {
            "core"
        } else {
            "user"
        };

        println!(
            "  {:<20} {:<8} {:<10} {:<7} {:<10} {}",
            phase.name,
            level_str,
            if phase.requires_claude { "yes" } else { "no" },
            if phase.can_add_tasks { "yes" } else { "no" },
            if phase.can_fail_spec { "yes" } else { "no" },
            source,
        );
    }
}

/// Show details for a specific phase.
pub fn cmd_phases_show(name: &str) {
    let mut registry = PhaseRegistry::new();
    let user_dir = phases::user_phases_dir();
    registry.load_user_phases(&user_dir);

    match registry.get(name) {
        Some(phase) => {
            let level_str = match phase.level {
                PhaseLevel::Spec => "spec",
                PhaseLevel::Task => "task",
            };
            let source = if registry.is_user_override(name) {
                "user override of core"
            } else if registry.core_names().contains(&name) {
                "core"
            } else {
                "user"
            };

            println!("Phase: {}", phase.name);
            println!("  Level:          {}", level_str);
            println!("  Source:         {}", source);
            println!("  Description:    {}", phase.description);
            println!("  Requires Claude:{}", if phase.requires_claude { " yes" } else { " no" });
            println!("  Can add tasks:  {}", if phase.can_add_tasks { "yes" } else { "no" });
            println!("  Can fail spec:  {}", if phase.can_fail_spec { "yes" } else { "no" });
            if let Some(t) = phase.timeout_minutes {
                println!("  Timeout:        {} min", t);
            }
            if let Some(r) = phase.retry_count {
                println!("  Retry count:    {}", r);
            }
            if let Some(ref s) = phase.approve_signal {
                println!("  Approve signal: {}", s);
            }
            if let Some(ref s) = phase.reject_signal {
                println!("  Reject signal:  {}", s);
            }
            if let Some(ref a) = phase.on_approve {
                println!("  On approve:     {}", a);
            }
            if let Some(ref a) = phase.on_reject {
                println!("  On reject:      {}", a);
            }
            if let Some(ref a) = phase.on_crash {
                println!("  On crash:       {}", a);
            }
            if let Some(m) = phase.min_lines_changed {
                println!("  Min lines:      {}", m);
            }
            if !phase.prompt_template.is_empty() {
                println!("\n  Prompt template:");
                for line in phase.prompt_template.lines() {
                    println!("    {}", line);
                }
            }

            // Show which pipelines include this phase
            println!("\n  Included in default pipelines:");
            for mode in &["execute", "challenge", "discover", "generate"] {
                let p = phases::default_pipeline(mode);
                let in_spec = p.spec_phases.contains(&name.to_string());
                let in_task = p.task_phases.contains(&name.to_string());
                if in_spec || in_task {
                    let loc = if in_spec { "spec" } else { "task" };
                    println!("    {} ({})", mode, loc);
                }
            }
        }
        None => {
            eprintln!("error: unknown phase '{}'", name);
            eprintln!("\nAvailable phases:");
            let all = registry.list();
            for phase in &all {
                eprintln!("  {}", phase.name);
            }
            std::process::exit(1);
        }
    }
}
