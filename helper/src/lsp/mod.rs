//! The LSP server: workspace onboarding (F5) and the live import guardian (F4).
//!
//! Two surfaces share one engine.
//!
//! On workspace open, a probe and compatibility pass raises at most one
//! notification with `[Fix everything] [Show details] [Ignore] [Never for this
//! project]`. If the environment is already correct it stays quiet.
//!
//! On Python buffers, imports that cannot resolve become diagnostics with quick
//! fixes. Analysis runs on open and save immediately, and one second after the
//! user stops typing. Each edit supersedes the pending run for that buffer, so a
//! burst of keystrokes costs one analysis rather than one per character.
//!
//! A CLI run started from a Zed task leaves a request file behind (see
//! [`crate::core::rescan`]); a watcher polls it and re-raises the notification,
//! which is how a terminal command surfaces in the editor.

mod diagnostics;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::core::platform::Platform;
use crate::core::{
    guardian, install, installed, rescan, setup, solver, Assessment, FixKind, Severity,
};
use crate::pypi::pyversion::PyVersion;
use crate::pypi::PyPiClient;
use crate::settings::{Notifications, Settings};

use diagnostics::{CMD_INSTALL, CMD_RECREATE};

// F5 commands.
const CMD_FIX: &str = "pypilot.fixEverything";
const CMD_DETAILS: &str = "pypilot.showDetails";
const CMD_IGNORE: &str = "pypilot.ignore";
const CMD_NEVER: &str = "pypilot.neverForProject";

// Notification button labels.
const BTN_FIX: &str = "Fix everything";
const BTN_DETAILS: &str = "Show details";
const BTN_IGNORE: &str = "Ignore";
const BTN_NEVER: &str = "Never for this project";

/// How long the buffer must be idle before the guardian runs.
const IDLE_DEBOUNCE: Duration = Duration::from_secs(1);

/// State shared between request handlers and spawned background tasks.
struct Shared {
    client: Client,
    root: Mutex<Option<PathBuf>>,
    /// Open buffer contents, by URI.
    documents: Mutex<HashMap<Url, String>>,
    /// Edit counter per buffer. A debounced run compares against this and exits
    /// if the buffer moved on, which is the whole cancellation mechanism.
    revisions: Mutex<HashMap<Url, u64>>,
    source: PyPiClient,
}

