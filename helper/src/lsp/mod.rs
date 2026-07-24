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
//! Buffer diagnostics (F4) attach here later via `did_open`/`did_save`; the
//! scaffolding (client, state, shared source) is already in place.

use std::path::PathBuf;

use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::core::{setup, solver, Assessment, FixKind, Severity};
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

struct State {
    root: Option<PathBuf>,
    settings: Settings,
}

struct Backend {
    client: Client,
    state: Mutex<State>,
    source: PyPiClient,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        let root = workspace_root(&params);
        let settings = root.as_ref().map(|r| Settings::load(r)).unwrap_or_default();

        {
            let mut state = self.state.lock().await;
            state.root = root;
            state.settings = settings;
        }

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
        // Run the onboarding scan off the init path.
        self.onboarding_scan().await;
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> RpcResult<Option<serde_json::Value>> {
        match params.command.as_str() {
            CMD_FIX => self.run_fix().await,
            CMD_DETAILS => self.show_details().await,
            CMD_NEVER => self.mark_never().await,
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

impl Backend {
    /// F5 core: assess, and toast only if something's wrong and it's allowed.
    async fn onboarding_scan(&self) {
        let (root, settings) = {
            let state = self.state.lock().await;
            (state.root.clone(), state.settings.clone())
        };
        let Some(root) = root else { return };

        // Respect F7: off / dismissed / auto-check disabled → silence.
        if settings.notifications == Notifications::Off
            || settings.dismissed
            || !settings.auto_check_on_open
        {
            return;
        }

        let assessment = solver::assess(&root, &settings, &self.source).await;

        // Not a Python project, or everything fine → stay silent (no toast spam).
        if !assessment.project.is_python_project() || assessment.all_good() {
            return;
        }
        // "problems-only" only suppresses info-level noise; we already gate on
        // all_good() (which ignores Info), so any surviving toast is a problem.

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

        let chosen = self
            .client
            .show_message_request(severity, message, Some(actions))
            .await
            .ok()
            .flatten();

        match chosen.as_ref().map(|a| a.title.as_str()) {
            Some(BTN_FIX) => self.run_fix().await,
            Some(BTN_DETAILS) => self.show_details().await,
            Some(BTN_NEVER) => self.mark_never().await,
            _ => {} // Ignore / dismissed.
        }
    }

    async fn run_fix(&self) {
        let (root, settings) = {
            let state = self.state.lock().await;
            (state.root.clone(), state.settings.clone())
        };
        let Some(root) = root else { return };

        self.client
            .show_message(MessageType::INFO, "PyPilot: setting up the environment…")
            .await;

        match setup::run(&root, &settings, &self.source).await {
            Ok(summary) if summary.ok => {
                let py = summary
                    .python
                    .map(|p| format!("Python {p}"))
                    .unwrap_or_else(|| "system interpreter".into());
                self.client
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
                self.client
                    .show_message(MessageType::ERROR, format!("PyPilot: {last}"))
                    .await;
            }
            Err(e) => {
                self.client
                    .show_message(MessageType::ERROR, format!("PyPilot: setup error — {e}"))
                    .await;
            }
        }
    }

    async fn show_details(&self) {
        let (root, settings) = {
            let state = self.state.lock().await;
            (state.root.clone(), state.settings.clone())
        };
        let Some(root) = root else { return };

        let assessment = solver::assess(&root, &settings, &self.source).await;
        let report = details_markdown(&assessment);

        // Write a temp report and try to open it; fall back to a message.
        let path = std::env::temp_dir().join("pypilot-report.md");
        if std::fs::write(&path, &report).is_ok() {
            if let Ok(uri) = Url::from_file_path(&path) {
                let opened = self
                    .client
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
        // Fallback: dump into the message surface.
        self.client.show_message(MessageType::INFO, report).await;
    }

    async fn mark_never(&self) {
        let root = { self.state.lock().await.root.clone() };
        let Some(root) = root else { return };
        if let Err(e) = Settings::mark_dismissed(&root) {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("could not persist dismissal: {e}"),
                )
                .await;
        }
        // Reflect it in live state too.
        self.state.lock().await.settings.dismissed = true;
    }
}

fn action(title: &str) -> MessageActionItem {
    MessageActionItem {
        title: title.to_string(),
        properties: Default::default(),
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
            state: Mutex::new(State {
                root: None,
                settings: Settings::default(),
            }),
            source: PyPiClient::new(),
        });
        Server::new(stdin, stdout, socket).serve(service).await;
    });
}
