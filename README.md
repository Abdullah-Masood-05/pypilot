# PyPilot for Zed

You clone a Python repo, run the install, and it fails on a package that has no
wheel for the interpreter you happen to have. PyPilot works out which Python
version the project's dependencies can actually run on, builds the venv on that
version, and tells you which package set the limit.

Every answer comes from package metadata and static tables. There are no API
keys and no model calls.

## The problem it solves

A package's `requires_python` field is often a lie of omission. mediapipe 0.10.9
declares `>=3.8`, but the only wheels it ships are cp39 through cp312. Install it
on Python 3.13 and pip goes looking for a source distribution that isn't there,
then fails with something about metadata. Nothing in that error mentions Python
versions.

PyPilot reads both signals. It takes `requires_python` as a starting range, then
intersects it with the interpreter versions that actually have a wheel built for
your OS and CPU. Do that for every dependency in the project and the overlap is
the set of interpreters the whole thing can run on. If the overlap is empty, two
packages disagree, and PyPilot says which two and what each one needs.

Naming the two packages matters more than applying the fix, because a conflict
between dependencies is the case where the error message tells you least.

## Status

Phase 0 and Phase 1 are done: the helper binary, the PyPI compatibility engine,
the environment bootstrap, and the onboarding notification.

Two features are scaffolded but not implemented. The hardware matrix in
`matrix/` returns an empty report, so nothing checks GPU drivers or CUDA builds
yet. There are no per-buffer diagnostics on `import` statements either. Both
have their module boundaries and call sites in place, so neither needs a
refactor to land.

## How it fits together

The Zed extension in `extension/` is a WASM shim of under 200 lines. It detects
your platform, downloads the matching helper binary, and registers it as a
language server. It holds no logic of its own, which keeps Zed API changes from
reaching anything important.

Everything else lives in `helper/`, a native binary with three modes over one
shared library:

```
pypilot doctor    read-only report, changes nothing
pypilot setup     build the environment
pypilot lsp       stdio LSP server, the mode Zed launches
```

The helper knows nothing about Zed and works in any terminal or CI job. The
toast button labelled "Fix everything" and the `pypilot setup` command call the
same function, so the two surfaces cannot drift apart.

Running `doctor` or `setup` from a Zed task also writes a small request file
into the cache directory. If an LSP instance is watching that workspace it
picks the request up within a second and shows the result as a notification, so
a task run gets the same actionable buttons as the startup scan.

## Layout

```
pypilot/
├─ extension/            Zed WASM shim (extension.toml + src/lib.rs)
├─ helper/               native binary
│  ├─ src/
│  │  ├─ main.rs         mode dispatch
│  │  ├─ cli/            doctor and setup front ends
│  │  ├─ lsp/            tower-lsp server, notifications, commands
│  │  ├─ core/           probes, project parsing, solver, uv and pip drivers
│  │  ├─ pypi/           metadata client, wheel tag parser, disk cache
│  │  └─ matrix/         hardware seam (stub)
│  ├─ data/              nvidia.json, frameworks.json, import_map.json
│  └─ tests/             fixtures and offline integration tests
├─ tasks/                Zed task templates
└─ .github/workflows/    cross platform CI and release builds
```

## Settings

Global config lives in the platform config directory. A project can override any
key from `.zed/pypilot.toml` or `pypilot.toml` in its root.

```toml
package_manager    = "uv"             # "uv" or "pip". pip mode never touches uv.
notifications      = "problems-only"  # "all", "problems-only", or "off"
auto_check_on_open = true
data_refresh_days  = 7                # 0 stays fully offline
```

In pip mode the compatibility checks are identical, because none of that logic
knows which installer you use. What changes is that pip cannot fetch a missing
interpreter, so if the project needs Python 3.12 and you don't have it, PyPilot
says so instead of installing it for you.

## Commands

Zed gives extensions no way to add command palette entries, so the commands ship
as tasks. Copy `tasks/pypilot.json` into `.zed/tasks.json` for one project, or
into your global Zed `tasks.json`, then run them from `task: spawn`.

On Windows, Zed cannot spawn Microsoft Store app execution aliases, which is
most PowerShell 7 installs. If a task fails with "os error 193", point the task
at a real executable such as `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`.
The template has the details.

## Build and test

```bash
cargo test  -p pypilot-helper
cargo build -p pypilot-helper --release

rustup target add wasm32-unknown-unknown
cargo build -p pypilot-zed --target wasm32-unknown-unknown --release
```

The tests never touch the network. PyPI responses are recorded JSON fixtures,
and the platform is pinned to Linux x86-64 inside the wheel tag tests so results
don't change with the machine running them.

To try the extension against a local build, put `pypilot` on your PATH with
`cargo install --path helper`, then use `zed: install dev extension` and pick the
`extension` directory. The shim prefers a `pypilot` already on PATH over
downloading a release.

## License

Two licenses, because the two halves are distributed differently.

`helper/`, which is the whole engine, is **AGPL-3.0-or-later**. See
[helper/LICENSE](helper/LICENSE).

`extension/`, the WASM shim Zed compiles and distributes, is **Apache-2.0**. See
[LICENSE](LICENSE). Zed's extension registry only accepts a fixed list of
licenses for the code that becomes the extension binary, and AGPL is not on it.
Their rules exempt tools the extension merely downloads and runs, naming
language servers specifically, which is exactly what the helper is.

In practice: use PyPilot however you like. Ship a modified helper and you owe
users its source.
