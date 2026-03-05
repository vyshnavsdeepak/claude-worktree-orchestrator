# Claude Worktree Orchestrator (`cwo`)

A terminal UI that orchestrates multiple Claude AI workers across git worktrees. Point it at a GitHub repo and a tmux session — it reads a discussion issue, extracts tasks, files GitHub issues, spins up isolated worktrees, launches Claude workers, reviews PRs, auto-merges, rebases, and self-heals crashed workers. You watch from a TUI while an army of Claude instances ships code.

## Prerequisites

- **Rust** (`cargo`)
- **tmux** — workers run in tmux windows
- **git** — worktrees for isolation
- **gh** — GitHub CLI for issues, PRs, reviews
- **claude** — [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code)

## Install

```bash
cargo build --release
cp target/release/cwo ~/.local/bin/  # optional: add to PATH
```

## Quick Start

```bash
# 1. Copy and edit the config
cp cwo.toml.example cwo.toml
# Edit: set repo, repo_root, session, discussion_issue

# 2. Create your tmux session
tmux new-session -d -s my-workers

# 3. Run
cwo
```

CWO will start polling the tmux session, reading your discussion issue for tasks, and launching workers.

## Configuration

All config lives in `cwo.toml`. See `cwo.toml.example` for a fully commented template.

### Core Settings

| Field | Default | Description |
|---|---|---|
| `session` | — | Tmux session name where worker windows live |
| `repo` | — | GitHub repo (`owner/name`) |
| `discussion_issue` | — | Issue number used as the product discussion thread |
| `repo_root` | — | Absolute path to your git repo root |
| `max_concurrent` | `3` | Max simultaneous Claude workers |
| `builder_sleep_secs` | `300` | Seconds between discussion-scan cycles |
| `poll_interval_secs` | `1` | Seconds between TUI poll ticks |
| `shell_prompts` | `["$ ", ">> "]` | Patterns that identify a shell prompt |
| `run_builder` | `true` | Set `false` for TUI-only mode (or use `--no-builder`) |

### Merge & Review Policy

| Field | Default | Description |
|---|---|---|
| `merge_policy` | `"auto"` | `"auto"` / `"review_then_merge"` / `"manual"` |
| `auto_review` | `true` | Spawn AI reviewers for new PRs |
| `review_timeout_secs` | `600` | Merge anyway after this timeout (`0` = wait forever) |

**How merge policies work:**

| `merge_policy` | `auto_review` | Behavior |
|---|---|---|
| `auto` | `false` | Merge CLEAN PRs immediately, no review |
| `auto` | `true` | Spawn reviewer, but merge without waiting |
| `review_then_merge` | `true` | Spawn reviewer, wait for APPROVED before merge |
| `review_then_merge` | `false` | Wait for external (human) review, merge when APPROVED |
| `manual` | `true` | Spawn reviewer, never merge — just notify |
| `manual` | `false` | Pure monitoring mode — no merge, no review |

### Worker Health

| Field | Default | Description |
|---|---|---|
| `auto_relaunch` | `true` | Auto-relaunch crashed workers |
| `max_relaunch_attempts` | `3` | Give up and mark `failed` after N relaunches |
| `stale_timeout_secs` | `300` | Mark worker `stale` if no output for this long (`0` = disabled) |

When a worker crashes (Claude exits to a shell prompt), CWO automatically relaunches it with a context-aware prompt that includes `git log` and `git status` so Claude picks up where it left off. After `max_relaunch_attempts` failures, the worker is marked `failed` and a toast notification alerts you.

## Running

```bash
cwo                          # reads ./cwo.toml
cwo --config /path/to/cwo.toml   # explicit config path
cwo --no-builder             # TUI-only mode — watch workers, no task extraction
```

## TUI

### Layout

```
┌─ Claude Worktree Orchestrator ──────────────────────────────────────┐
│ Session: my-workers │ Workers: 5 │ Active: 3 │ Idle: 1 │ Queued: 1 │
│ Backoff: none   ✓ Last scan: 3s ago   Merged: 5 │ Failed: 0 │ ... │
├─────────────────────────────────────────────────────────────────────┤
│ WORKER       PHASE        STATE          LAST OUTPUT               │
│ ▶ issue-326  ●→○ CODING   🟢 working    Analyzing src/main.rs     │
│   issue-327  ●→● PR READY ✅ complete   Created pull request #42  │
│   issue-328  ●→○ CRASHED  🔴 shell exit exec claude --dang...     │
│   issue-329  ○→○ QUEUED   ⏳ in queue   user@host:~$              │
│   issue-330  ●→○ STALE    💀 stale      Thinking...               │
├─────────────────────────────────────────────────────────────────────┤
│ [s] Send [i] Int [b] Broadcast [m] Merge [c] Config [l] Log ...   │
└─────────────────────────────────────────────────────────────────────┘
```

### Key Bindings

