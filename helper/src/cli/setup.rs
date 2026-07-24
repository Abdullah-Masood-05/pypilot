//! `pypilot setup` — full F1 bootstrap. Same engine as the LSP "Fix everything".

use std::path::Path;

use crate::core::setup;
use crate::pypi::MetadataSource;
use crate::settings::Settings;

pub async fn run<S: MetadataSource>(
    workspace: &Path,
    settings: &Settings,
    source: &S,
) -> crate::Result<()> {
    println!(
        "PyPilot setup — {} ({:?} mode)",
        workspace.display(),
        settings.package_manager
    );

    let summary = setup::run(workspace, settings, source).await?;

    if !summary.why.is_empty() {
        println!("{}", summary.why);
    }
    println!();

    for step in &summary.steps {
        let mark = if step.ok { "✓" } else { "✗" };
        println!("  {mark} {} — {}", step.name, truncate(&step.detail, 300));
    }
    println!();

    // If an LSP instance is watching this workspace (e.g. the command was run
    // from a Zed task), have it re-scan and toast the fresh state.
    crate::core::rescan::notify(workspace, crate::core::rescan::Kind::Setup);

    if summary.ok {
        println!(
            "Done. Environment at {} ({}).",
            summary.venv_path.display(),
            summary
                .python
                .map(|p| format!("Python {p}"))
                .unwrap_or_else(|| "system interpreter".into())
        );
        Ok(())
    } else {
        anyhow::bail!("setup did not complete — see the steps above");
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}
