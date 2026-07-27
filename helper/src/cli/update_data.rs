//! `pypilot update-data`: force-refresh the bundled driver/framework/import
//! tables from the project repo, bypassing the TTL.
//!
//! Global, not project-scoped: the datasets are not per-workspace, so unlike
//! every other CLI mode this one takes no `--path`.

use crate::matrix::refresh;

pub async fn run() -> crate::Result<()> {
    println!("Refreshing PyPilot's bundled data...");
    let results = refresh::refresh_all().await;

    let mut any_failed = false;
    for (name, outcome) in results {
        match outcome {
            Ok(()) => println!("  ok    {name}"),
            Err(e) => {
                any_failed = true;
                println!("  failed {name}: {e}");
            }
        }
    }

    if any_failed {
        println!("\nOne or more datasets could not be refreshed; PyPilot keeps using the bundled snapshot for those.");
    } else {
        println!("\nAll datasets refreshed.");
    }
    Ok(())
}
