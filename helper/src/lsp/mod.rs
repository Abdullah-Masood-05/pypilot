//! Minimal LSP server (F5 onboarding flow).
//!
//! On workspace open it runs the fast probe + F3 analysis, and — only if
//! something is wrong and notifications aren't disabled — raises a single
//! consolidated `window/showMessageRequest` toast:
//!
//!   [Fix everything] [Show details] [Ignore] [Never for this project]
//!
//! The button click routes to the exact same `core::setup` code the CLI uses.
//! `workspace/executeCommand` exposes the same actions for task/code-action use.
//!
//! **CLI bridge:** a task-spawned `pypilot doctor|setup` drops a rescan request
//! (see [`crate::core::rescan`]); a background watcher here polls it (one file
//! `stat` per second) and re-raises the toast with fresh results. Explicit
//! requests bypass the silence gates — the user asked, so even "all good" gets
//! a confirmation toast.
//!
//! Buffer diagnostics (F4) attach here later via `did_open`/`did_save`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::core::{rescan, setup, solver, Assessment, FixKind, Severity};
use crate::pypi::PyPiClient;
use crate::settings::{Notifications, Settings};

// Command IDs surfaced via executeCommand.
const CMD_FIX: &str = "pypilot.fixEverything";
const CMD_DETAILS: &str = "pypilot.showDetails";
const CMD_IGNORE: &str = "pypilot.ignore";
const CMD_NEVER: &str = "pypilot.neverForProject";

// Toast button labels.
const BTN_FIX: &str = "Fix everything";
const BTN_DETAILS: &str = "Show details";
const BTN_IGNORE: &str = "Ignore";
const BTN_NEVER: &str = "Never for this project";

struct Backend {
    client: Client,
    root: Mutex<Option<PathBuf>>,
    source: PyPiClient,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        *self.root.lock().await = workspace_root(&params);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "PyPilot".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                // We don't need document sync yet (F4). Declare only what we use.
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        CMD_FIX.into(),
                        CMD_DETAILS.into(),
                        CMD_IGNORE.into(),
                        CMD_NEVER.into(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let Some(root) = self.root.lock().await.clone() else {
            return;
        };

        // CLI → toast bridge: watch for rescan requests from task-spawned runs.
        tokio::spawn(watch_rescan_requests(self.client.clone(), root.clone()));

        // The unsolicited onboarding scan (respects the silence gates).
        scan_and_notify(&self.client, &self.source, &root, false).await;
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> RpcResult<Option<serde_json::Value>> {
        let root = self.root.lock().await.clone();
        let Some(root) = root else { return Ok(None) };

        match params.command.as_str() {
            CMD_FIX => run_fix(&self.client, &self.source, &root).await,
            CMD_DETAILS => show_details(&self.client, &self.source, &root).await,
            CMD_NEVER => mark_never(&self.client, &root).await,
            CMD_IGNORE => {} // session-only dismissal; nothing persisted.
            other => {
                self.client
                    .log_message(MessageType::WARNING, format!("unknown command `{other}`"))
                    .await;
            }
        }
        Ok(None)
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }
}

/// Poll the workspace's rescan-request file; on change, run an explicit scan.
/// One `stat` per second, no allocation — negligible even on battery.
async fn watch_rescan_requests(client: Client, root: PathBuf) {
    // Ignore any request that predates this server instance.
    let mut last_seen = rescan::request_mtime(&root);
    let source = PyPiClient::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        let current = rescan::request_mtime(&root);
        if current.is_some() && current != last_seen {
            last_seen = current;
            scan_and_notify(&client, &source, &root, true).await;
        }
    }
}

