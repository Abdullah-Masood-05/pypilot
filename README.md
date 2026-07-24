# PyPilot for Zed

A Zed editor extension that sets up Python environments and resolves
version/hardware compatibility — deterministically, with **zero AI/LLM calls**.
Every decision is a metadata lookup, a static table, or a shell command.

> **Status:** Phase 0 + Phase 1 (skeleton, `doctor`/`setup`, the PyPI metadata
> engine, and the onboarding toast). The live import guardian (F4) and the
> CUDA/hardware matrix (F2) are scaffolded but not yet implemented.

## Architecture

Two components, one repo:

- **`extension/`** — a thin Zed WASM shim (`zed_extension_api`). It detects the
  platform, downloads the matching prebuilt helper on first run, and registers it
  as a Python language server. Almost no logic, so Zed API churn barely touches it.
- **`helper/`** — the native `pypilot` binary. One binary, three modes, one shared
  brain:
  - `pypilot doctor` — read-only probe + compatibility report.
  - `pypilot setup` — full environment bootstrap (uv by default, pip if configured).
  - `pypilot lsp` — the stdio LSP server Zed launches (F5 onboarding toast).

The helper is **editor-agnostic** by design: it works standalone in any terminal
or CI. Only the shim knows about Zed.

### How the compatibility engine works (the "mediapipe problem")

The set of CPython versions a project can run on is a small, bounded universe
(`3.7…3.14`). Each dependency's `requires_python` (PEP 440) and its wheel `cpXY`
tags — filtered to your OS/arch — *narrow* that set. The project's answer is the
**intersection** across every dependency. If it's empty, PyPilot names the
conflicting pair. mediapipe ships `cp39`–`cp312` wheels only, so on Python 3.13 it
can't install even though `requires_python` says `>=3.8` — PyPilot catches that and
suggests 3.12, stating *why*.

## Layout

```
pypilot/
├─ extension/            # Zed WASM shim (extension.toml + src/lib.rs)
├─ helper/               # native binary
│  ├─ src/
│  │  ├─ main.rs         # mode dispatch: lsp | setup | doctor | check | fix
│  │  ├─ cli/            # doctor + setup frontends
│  │  ├─ lsp/            # tower-lsp server (F5 toast, executeCommand)
│  │  ├─ core/           # probes, project parsing, solver, uv/pip drivers
│  │  ├─ pypi/           # F3: metadata client, wheel-tag parser, cache
│  │  └─ matrix/         # F2 seam: bundled-data loader (stub)
│  ├─ data/              # nvidia.json, frameworks.json, import_map.json
│  └─ tests/             # fixtures + integration tests (offline)
├─ tasks/                # Zed task templates (F6)
└─ .github/workflows/    # cross-platform CI + release
```

## Settings (F7)

Global config in the platform config dir, overridable per-project via
`<workspace>/.zed/pypilot.toml` or `<workspace>/pypilot.toml`:

```toml
package_manager   = "uv"            # "uv" (default) | "pip" — pip never touches uv
notifications     = "problems-only" # "all" | "problems-only" | "off"
auto_check_on_open = true
data_refresh_days  = 7              # 0 = fully offline
```

## Build & test

```bash
# Native helper (the part with all the logic):
cargo test  -p pypilot-helper
cargo build -p pypilot-helper --release

# WASM shim:
rustup target add wasm32-unknown-unknown
cargo build -p pypilot-zed --target wasm32-unknown-unknown --release
```

Tests are fully offline — PyPI is mocked with recorded JSON fixtures.

## License

MIT — see [LICENSE](LICENSE).
