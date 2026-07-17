# agent-switch

Track and switch between AI coding agent sessions (Claude, Codex, Pi, OpenCode) on **niri**.

## What it does

- Tracks agent session state (`waiting`, `responding`, `idle`) from hook events
- Provides a GTK overlay for jumping between active agent sessions
- Merges agent state into one daemon-backed session cache
- Exposes session state as JSON (`list`, `focused`) for status bars and scripts

---

## Install

```bash
just build
just install
```

This installs `agent-switch` to `~/.cargo/bin/agent-switch`.

---

## Usage

### Start daemon + overlay

```bash
agent-switch serve --niri
```

From source:

```bash
cargo run -- serve --niri
```

### Toggle the agents overlay

Opens the overlay listing windows with active agent sessions:

```bash
agent-switch toggle
```

Press `Space` to smart-jump to the most relevant agent window, or the shown
key to jump to a specific session. `q`/`Escape` closes the overlay.

Optional niri binds:

```kdl
Mod+S { spawn "agent-switch" "toggle"; }
```

Optional startup entry:

```kdl
spawn-at-startup "agent-switch" "serve" "--niri"
```

### Config (`~/.config/agent-switch/config.toml`)

Example:

```toml
ignore = ["games", "web"]
ignoreUnnamedWorkspaces = true
ignoreNumericSessions = true
theme = "molokai"
```

Notes:
- `ignore` hides matching discovered niri workspaces
- `ignoreUnnamedWorkspaces` defaults to `true`
- `ignoreNumericSessions` defaults to `false`

---

## Claude Code hook setup

Without hooks, switching still works, but live state labels will be incomplete.

Configure hooks in **`~/.claude/settings.json`**:

```json
{
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "agent-switch track stop --agent claude" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "agent-switch track prompt-submit --agent claude" }] }],
    "Notification": [{ "matcher": "permission_prompt", "hooks": [{ "type": "command", "command": "agent-switch track notification --agent claude" }] }],
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "agent-switch track session-start --agent claude" }] }],
    "SessionEnd": [{ "hooks": [{ "type": "command", "command": "agent-switch track session-end --agent claude" }] }]
  }
}
```

### Hook requirements

- `agent-switch` must be on `PATH`
- daemon should be running (`agent-switch serve --niri`)
- on niri, `agent-switch track` captures the currently focused window ID automatically
- every hook must identify the agent, either via `--agent claude` or an `agent` field in the hook payload

---

## Codex hook setup

Codex uses the same tracked-session path as other agents. Hooks provide the agent name,
session identity, and current window binding, while rollout watching continues to drive
transcript-derived live state.

Enable Codex hooks in **`~/.codex/config.toml`**:

```toml
[features]
codex_hooks = true
```

Configure the hook in **`~/.codex/hooks.json`**:

```json
{
  "hooks": {
    "SessionStart": [{ "matcher": "", "hooks": [{ "type": "command", "command": "agent-switch track session-start --agent codex", "timeout": 5 }] }],
    "UserPromptSubmit": [{ "matcher": "", "hooks": [{ "type": "command", "command": "agent-switch track prompt-submit --agent codex", "timeout": 5 }] }],
    "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": "agent-switch track stop --agent codex", "timeout": 5 }] }]
  }
}
```

Notes:
- daemon should be running (`agent-switch serve --niri`)
- on niri, `agent-switch track` captures the currently focused window ID automatically
- every hook must identify the agent, either via `--agent codex` or an `agent` field in the hook payload

---

## Design notes

- [Background-task session state](docs/background-task-state.md) — deferred PID-tree approach
  for distinguishing delegated work from true idle without transcript parsing

## Useful commands

```bash
agent-switch list      # dump tracked sessions as JSON
agent-switch focused   # dump the focused window's session as JSON
agent-switch cleanup   # remove stale sessions
```

Socket path: `$AGENT_SWITCH_SOCKET` if set, otherwise `$XDG_RUNTIME_DIR/agent-switch/agent-switch.sock` (fallback: `/tmp/agent-switch/agent-switch.sock`).

---

## License

MIT — see [`LICENSE`](./LICENSE).