/// The one scan → toast path shared by onboarding, the CLI bridge, and (later)
/// any re-scan trigger. `explicit == true` means the user asked for this scan
/// (task/CLI), so silence gates are bypassed and success is confirmed too.
async fn scan_and_notify(client: &Client, source: &PyPiClient, root: &Path, explicit: bool) {
    let settings = Settings::load(root);

    if !explicit
        && (settings.notifications == Notifications::Off
            || settings.dismissed
            || !settings.auto_check_on_open)
    {
        return;
    }

    let assessment = solver::assess(root, &settings, source).await;

    if !assessment.project.is_python_project() {
        if explicit {
            client
                .show_message(
                    MessageType::INFO,
                    "PyPilot: no Python project files found in this workspace.",
                )
                .await;
        }
        return;
    }

    // Everything fine → silence for unsolicited scans, confirmation for explicit.
    if assessment.all_good() {
        if explicit {
            client
                .show_message(MessageType::INFO, all_good_summary(&assessment))
                .await;
        }
        return;
    }

    let message = toast_summary(&assessment);
    let actions = vec![
        action(BTN_FIX),
        action(BTN_DETAILS),
        action(BTN_IGNORE),
        action(BTN_NEVER),
    ];

    let severity = match assessment.worst_severity() {
        Some(Severity::Error) => MessageType::ERROR,
        _ => MessageType::WARNING,
    };

    let chosen = client
        .show_message_request(severity, message, Some(actions))
        .await
        .ok()
        .flatten();

    match chosen.as_ref().map(|a| a.title.as_str()) {
        Some(BTN_FIX) => run_fix(client, source, root).await,
        Some(BTN_DETAILS) => show_details(client, source, root).await,
        Some(BTN_NEVER) => mark_never(client, root).await,
        _ => {} // Ignore / dismissed.
    }
}

/// Run the full F1 bootstrap and toast the outcome. Same engine as `pypilot setup`.
async fn run_fix(client: &Client, source: &PyPiClient, root: &Path) {
    let settings = Settings::load(root);

    client
        .show_message(MessageType::INFO, "PyPilot: setting up the environment…")
        .await;

    match setup::run(root, &settings, source).await {
        Ok(summary) if summary.ok => {
            let py = summary
                .python
                .map(|p| format!("Python {p}"))
                .unwrap_or_else(|| "system interpreter".into());
            client
                .show_message(
                    MessageType::INFO,
                    format!(
                        "PyPilot: done. {} at {}. {}",
                        py,
                        summary.venv_path.display(),
                        summary.why
                    ),
                )
                .await;
        }
        Ok(summary) => {
            let last = summary
                .steps
                .iter()
                .rev()
                .find(|s| !s.ok)
                .map(|s| format!("{}: {}", s.name, s.detail))
                .unwrap_or_else(|| "setup failed".into());
            client
                .show_message(MessageType::ERROR, format!("PyPilot: {last}"))
                .await;
        }
        Err(e) => {
            client
                .show_message(MessageType::ERROR, format!("PyPilot: setup error — {e}"))
                .await;
        }
    }
}

/// Generate the markdown report, write it to a temp file, and open it in the
/// editor; falls back to a plain message if the client can't show documents.
async fn show_details(client: &Client, source: &PyPiClient, root: &Path) {
    let settings = Settings::load(root);
    let assessment = solver::assess(root, &settings, source).await;
    let report = details_markdown(&assessment);

    let path = std::env::temp_dir().join("pypilot-report.md");
    if std::fs::write(&path, &report).is_ok() {
        if let Ok(uri) = Url::from_file_path(&path) {
            let opened = client
                .send_request::<request::ShowDocument>(ShowDocumentParams {
                    uri,
                    external: Some(false),
                    take_focus: Some(true),
                    selection: None,
                })
                .await;
            if opened.is_ok() {
                return;
            }
        }
    }
    client.show_message(MessageType::INFO, report).await;
}

/// Persist "Never for this project" (F5 dismissal).
async fn mark_never(client: &Client, root: &Path) {
    if let Err(e) = Settings::mark_dismissed(root) {
        client
            .log_message(
                MessageType::WARNING,
                format!("could not persist dismissal: {e}"),
            )
            .await;
    }
}

fn action(title: &str) -> MessageActionItem {
    MessageActionItem {
        title: title.to_string(),
        properties: Default::default(),
    }
}

