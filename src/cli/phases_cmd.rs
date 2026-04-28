use crate::phases::{self, PhaseLevel, PhaseRegistry};

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
