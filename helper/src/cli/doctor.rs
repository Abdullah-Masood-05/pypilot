//! `pypilot doctor` — read-only probe + compatibility report. Executes nothing.

use std::path::Path;

use colored::Colorize;

use crate::core::solver;
use crate::core::Severity;
use crate::pypi::MetadataSource;
use crate::settings::Settings;

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> crate::Result<()> {
    let a = solver::assess(workspace, settings, source).await;

    println!(
        "{} — {}",
        "PyPilot doctor".bold().cyan(),
        workspace.display()
    );
    println!(
        "  {:<16}: {}",
        "package manager",
        format!("{:?}", settings.package_manager).bold()
    );
    println!();

    // --- Environment probes ---
    println!(
        "{}",
        "── Environment ──────────────────────────────────────────"
            .bold()
            .cyan()
    );
    match &a.probes.uv {
        Some(uv) => println!(
            "  {:<16}: {} ({}, {})",
            "uv",
            "present".green().bold(),
            format!("v{}", uv.version).cyan().bold(),
            if uv.managed { "managed" } else { "on PATH" }
        ),
        None => println!("  {:<16}: {}", "uv", "not detected".yellow()),
    }
    match &a.probes.venv {
        Some(v) => println!(
            "  {:<16}: {} ({})",
            "virtualenv",
            v.path.display(),
            v.python
                .map(|p| format!("Python {p}").green().bold().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ),
        None => println!("  {:<16}: {}", "virtualenv", "none".yellow()),
    }
    if a.probes.interpreters.is_empty() {
        println!("  {:<16}: {}", "interpreters", "none found".yellow());
    } else {
        let list = a
            .probes
            .interpreters
            .iter()
            .map(|i| format!("{} ({})", i.version.to_string().cyan().bold(), i.command))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  {:<16}: {list}", "interpreters");
    }
    println!();

    // --- Project ---
    println!(
        "{}",
        "── Project ──────────────────────────────────────────────"
            .bold()
            .cyan()
    );
    if a.project.sources.is_empty() {
        println!("  (no Python project files detected)");
    } else {
        let files = a
            .project
            .sources
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|f| f.to_string_lossy().bold().to_string())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("  {:<16}: {files}", "files");
        if let Some(rp) = &a.project.declared_requires_python {
            println!(
                "  {:<16}: {}",
                "requires-python",
                rp.to_string().green().bold()
            );
        }
        let deps = a
            .project
            .packages
            .iter()
            .map(|r| r.to_string().bold().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!("  {:<16}: {deps}", "dependencies");
    }
    println!();

    // --- Compatibility ---
    if let Some(compat) = &a.compat {
        println!(
            "{}",
            "── Compatibility ────────────────────────────────────────"
                .bold()
                .cyan()
        );
        println!(
            "  {:<16}: {}",
            "supported Python",
            compat.intersection.to_range_string().green().bold()
        );
        if let Some(t) = a.target_python {
            println!(
                "  {:<16}: {}",
                "recommended",
                format!("Python {t}").cyan().bold()
            );
        }
        for p in &compat.per_package {
            let sdist_note = if p.sdist_only {
                " (sdist only)".yellow().to_string()
            } else {
                "".to_string()
            };
            println!(
                "    {} {:<20} {}{}",
                "•".cyan(),
                p.name.bold(),
                p.supported.to_range_string().green(),
                sdist_note
            );
        }
        for (name, err) in &compat.unresolved {
            println!(
                "    {} {:<20} {}",
                "•".cyan(),
                name.bold(),
                format!("unresolved ({err})").red().bold()
            );
        }
        println!();
    }

    // --- Findings ---
    println!(
        "{}",
        "── Findings ─────────────────────────────────────────────"
            .bold()
            .cyan()
    );
    if a.findings.is_empty() {
        println!("  {} Everything looks good.", "✓".green().bold());
    } else {
        for f in &a.findings {
            let tag = match f.severity {
                Severity::Error => "[ERROR]".red().bold(),
                Severity::Warning => "[WARN ]".yellow().bold(),
                Severity::Info => "[INFO ]".cyan().bold(),
            };
            println!("  {tag} {}", f.title.bold());
            println!("          {}", f.detail);
        }
    }

    // If an LSP instance is watching this workspace (e.g. the command was run
    // from a Zed task), have it surface the same result as a toast.
    crate::core::rescan::notify(workspace, crate::core::rescan::Kind::Doctor);

    Ok(())
}
