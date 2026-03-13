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

tmux session ordering is read from `~/.config/projects.toml` using the `[[project]]` list order
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

Optional niri bind:

```kdl
Mod+S { spawn "agent-switch" "niri" "--toggle"; }
```

Optional startup entry:

```kdl
spawn-at-startup "agent-switch" "serve" "--niri"
```

### niri project config (`~/.config/projects.toml`)

Example:

```toml
ignore = ["games", "web"]
ignoreUnnamedWorkspaces = true
ignoreNumericSessions = true
codexAliases = ["cx", "cxy"]

[[project]]
dir = "~/dotfiles"
static_workspace = true

[[project]]
name = "company"
dir = "~/code/the-company-private"

[[project]]
dir = "~/code/agent-switch" # name inferred from folder if omitted
```

Notes:
- `ignoreUnnamedWorkspaces` defaults to `true` (niri)
- `ignoreNumericSessions` defaults to `false` and works for both niri + tmux
- `codexAliases` adds extra names (for example `cx`, `cxy`) to detect Codex windows in tmux + niri
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
          { "type": "command", "command": "agent-switch track stop" }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          { "type": "command", "command": "agent-switch track prompt-submit" }
        ]
      }
    ],
    "Notification": [
      {
        "matcher": "permission_prompt",
        "hooks": [
          { "type": "command", "command": "agent-switch track notification" }
        ]
      }
    ],
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "agent-switch track session-start" }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          { "type": "command", "command": "agent-switch track session-end" }
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

---

## Codex hook setup

Codex rollout watching is enough for live state, but it cannot reliably distinguish multiple
Codex sessions in the same repo by itself. Add a `SessionStart` hook so `agent-switch` can bind
the Codex `session_id` to the current tmux or niri window.

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
    ]
  }
}
```

Notes:
- daemon should be running (`agent-switch serve` or `agent-switch serve --niri`)
- run Codex inside tmux if you want tmux window IDs captured
- on niri, `agent-switch track` also captures the currently focused window ID automatically
- you still want rollout watching: the hook binds identity, the rollout files provide live state

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
