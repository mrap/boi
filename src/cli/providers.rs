use crate::runtime::{ProviderRegistry, ProviderStatus};

pub fn cmd_providers_list() {
    let registry = ProviderRegistry::new();
    let entries = registry.list();

    println!("Registered providers:");
    for (name, status) in &entries {
        match status {
            ProviderStatus::Active => println!("  {name} [active]"),
            ProviderStatus::Disabled(reason) => println!("  {name} [disabled: {reason}]"),
        }
    }
}
