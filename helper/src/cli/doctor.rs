//! `pypilot doctor` — read-only probe + compatibility report. Executes nothing.

use std::path::Path;

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

    println!("PyPilot doctor — {}", workspace.display());
    println!("  package manager : {:?}", settings.package_manager);
    println!();

    // --- Environment probes ---
    println!("Environment");
    match &a.probes.uv {
        Some(uv) => println!(
            "  uv              : present (v{}, {})",
            uv.version,
            if uv.managed { "managed" } else { "on PATH" }
        ),
        None => println!("  uv              : not detected"),
    }
    match &a.probes.venv {
        Some(v) => println!(
            "  virtualenv      : {} (Python {})",
            v.path.display(),
            v.python
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into())
        ),
        None => println!("  virtualenv      : none"),
    }
    if a.probes.interpreters.is_empty() {
        println!("  interpreters    : none found");
    } else {
        let list = a
            .probes
            .interpreters
            .iter()
            .map(|i| format!("{} ({})", i.version, i.command))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  interpreters    : {list}");
    }
    println!();

    // --- Project ---
    println!("Project");
    if a.project.sources.is_empty() {
        println!("  (no Python project files detected)");
    } else {
        let files = a
            .project
            .sources
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(", ");
        println!("  files           : {files}");
        if let Some(rp) = &a.project.declared_requires_python {
            println!("  requires-python : {rp}");
        }
        println!("  dependencies    : {}", a.project.packages.join(", "));
    }
    println!();

    // --- Compatibility ---
    if let Some(compat) = &a.compat {
        println!("Compatibility");
        println!(
            "  supported Python: {}",
            compat.intersection.to_range_string()
        );
        if let Some(t) = a.target_python {
            println!("  recommended     : Python {t}");
        }
        for p in &compat.per_package {
            println!(
                "    - {:<20} {}{}",
                p.name,
                p.supported.to_range_string(),
                if p.sdist_only { "  (sdist only)" } else { "" }
            );
        }
        for (name, err) in &compat.unresolved {
            println!("    - {name:<20} unresolved ({err})");
        }
        println!();
    }

    // --- Findings ---
    if a.findings.is_empty() {
        println!("✓ Everything looks good.");
    } else {
        println!("Findings");
        for f in &a.findings {
            let tag = match f.severity {
                Severity::Error => "ERROR",
                Severity::Warning => "WARN ",
                Severity::Info => "INFO ",
            };
            println!("  [{tag}] {}", f.title);
            println!("          {}", f.detail);
        }
    }

    // If an LSP instance is watching this workspace (e.g. the command was run
    // from a Zed task), have it surface the same result as a toast.
    crate::core::rescan::notify(workspace, crate::core::rescan::Kind::Doctor);

    Ok(())
}
