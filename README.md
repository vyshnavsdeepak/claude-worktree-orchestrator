# Claude Worktree Orchestrator (`cwo`)

A terminal UI for orchestrating Claude AI workers across git worktrees. Point it at any GitHub repo and a tmux session — it monitors worker windows, probes idle workers with AI log-readers, auto-merges clean PRs, resolves rebase conflicts, and runs a builder loop that reads a discussion issue and files concrete GitHub issues for Claude to implement.

## Install

```bash
cargo build --release
# Optional: install to PATH
cp target/release/cwo ~/.local/bin/cwo
```

Requires: `cargo`, `tmux`, `git`, `gh` (GitHub CLI), `claude` CLI.

## Configure

```bash
cp cwo.toml.example cwo.toml
# Edit cwo.toml — set repo, repo_root, session, discussion_issue
```

Key fields:

| Field | Description |
|---|---|
| `session` | Tmux session where worker windows live |
| `repo` | GitHub repo (`owner/name`) |
| `discussion_issue` | Issue number used as the product discussion thread |
| `repo_root` | Absolute path to your git repo |
| `shell_prompts` | Prefixes that identify a shell prompt (e.g. `["user@host", "$ "]`) |
| `max_concurrent` | Max simultaneous Claude workers |
| `builder_sleep_secs` | Seconds between discussion-scan cycles |

## Run

```bash
./target/release/cwo                     # reads ./cwo.toml
cwo --config /path/to/cwo.toml           # explicit config
cwo --no-builder                         # TUI-only, no builder loop
```

## Key bindings

| Key | Action |
|---|---|
| `j` / `k` | Navigate workers |
| `s` | Send prompt to selected worker |
| `i` | Interrupt selected worker (C-c) |
| `b` | Broadcast to all idle workers |
| `m` | Check and merge all clean PRs |
| `M` | Merge selected worker's PR |
| `d` / Enter | Detail overlay (pane scrollback / git log) |
| `p` | Free-form prompt → Claude extracts & files issues |
| `n` | Spin up worker for an existing issue number |
| `:` | Send command to builder loop |
| `l` | Toggle log panel |
| `r` | Force refresh |
| `q` / Esc | Quit |

## How it works

1. **Poller** — polls tmux windows every `poll_interval_secs`, classifies each pane (active / idle / shell / done / conflict / probing), merges orphaned worktrees.
2. **AI probes** — for idle or shell-with-history workers, spawns `claude --print` in a bottom split pane to read the log and take action (push, PR, rebase).
3. **Builder loop** — every `builder_sleep_secs`, reads the discussion issue, calls Claude to extract new tasks, files GitHub issues, creates worktrees, launches Claude workers.
4. **Auto-merge** — detects merged PRs, tests rebase cleanliness, pushes rebased branches, and squash-merges CLEAN PRs.

## Pre-commit hooks

```bash
git config core.hooksPath .githooks
```

Runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
