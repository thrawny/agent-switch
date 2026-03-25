# agent-switch

Track and switch between AI coding agent sessions (Claude, Codex) across **tmux** and **niri**.

## What it does

- Tracks agent session state (`waiting`, `working`, `idle`) from hook events
- Lets you quickly switch tmux windows with a compact picker
- Shows a niri overlay switcher (GTK, Linux) with workspace/column shortcuts
- Merges Claude + Codex state into one view

---

## Install

```bash
just build
just install
```

This installs `agent-switch` to `~/.cargo/bin/agent-switch`.

---

## tmux usage

### Start daemon

```bash
agent-switch serve
```

Example tmux autostart:

```tmux
run-shell -b 'pgrep -f "agent-switch serve" >/dev/null 2>&1 || agent-switch serve &'
```

### Open picker

```bash
agent-switch tmux
```

- 2-key mode: first key picks session, second picks window
- `/` enters fzf search
- `q` / `Esc` cancels

Direct fzf mode:

```bash
agent-switch tmux --fzf
```

Optional binding:

```tmux
bind-key -n C-` display-popup -E -w 60% -h 60% "agent-switch tmux"
```

### Session order source

tmux session ordering is read from `~/.config/agent-switch/config.toml` using the `[[project]]` list order
(the same file used by niri). If `name` is omitted, the project name is inferred from the
last folder segment of `dir`.

tmux also respects:
- `ignore = ["..."]` to hide matching session names
- `ignoreNumericSessions = true` to hide numeric-only session names (e.g. `1`, `2`)

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

You can also press `a` inside the overlay to switch between the full workspace view and agents-only view. In agents-only view, press `Space` to smart-jump to the most relevant agent window.

Optional niri binds:

```kdl
Mod+S { spawn "agent-switch" "niri" "--toggle"; }
Mod+A { spawn "agent-switch" "niri" "--toggle-agents"; }
```

Optional startup entry:

```kdl
spawn-at-startup "agent-switch" "serve" "--niri"
```

### niri project config (`~/.config/agent-switch/config.toml`)

Example:

```toml
ignore = ["games", "web"]
ignoreUnnamedWorkspaces = true
ignoreNumericSessions = true

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
- `ignoreUnnamedWorkspaces` defaults to `true` (niri)
- `ignoreNumericSessions` defaults to `false` and works for both niri + tmux
- `ignore` works for discovered niri workspaces and tmux sessions
- if `project.name` is omitted, name is inferred from `dir` basename
- `static_workspace = true` means “focus existing workspace, don’t auto-create”

---

## Claude Code hook setup

Without hooks, switching still works, but live state labels will be incomplete.

Configure hooks in **`~/.claude/settings.json`**:

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "agent-switch track stop --agent claude" }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          { "type": "command", "command": "agent-switch track prompt-submit --agent claude" }
        ]
      }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt",
        "hooks": [
          { "type": "command", "command": "agent-switch track notification --agent claude" }
        ]
      }
    ],
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "agent-switch track session-start --agent claude" }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          { "type": "command", "command": "agent-switch track session-end --agent claude" }
        ]
      }
    ]
  }
}
```

### Hook requirements

- `agent-switch` must be on `PATH` for Claude
- daemon should be running (`agent-switch serve` or `agent-switch serve --niri`)
- run Claude inside tmux if you want tmux window IDs captured
- every hook must identify the agent, either via `--agent claude` or an `agent` field in the hook payload

---

## Codex hook setup

Codex now uses the same tracked-session path as other agents. Hooks still provide the agent name,
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
    "SessionStart": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "agent-switch track session-start --agent codex",
            "timeout": 5
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "agent-switch track prompt-submit --agent codex",
            "timeout": 5
          }
        ]
      }
    ],
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "agent-switch track stop --agent codex",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

Notes:
- daemon should be running (`agent-switch serve` or `agent-switch serve --niri`)
- run Codex inside tmux if you want tmux window IDs captured
- on niri, `agent-switch track` also captures the currently focused window ID automatically
- every hook must identify the agent, either via `--agent codex` or an `agent` field in the hook payload

---

## Useful commands

```bash
agent-switch list      # dump tracked sessions as JSON
agent-switch cleanup   # remove stale sessions
```

Socket path: `$XDG_RUNTIME_DIR/agent-switch.sock` (fallback: `/tmp/agent-switch.sock`).

---

## License

MIT — see [`LICENSE`](./LICENSE).