struct Backend {
    shared: Arc<Shared>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        *self.shared.root.lock().await = workspace_root(&params);

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "PyPilot".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        CMD_FIX.into(),
                        CMD_DETAILS.into(),
                        CMD_IGNORE.into(),
                        CMD_NEVER.into(),
                        CMD_INSTALL.into(),
                        CMD_RECREATE.into(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let Some(root) = self.shared.root.lock().await.clone() else {
            return;
        };
        tokio::spawn(watch_rescan_requests(self.shared.clone(), root.clone()));
        scan_and_notify(&self.shared.client, &self.shared.source, &root, false).await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        self.shared.documents.lock().await.insert(uri.clone(), text);
        run_guardian(self.shared.clone(), uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        // FULL sync: the last content change is the whole document.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        self.shared
            .documents
            .lock()
            .await
            .insert(uri.clone(), change.text);

        let revision = {
            let mut revisions = self.shared.revisions.lock().await;
            let counter = revisions.entry(uri.clone()).or_insert(0);
            *counter += 1;
            *counter
        };

        let shared = self.shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_DEBOUNCE).await;
            // Superseded by a newer edit: that run will handle it.
            if shared.revisions.lock().await.get(&uri).copied() != Some(revision) {
                return;
            }
            run_guardian(shared, uri).await;
        });
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // A save is an explicit checkpoint; do not make the user wait a second.
        run_guardian(self.shared.clone(), params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.shared.documents.lock().await.remove(&uri);
        self.shared.revisions.lock().await.remove(&uri);
        // Clear our squiggles so they do not outlive the buffer.
        self.shared
            .client
            .publish_diagnostics(uri, vec![], None)
            .await;
    }

    async fn code_action(&self, params: CodeActionParams) -> RpcResult<Option<CodeActionResponse>> {
        let actions = diagnostics::code_actions(&params.context.diagnostics);
        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> RpcResult<Option<serde_json::Value>> {
        let root = self.shared.root.lock().await.clone();
        let Some(root) = root else { return Ok(None) };
        let client = &self.shared.client;

        match params.command.as_str() {
            CMD_FIX => run_fix(client, &self.shared.source, &root).await,
            CMD_DETAILS => show_details(client, &self.shared.source, &root).await,
            CMD_NEVER => mark_never(client, &root).await,
            CMD_IGNORE => {}

            CMD_INSTALL => {
                let Some(package) = string_arg(&params.arguments, 0) else {
                    return Ok(None);
                };
                run_install(self.shared.clone(), &root, &package).await;
            }

            CMD_RECREATE => {
                let (Some(python), Some(package)) = (
                    string_arg(&params.arguments, 0),
                    string_arg(&params.arguments, 1),
                ) else {
                    return Ok(None);
                };
                run_recreate(self.shared.clone(), &root, &python, &package).await;
            }

            other => {
                client
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

// --- F4: the guardian pass ---------------------------------------------------

/// Analyze one buffer and publish its diagnostics.
async fn run_guardian(shared: Arc<Shared>, uri: Url) {
    let Ok(path) = uri.to_file_path() else { return };
    if path.extension().is_some_and(|e| e != "py") {
        return;
    }

    let Some(root) = shared.root.lock().await.clone() else {
        return;
    };
    let Some(text) = shared.documents.lock().await.get(&uri).cloned() else {
        return;
    };

    let settings = Settings::load(&root);
    if settings.notifications == Notifications::Off {
        return;
    }

    // Everything here is local except the PyPI lookups, which are cached and
    // only happen for imports that are neither stdlib, local, nor installed.
    let venv = root.join(".venv");
    let installed = installed::scan(&venv);
    let venv_python = venv_python(&venv);

    let mut findings = guardian::analyze_buffer(
        &text,
        &root,
        Platform::current(),
        venv_python,
        &installed,
        &shared.source,
    )
    .await;

    // Packages that are installed still have to expose what the file uses.
    // Purely local, so this costs a directory listing rather than a request.
    findings.extend(guardian::check_attributes(&text, &venv, &installed));

    let published: Vec<Diagnostic> = findings.iter().map(diagnostics::to_diagnostic).collect();
    shared
        .client
        .publish_diagnostics(uri, published, None)
        .await;
}

/// Re-analyze every open buffer, so fixed imports stop being marked.
async fn refresh_all_buffers(shared: Arc<Shared>) {
    let uris: Vec<Url> = shared.documents.lock().await.keys().cloned().collect();
    for uri in uris {
        run_guardian(shared.clone(), uri).await;
    }
}

/// Read the venv's Python version from `pyvenv.cfg`.
fn venv_python(venv: &Path) -> Option<PyVersion> {
    let text = std::fs::read_to_string(venv.join("pyvenv.cfg")).ok()?;
    for line in text.lines() {
        let mut parts = line.splitn(2, '=');
        let key = parts.next()?.trim();
        if key == "version" || key == "version_info" {
            if let Some(v) = parts.next().and_then(|v| PyVersion::parse(v.trim())) {
                return Some(v);
            }
        }
    }
    None
}

async fn run_install(shared: Arc<Shared>, root: &Path, package: &str) {
    let client = &shared.client;
    if install::validate_package_name(package).is_err() {
        client
            .show_message(
                MessageType::ERROR,
                format!("PyPilot: `{package}` is not a valid package name."),
            )
            .await;
        return;
    }

    let settings = Settings::load(root);
    client
        .show_message(MessageType::INFO, format!("PyPilot: installing {package}…"))
        .await;

    match install::install_package(root, &settings, package).await {
        Ok(summary) if summary.ok => {
            client
                .show_message(MessageType::INFO, format!("PyPilot: installed {package}."))
                .await;
        }
        Ok(summary) => report_failed_step(client, &summary).await,
        Err(e) => {
            client
                .show_message(MessageType::ERROR, format!("PyPilot: install failed. {e}"))
                .await;
        }
    }
    refresh_all_buffers(shared).await;
}

async fn run_recreate(shared: Arc<Shared>, root: &Path, python: &str, package: &str) {
    let client = &shared.client;
    let Some(version) = PyVersion::parse(python) else {
        client
            .show_message(
                MessageType::ERROR,
                format!("PyPilot: `{python}` is not a Python version."),
            )
            .await;
        return;
    };
    if install::validate_package_name(package).is_err() {
        client
            .show_message(
                MessageType::ERROR,
                format!("PyPilot: `{package}` is not a valid package name."),
            )
            .await;
        return;
    }

    let settings = Settings::load(root);
    client
        .show_message(
            MessageType::INFO,
            format!("PyPilot: rebuilding the environment on Python {version}…"),
        )
        .await;

    match install::recreate_with_python(root, &settings, version, package).await {
        Ok(summary) if summary.ok => {
            client
                .show_message(
                    MessageType::INFO,
                    format!(
                        "PyPilot: environment now runs Python {version}, with {package} installed."
                    ),
                )
                .await;
        }
        Ok(summary) => report_failed_step(client, &summary).await,
        Err(e) => {
            client
                .show_message(MessageType::ERROR, format!("PyPilot: rebuild failed. {e}"))
                .await;
        }
    }
    refresh_all_buffers(shared).await;
}

async fn report_failed_step(client: &Client, summary: &setup::SetupSummary) {
    let detail = summary
        .steps
        .iter()
        .rev()
        .find(|s| !s.ok)
        .map(|s| format!("{}: {}", s.name, s.detail))
        .unwrap_or_else(|| "the operation did not finish".to_string());
    client
        .show_message(MessageType::ERROR, format!("PyPilot: {detail}"))
        .await;
}

fn string_arg(args: &[serde_json::Value], index: usize) -> Option<String> {
    args.get(index)?.as_str().map(|s| s.to_string())
}

// --- F5: onboarding and the CLI bridge ---------------------------------------

/// Poll the workspace's rescan request file. One stat per second.
async fn watch_rescan_requests(shared: Arc<Shared>, root: PathBuf) {
    let mut last_seen = rescan::request_mtime(&root);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        let current = rescan::request_mtime(&root);
        if current.is_some() && current != last_seen {
            last_seen = current;
            scan_and_notify(&shared.client, &shared.source, &root, true).await;
            refresh_all_buffers(shared.clone()).await;
        }
    }
}

/// The workspace scan. `explicit` means the user asked, which lifts the silence
/// rules and turns a clean result into a confirmation instead of nothing.
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

    // "problems-only" (the default) only raises a toast for Warning/Error, per
    // `all_good()`. "all" also surfaces Info-only results (an MPS note, a
    // conda-migration hint, a driver-matched CUDA build) instead of treating
    // them as nothing to report — that is the entire distinction the setting
    // documents; unsolicited scans never toast on Info under either tier.
    let has_something_to_say = if settings.notifications == Notifications::All {
        !assessment.findings.is_empty()
    } else {
        !assessment.all_good()
    };

    if !has_something_to_say {
        if explicit {
            client
                .show_message(MessageType::INFO, all_good_summary(&assessment))
                .await;
        }
        return;
    }

    let actions = vec![
        action(BTN_FIX),
        action(BTN_DETAILS),
        action(BTN_IGNORE),
        action(BTN_NEVER),
    ];
    let severity = match assessment.worst_severity() {
        Some(Severity::Error) => MessageType::ERROR,
        Some(Severity::Warning) => MessageType::WARNING,
        // Only reachable under the "all" tier: an Info-only result deserves an
        // FYI toast, not one styled like a problem.
        Some(Severity::Info) | None => MessageType::INFO,
    };

    let chosen = client
        .show_message_request(severity, toast_summary(&assessment), Some(actions))
        .await
        .ok()
        .flatten();

    match chosen.as_ref().map(|a| a.title.as_str()) {
        Some(BTN_FIX) => run_fix(client, source, root).await,
        Some(BTN_DETAILS) => show_details(client, source, root).await,
        Some(BTN_NEVER) => mark_never(client, root).await,
        _ => {}
    }
}

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
        Ok(summary) => report_failed_step(client, &summary).await,
        Err(e) => {
            client
                .show_message(MessageType::ERROR, format!("PyPilot: setup error. {e}"))
                .await;
        }
    }
}

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

