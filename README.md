# agent-gc

> A lightweight Rust TUI for cleaning up AI coding agent worktrees, duplicate dependencies, and build artifacts.

`agent-gc` helps developers reclaim disk space left behind by tools like Codex, Claude Code, OpenCode, Cursor, and ordinary local development workflows. It is inspired by the simplicity of `npkill`, but focuses on agent-generated worktrees and safer cleanup decisions.

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

- Fast keyboard-first TUI (vim `j`/`k` supported)
- Scans common AI agent and developer project paths
- Detects dependency folders, build outputs, and language caches
- Marker-aware classification (`target` needs `Cargo.toml`; bare `env` needs venv markers)
- Groups Codex, Claude Code, OpenCode, Cursor, Gemini, and Aider paths as agent-related candidates
- Calculates size, last modified time, category, risk level, project name, git cleanliness, and dangerous file presence
- Locks dangerous candidates so they cannot be selected or deleted
- Partial-delete reporting (successful deletes are marked even if some paths fail)
- CLI dry-run, presets, and non-interactive `--yes`
- Ships as a small npm wrapper around a native Rust binary

## Status

`agent-gc` is early software (0.2.x).

Published npm packages currently include a **macOS arm64** vendor binary by default. Scan/runtime logic is multi-platform (separator-agnostic paths, portable home, XDG roots). Other OS binaries: build from source + `scripts/vendor-current.sh`, or ship via CI later.

```text
Vendor binary shipped today: macOS arm64
Wrapper looks for:           darwin-arm64, darwin-x64, linux-x64, linux-arm64,
                             win32-x64, win32-arm64
From source:                 cargo build --release && ./scripts/vendor-current.sh
```

### 0.2 highlights

- Safer classification (fewer false positives)
- Skip `.git` / VCS / trash dirs; cache git status per project
- Path segment matching (`/` and `\` safe); portable home + XDG roots
- TUI accepts scan paths; category/risk/min-size filters; multi-mode sort
- CLI presets: `safe`, `agent-only`, `older` + `--older-than`
- CLI `--yes` for scripts; non-TTY delete refused without it
- RELEASABLE metric excludes already-deleted rows

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

Open the TUI (optional paths):

```bash
agent-gc
agent-gc ~/dev ~/.codex/worktrees
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
agent-gc clean --dry-run --preset agent-only
agent-gc clean --dry-run --preset older --older-than 30d
agent-gc clean --dry-run --preset safe --category node --min-size 1GB
```

Delete selected safe artifacts after confirmation, or non-interactively:

```bash
agent-gc clean --preset safe
agent-gc clean --preset safe --yes
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

CLI presets:

```text
safe         SAFE risk only
agent-only   SAFE agent / agent-cache paths
older        SAFE items older than --older-than (default 30d)
```

## TUI Controls

```text
↑ / ↓ / j / k   move
Space           select / unselect
a               add visible SAFE items
d               delete selected items
f               category filter
t               risk filter
m               min-size filter (off → 10MB → 100MB → 1GB)
s               sort cycle (SIZE ↓/↑, AGE ↓/↑, PATH, RISK)
Enter           detail view
r               rescan
q               quit
Ctrl-C          quit
```

Deleted rows stay visible and show `DEL` in the `Sel` column. `RELEASABLE` is the sum of non-deleted candidates; `DELETED` accumulates reclaimed bytes.

## Default Scan Paths

When no paths are given, `agent-gc` scans these locations if they exist:

```text
~/.codex/worktrees
~/.claude
~/.opencode
~/.cursor
~/.gemini
~/.aider
$XDG_CONFIG_HOME/opencode  (or ~/.config/opencode)
$XDG_CACHE_HOME/opencode   (or ~/.cache/opencode)
~/dev
~/workspace
~/projects
~/Developer
~/src
# Windows also tries:
~/source/repos
~/Documents/GitHub
~/Documents/Projects
```

Agent path detection uses path **components**, not raw `"/..."` substrings, so classification stays correct on Windows-style paths.

You can also pass explicit paths:

```bash
agent-gc scan ~/dev ~/workspace ~/.codex/worktrees
agent-gc ~/dev
```

## Detected Artifacts

```text
node_modules
.next
.turbo
dist          (project marker required)
build         (project marker required)
coverage      (project marker required)
.vite
.cache
.venv
venv
env           (python venv markers required)
__pycache__
.pytest_cache
.mypy_cache
.ruff_cache
target        (Cargo.toml ancestor required)
```

VCS directories (`.git`, `.svn`, `.hg`, `.jj`) and `.Trash` are skipped while walking.

## Safety Model

`agent-gc` is intentionally conservative.

```text
SAFE     selectable with Space or bulk-selected with a
CAUTION  selectable with Space only
DANGER   locked; cannot be selected or deleted
```

Dangerous local files such as `.env`, local databases, uploads, secrets, and key files make a candidate `DANGER`. Template env files (`.env.example`, `.env.sample`, `.env.template`, `.env.test`) are allowed.

Deletion always requires an explicit confirmation prompt in interactive mode. Use `--yes` only in trusted automation. Partial failures report which paths failed and still mark successful deletes.

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

Vendor the current host binary for the npm wrapper:

```bash
./scripts/vendor-current.sh
# or
npm run vendor
```

Test the npm wrapper locally:

```bash
cargo build --release
./scripts/vendor-current.sh
node bin/agent-gc.js --help
node bin/ag.js --help
node bin/agent-gc.js scan --json .
```

Inspect the npm package contents:

```bash
npm pack --dry-run
```

## Contributing

Issues and pull requests are welcome. Please keep changes small, conservative, and easy to review.

## License

MIT
