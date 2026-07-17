# agent-switch

Track and switch between AI coding agent sessions (Claude, Codex, Pi, OpenCode) on **niri**.

## What it does

- Tracks agent session state (`waiting`, `responding`, `idle`) from hook events
- Shows a niri GTK overlay for switching workspaces and configured app windows
- Provides an agents-only overlay for jumping between active agent sessions
- Merges agent state into one daemon-backed session cache

---

## Install

```bash
just build
just install
```

This installs `agent-switch` to `~/.cargo/bin/agent-switch`.

---

## niri usage (Linux)

`niri` overlay requires the Cargo `niri` feature.

### Start daemon + overlay

```bash
agent-switch serve --niri
```

From source:

```bash
cargo run --features niri -- serve --niri
```

### Toggle overlay

```bash
agent-switch niri --toggle
```

### Toggle agents-only view

Opens the overlay filtered to only show windows with active agent sessions:

```bash
agent-switch niri --toggle-agents
```

In agents-only view, press `Space` to smart-jump to the most relevant agent window.

Optional niri binds:

```kdl
Mod+S { spawn "agent-switch" "niri" "--toggle"; }
Mod+A { spawn "agent-switch" "niri" "--toggle-agents"; }
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

[bindings.workspaces]
h = "dotfiles"

[[bindings.apps]]
key = "s"
label = "slack"
appId = "Slack"

[[bindings.apps]]
key = "t"
label = "teams"
titleContains = "Microsoft Teams"

[[project]]
dir = "~/dotfiles"
static_workspace = true

[[project]]
name = "company"
dir = "~/code/the-office"

[[project]]
dir = "~/code/agent-switch" # name inferred from folder if omitted
```

Notes:
- `ignoreUnnamedWorkspaces` defaults to `true`
- `ignoreNumericSessions` defaults to `false`
- `ignore` hides matching discovered niri workspaces
- if `project.name` is omitted, name is inferred from `dir` basename
- `static_workspace = true` means “focus existing workspace, don’t auto-create”
- app bindings are explicit only and hidden when no matching window is open

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
agent-switch cleanup   # remove stale sessions
```

Socket path: `$AGENT_SWITCH_SOCKET` if set, otherwise `$XDG_RUNTIME_DIR/agent-switch/agent-switch.sock` (fallback: `/tmp/agent-switch/agent-switch.sock`).

---

## License

MIT — see [`LICENSE`](./LICENSE).
