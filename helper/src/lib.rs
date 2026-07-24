//! PyPilot helper library — the editor-agnostic brain.
//!
//! The same code backs all three binary modes (`doctor`, `setup`, `lsp`). Module
//! boundaries are drawn so that the not-yet-built features slot in without a
//! refactor:
//!
//! * [`pypi`] — F3, the PyPI metadata engine. Self-contained; knows nothing of
//!   uv, LSP, or the editor. Complete.
//! * [`matrix`] — F2 seam. Bundled-data loader plus a `HardwareReport` that is
//!   empty today; the CUDA/framework solver drops in here.
//! * [`core`] — the engine that ties probing + project parsing + `pypi` +
//!   `matrix` into findings and executes the bootstrap flow.
//! * [`lsp`] — thin tower-lsp frontend. F4 buffer diagnostics attach here.
//! * [`cli`] — thin `doctor` / `setup` frontends.
//! * [`settings`] — F7 config, threaded through everything via [`settings::Settings`].

pub mod cli;
pub mod core;
pub mod lsp;
pub mod matrix;
pub mod pypi;
pub mod settings;

/// Crate-wide result alias.
pub type Result<T> = anyhow::Result<T>;