| Key | Action |
|---|---|
| `j` / `k` | Navigate workers |
| `d` / `Enter` | Detail overlay — pane scrollback, git log, review notes |
| `s` | Send prompt to selected worker |
| `i` | Interrupt selected worker (sends `C-c`) |
| `b` | Broadcast message to all idle workers |
| `m` | Check and merge all clean PRs |
| `M` | Merge selected worker's PR |
| `v` | Open selected worker's PR in browser |
| `p` | Free-form prompt — Claude extracts tasks and spins up workers |
| `n` | Spin up a worker for an existing issue number |
| `c` | Open Settings panel (live config editor) |
| `l` | Toggle log panel |
| `:` | Send command to builder loop |
| `q` / `Esc` | Quit |

### Settings Panel (`c`)

Press `c` to open an interactive settings panel. Use `j`/`k` to navigate and `Enter` or `Space` to cycle values:

- **Merge Policy** — `auto` → `review_then_merge` → `manual`
- **Auto Review** — `on` / `off`
- **Review Timeout** — `300s` → `600s` → `900s` → `forever`
- **Auto Relaunch** — `on` / `off`
- **Max Relaunch Attempts** — `1` → `2` → `3` → `5`
- **Stale Timeout** — `180s` → `300s` → `600s` → `disabled`

Changes take effect immediately — no restart needed. Settings are persisted in `/tmp/cwo-runtime.json` and override `cwo.toml` values for the current session.

### Commands (`:`)

| Command | Description |
|---|---|
| `merge all` | Check and merge all clean PRs now |
| `merge pr 42` | Merge a specific PR |
| `rebase all` | Fetch main and rebase all workers |
| `broadcast <msg>` | Send message to all idle Claude windows |
| `nudge all` | Send "continue with the task" to idle workers |
| `stats` | Show session summary (merged count, failed count, avg merge time) |

## How It Works

### Architecture

CWO runs three concurrent loops:

1. **Poller** (every `poll_interval_secs`) — Polls tmux windows, classifies each pane's state (active, idle, shell, done, stale, failed, conflict, probing), detects orphaned worktrees, tracks content changes for stale detection.

2. **Builder** (every `builder_sleep_secs`) — Reads the discussion issue via `gh`, calls Claude to extract implementable tasks, files GitHub issues, creates git worktrees, launches Claude workers in tmux windows. Also runs the monitor cycle.

3. **Monitor** (runs within builder cycle) — Health-checks workers, auto-relaunches crashed ones, probes idle workers with `claude --print`, checks and merges open PRs (respecting merge policy), resolves rebase conflicts, cleans up finished windows and worktrees.

### Worker Lifecycle

```
Discussion Issue
    │
    ▼
Builder extracts task → files GitHub issue → creates worktree → launches Claude
    │
    ▼
Claude implements → commits → pushes → opens PR
    │
    ▼
Reviewer spawns (if auto_review) → posts APPROVED or CHANGES_REQUESTED
    │
    ▼
Monitor checks merge state:
  CLEAN + policy allows → squash merge → delete branch → cleanup
  BEHIND → rebase + push → poll for CLEAN → merge
  BLOCKED → fetch review context → send to worker
  DIRTY → spawn AI resolver
    │
    ▼
After merge → fetch main → rebase remaining branches → repeat
```

### Worker States

| State | Icon | Meaning |
|---|---|---|
| `active` | 🟢 | Claude is working (spinner detected) |
| `idle` | 🟡 | Claude is at the prompt, waiting for input |
| `shell` | 🔴 | Claude exited, bare shell prompt visible |
| `stale` | 💀 | No output change for `stale_timeout_secs` |
| `failed` | ❌ | Exceeded `max_relaunch_attempts`, gave up |
| `done` | ✅ | PR created, work complete |
| `queued` | ⏳ | Window exists but Claude not yet launched |
| `sleeping` | 💤 | Rate limited, waiting |
| `conflict` | ⚠️ | Rebase conflict detected |
| `probing` | 🔍 | AI probe running in split pane |
| `no-window` | 👻 | Orphaned worktree with no tmux window |

### Event Log

Every significant action is logged to `{repo_root}/.claude/cwo-events.jsonl`:

```json
{"ts":"2026-03-05T14:32:01Z","event":"worker_launched","issue":326,"branch":"feature/issue-326-fix-perms"}
{"ts":"2026-03-05T14:45:12Z","event":"pr_merged","pr":42}
{"ts":"2026-03-05T14:46:01Z","event":"review_spawned","issue":326,"pr":42}
{"ts":"2026-03-05T14:53:00Z","event":"worker_failed","issue":327,"reason":"3 relaunch failures"}
```

Use `:stats` in the TUI or `cat .claude/cwo-events.jsonl | jq` to inspect history.

## Tips

- **Shell prompts matter.** If CWO can't detect your shell prompt, workers get misclassified. Add your prompt prefix to `shell_prompts` (e.g. `["vyshnav@mac", "$ "]`).
- **Start with `manual` merge policy** on repos with real users. Switch to `review_then_merge` once you trust the reviewer, and `auto` only for personal/experimental repos.
- **Use `--no-builder`** to just monitor existing workers without the builder loop scanning for new tasks.
- **Watch the log panel** (`l`) to see what CWO is doing under the hood.
- **Increase `max_concurrent`** if you have API headroom. Each worker is an independent Claude session.

## Pre-commit Hooks

```bash
git config core.hooksPath .githooks
```

Runs `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