/// Confirmation for an explicitly requested scan that found nothing wrong.
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
            "PyPilot: everything looks good. The {} is compatible, and dependencies support {}.",
            venv,
            compat.intersection.to_range_string()
        ),
        None => format!("PyPilot: everything looks good. The {venv} is in place."),
    }
}

/// One line for the notification, always naming the package that caused it.
fn toast_summary(a: &Assessment) -> String {
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

/// The report behind "Show details".
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
                Severity::Error => "Error",
                Severity::Warning => "Warning",
                Severity::Info => "Note",
            };
            s.push_str(&format!("### {sev}: {}\n{}\n", f.title, f.detail));
            if let FixKind::RecreateWithPython(v) = &f.fix {
                s.push_str(&format!(
                    "\n_Fix: rebuild the environment on Python {v}._\n"
                ));
            } else if f.fix == FixKind::SetupEnvironment {
                s.push_str("\n_Fix: run \"Fix everything\", or `pypilot setup`._\n");
            } else if f.fix == FixKind::MigrateConda {
                s.push_str("\n_Fix: run `pypilot migrate-conda`._\n");
            }
            s.push('\n');
        }
    }
    s
}

/// Workspace root from the initialize params, folders first.
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

/// Entry point for `pypilot lsp`.
pub fn run_stdio() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build LSP runtime");

    rt.block_on(async {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let (service, socket) = LspService::new(|client| Backend {
            shared: Arc::new(Shared {
                client,
                root: Mutex::new(None),
                documents: Mutex::new(HashMap::new()),
                revisions: Mutex::new(HashMap::new()),
                source: PyPiClient::new(),
            }),
        });
        Server::new(stdin, stdout, socket).serve(service).await;
    });
}
