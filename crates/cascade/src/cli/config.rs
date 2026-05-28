use anyhow::Result;
use cascade_config::merge;
use std::path::Path;

/// Show the resolved .cascade config for a directory.
pub fn show(path: &str) -> Result<()> {
    let dir = Path::new(path);
    if !dir.exists() {
        anyhow::bail!("directory does not exist: {path}");
    }

    let resolved = merge::resolve(dir, dir);

    println!("Resolved config for: {path}");
    println!();

    if resolved.ignores.is_empty() {
        println!("  No ignore rules.");
    } else {
        println!("  Ignore rules:");
        for rule in &resolved.ignores {
            let neg = if rule.negated { "!" } else { "" };
            let d = if rule.dir_only { "/" } else { "" };
            println!("    {}{}{}", neg, rule.pattern, d);
        }
    }

    if resolved.pins.is_empty() {
        println!("  No pin rules.");
    } else {
        println!("  Pin rules:");
        for pin in &resolved.pins {
            println!("    {}", pin.path);
        }
    }

    if let Some(cache) = &resolved.cache {
        println!("  Cache:");
        if let Some(max) = &cache.max_size {
            println!("    Max size: {max}");
        }
        if let Some(age) = &cache.max_age {
            println!("    Max age: {age}");
        }
    }

    Ok(())
}

/// Validate all .cascade files in the current tree.
pub fn validate() -> Result<()> {
    let current = std::env::current_dir()?;

    println!("Validating .cascade files under: {}", current.display());

    walk_and_validate(&current)?;

    println!("Validation complete.");
    Ok(())
}

fn walk_and_validate(dir: &Path) -> Result<()> {
    if let Some(config) = cascade_config::parse::load_dir(dir) {
        for rule in &config.ignore {
            if rule.pattern.is_empty() {
                anyhow::bail!("empty ignore pattern in {}", dir.join(".cascade").display());
            }
        }
        println!("  OK: {}", dir.display());
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type()?.is_dir() {
                walk_and_validate(&entry.path())?;
            }
        }
    }

    Ok(())
}