/// Positive confirmation for explicit scans, still stating the why.
fn all_good_summary(a: &Assessment) -> String {
    let venv = a
        .probes
        .venv
        .as_ref()
        .and_then(|v| v.python)
        .map(|p| format!("Python {p} venv"))
        .unwrap_or_else(|| "environment".into());
    match &a.compat {
        Some(compat) => format!(
            "PyPilot: everything looks good — {} is compatible (dependencies support {}).",
            venv,
            compat.intersection.to_range_string()
        ),
        None => format!("PyPilot: everything looks good — {} in place.", venv),
    }
}

/// One-line consolidated toast body, always stating the *why*.
fn toast_summary(a: &Assessment) -> String {
    // Prefer the most severe finding's detail — it already names the package.
    let primary = a
        .findings
        .iter()
        .find(|f| f.severity == Severity::Error)
        .or_else(|| a.findings.iter().find(|f| f.severity == Severity::Warning));

    match primary {
        Some(f) => format!("PyPilot: {}. {}", f.title, f.detail),
        None => "PyPilot: this project needs attention.".to_string(),
    }
}

/// Full markdown report for "Show details".
fn details_markdown(a: &Assessment) -> String {
    let mut s = String::new();
    s.push_str("# PyPilot report\n\n");

    s.push_str("## Environment\n");
    s.push_str(&format!(
        "- uv: {}\n",
        a.probes
            .uv
            .as_ref()
            .map(|u| format!("v{}", u.version))
            .unwrap_or_else(|| "not detected".into())
    ));
    s.push_str(&format!(
        "- virtualenv: {}\n",
        a.probes
            .venv
            .as_ref()
            .map(|v| v.path.display().to_string())
            .unwrap_or_else(|| "none".into())
    ));
    s.push_str(&format!(
        "- interpreters: {}\n\n",
        if a.probes.interpreters.is_empty() {
            "none".to_string()
        } else {
            a.probes
                .interpreters
                .iter()
                .map(|i| i.version.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));

    if let Some(compat) = &a.compat {
        s.push_str("## Compatibility\n");
        s.push_str(&format!(
            "- Supported Python across dependencies: **{}**\n",
            compat.intersection.to_range_string()
        ));
        if let Some(t) = a.target_python {
            s.push_str(&format!("- Recommended interpreter: **Python {t}**\n"));
        }
        s.push_str("\n| Package | Supported Python |\n|---|---|\n");
        for p in &compat.per_package {
            s.push_str(&format!(
                "| {} | {} |\n",
                p.name,
                p.supported.to_range_string()
            ));
        }
        s.push('\n');
    }

    s.push_str("## Findings\n");
    if a.findings.is_empty() {
        s.push_str("Everything looks good.\n");
    } else {
        for f in &a.findings {
            let sev = match f.severity {
                Severity::Error => "❌",
                Severity::Warning => "⚠️",
                Severity::Info => "ℹ️",
            };
            s.push_str(&format!("### {sev} {}\n{}\n", f.title, f.detail));
            if let FixKind::RecreateWithPython(v) = &f.fix {
                s.push_str(&format!(
                    "\n_Fix: recreate the environment with Python {v}._\n"
                ));
            } else if f.fix == FixKind::SetupEnvironment {
                s.push_str("\n_Fix: run \"Fix everything\" / `pypilot setup`._\n");
            }
            s.push('\n');
        }
    }
    s
}

/// Resolve the workspace root from init params (folders first, then root_uri).
fn workspace_root(params: &InitializeParams) -> Option<PathBuf> {
    if let Some(folders) = &params.workspace_folders {
        if let Some(first) = folders.first() {
            if let Ok(p) = first.uri.to_file_path() {
                return Some(p);
            }
        }
    }
    #[allow(deprecated)]
    if let Some(uri) = &params.root_uri {
        if let Ok(p) = uri.to_file_path() {
            return Some(p);
        }
    }
    None
}

/// Entry point for `pypilot lsp`. Owns its multi-threaded runtime.
pub fn run_stdio() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build LSP runtime");

    rt.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::new(|client| Backend {
            client,
            root: Mutex::new(None),
            source: PyPiClient::new(),
        });
        Server::new(stdin, stdout, socket).serve(service).await;
    });
}
