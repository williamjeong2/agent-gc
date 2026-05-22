# agent-gc

> A lightweight Rust TUI for cleaning up AI coding agent worktrees, duplicate dependencies, and build artifacts.

`agent-gc` helps developers reclaim disk space left behind by tools like Codex, Claude Code, OpenCode, and ordinary local development workflows. It is inspired by the simplicity of `npkill`, but focuses on agent-generated worktrees and safer cleanup decisions.

```bash
npx agent-gc
```

After installation, the package exposes both commands:

```bash
agent-gc
ag
```

> `ag` is also the historical command name for The Silver Searcher. Because the `ag` npm package name already exists, use `npx agent-gc` for one-off runs. To run the alias through npm, use `npm exec --package agent-gc ag`.

## Why

AI coding agents often create temporary worktrees and run package installs, builds, tests, and caches inside each one. A single project can end up duplicated across paths like:

```text
~/.codex/worktrees/91e7/my-app/node_modules
~/.codex/worktrees/02f3/my-app/node_modules
~/.cache/opencode/packages/.../node_modules
```

General disk cleaners can find large folders, but they usually do not understand:

```text
- whether a folder belongs to an AI agent worktree
- whether an artifact is safely regenerable
- whether a git worktree has uncommitted changes
- whether dangerous local files are present
- how much space can be reclaimed before deleting anything
```

`agent-gc` is built for that specific cleanup loop.

## Features

- Fast keyboard-first TUI
- Scans common AI agent and developer project paths
- Detects dependency folders, build outputs, and language caches
- Groups Codex, Claude Code, and OpenCode paths as agent-related candidates
- Calculates size, last modified time, category, risk level, project name, git cleanliness, and dangerous file presence
- Locks dangerous candidates so they cannot be selected or deleted
- Supports dry-run CLI cleanup for scripts
- Ships as a small npm wrapper around a native Rust binary

## Status

`agent-gc` is early MVP software.

The current npm package ships a macOS arm64 binary only. Other platforms will be added once release binaries are available.

```text
Supported now: macOS arm64
Planned:       macOS x64, Linux x64, Linux arm64
```

## Install

Run without installing:

```bash
npx agent-gc
```

Install globally:

```bash
npm install -g agent-gc
agent-gc
ag
```

From source:

```bash
git clone https://github.com/williamjeong2/agent-gc.git
cd agent-gc
cargo run
```

## Usage

Open the TUI:

```bash
agent-gc
```

Scan in CLI mode:

```bash
agent-gc scan
agent-gc scan --json
agent-gc scan ~/dev ~/.codex/worktrees
agent-gc scan --category agent --min-size 500MB
```

Preview cleanup without deleting anything:

```bash
agent-gc clean --dry-run --preset safe
agent-gc clean --dry-run --preset safe --category node --min-size 1GB
```

Delete selected safe artifacts from CLI mode after confirmation:

```bash
agent-gc clean --preset safe
```

CLI category filters:

```text
agent
agent-cache
node
python
rust
cache
other
```

## TUI Controls

```text
↑ / ↓     move
Space     select / unselect
a         add visible SAFE items
d         delete selected items
f         change category filter
s         toggle size sort direction
Enter     detail view
r         rescan
q         quit
Ctrl-C    quit
```

Deleted rows stay visible in the table and show `DEL` in the `Sel` column. The top metrics keep the original `RELEASABLE` total separate from the accumulated `DELETED` total.

## Default Scan Paths

On macOS, `agent-gc` scans these paths by default when they exist:

```text
~/.codex/worktrees
~/.claude
~/.opencode
~/.config/opencode
~/.cache/opencode
~/dev
~/workspace
~/projects
```

You can also pass explicit paths:

```bash
agent-gc scan ~/dev ~/workspace ~/.codex/worktrees
```

## Detected Artifacts

```text
node_modules
.next
.turbo
dist
build
coverage
.vite
.cache
.venv
venv
env
__pycache__
.pytest_cache
.mypy_cache
.ruff_cache
target
```

## Safety Model

`agent-gc` is intentionally conservative.

```text
SAFE     selectable with Space or bulk-selected with a
CAUTION  selectable with Space only
DANGER   locked; cannot be selected or deleted
```

Dangerous local files such as `.env`, local databases, uploads, secrets, and key files make a candidate `DANGER`. Deletion always requires an explicit confirmation prompt.

## Development

Build:

```bash
cargo build --release
```

Run checks:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Test the npm wrapper locally:

```bash
cargo build --release
mkdir -p vendor
cp target/release/agent-gc vendor/agent-gc-darwin-arm64
chmod 755 vendor/agent-gc-darwin-arm64
node bin/agent-gc.js --help
node bin/ag.js --help
```

Inspect the npm package contents:

```bash
npm pack --dry-run
```

## Contributing

Issues and pull requests are welcome. Please keep changes small, conservative, and easy to review.

## License

MIT
