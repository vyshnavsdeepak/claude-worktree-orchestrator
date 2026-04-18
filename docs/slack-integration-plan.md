# Slack Integration Plan

Synthesis of issue #3 discussion. See the full thread:
https://github.com/vyshnavsdeepak/claude-worktree-orchestrator/issues/3

## V1 Scope

### Phase 1 — Read-only notifications + status
- Thread-per-worker: notify on state transitions (started, PR opened, merged, failed/stale)
- `cwo: status` command (only inbound command in phase 1)
- Startup/reconnect messages
- Mandatory `allowed_users` (Slack user IDs), default-deny

### Phase 2 — Write commands
- `cwo: <prompt>` to launch work via existing `__DIRECT_` protocol
- `cwo: merge all`
- `cwo: send <worker> <message>`
- Confirmation flow for destructive ops

## Implementation Order

1. Socket mode task — `tokio::spawn` with `tokio-tungstenite` + `reqwest`, clone `cmd_tx`/`prompt_tx`
2. Outbound notifications — subscribe to `worker_rx`, diff state transitions, thread-per-worker
3. Inbound `cwo: status` — parse, allowlist-check, respond in-thread
4. Auth & audit logging
5. Phase 2 write commands with confirmation gates

## Key Risks

| Risk | Mitigation |
|------|------------|
| Prompt injection via Slack | Mandatory user ID allowlist, separate `slack_claude_flags` defaulting to `[]` (no `--dangerously-skip-permissions`) |
| Token exposure | Env vars only, move `/tmp/cwo-*` to `~/.local/state/cwo/` with `0600` perms |
| Notification spam | State-transition-only, batch burst events, rate-limit 1 msg/sec |

## Config Shape

```toml
[slack]
enabled = true
channel = "#cwo-workers"
allowed_users = ["U12345"]  # Slack user IDs, default-deny
# Token via $SLACK_BOT_TOKEN env var — never in config
```

## Open Questions

1. Hard-block `--dangerously-skip-permissions` for Slack-triggered workers, or configurable via `slack_claude_flags`?
2. Multi-instance (`cwo@host:` targeting) needed for v1?
3. Thread key: `window_name + branch` sufficient, or need UUIDs?
