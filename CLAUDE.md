# agent-switch

Track and switch between AI coding agent sessions (Claude, Codex, OpenCode) across tmux and niri.

## 1) How to execute tasks

- Prefer `just` recipes over raw commands.
- Primary dev loop: `just watch-niri` or `just watch-tmux` (runs in zmx).

## 2) After code changes

- Do NOT run `cargo build` directly. Instead, ensure the watcher is running — it rebuilds automatically on file changes.
- Run `just check` after every code change (runs fmt, clippy, test).
- To check build output / runtime logs from the watcher:
  - `zmx list --short`
  - `zmx history agent-switch-build | tail -n 200` (build watcher)
  - `zmx history agent-switch-niri | tail -n 200` (niri daemon)
  - `zmx history agent-switch-tmux | tail -n 200` (tmux daemon)

## Task Runner

```bash
just              # List all recipes
just build        # Build with release profile (includes niri on Linux)
just install      # Install to ~/.cargo/bin
just test         # Run tests
just clippy       # Lint
just fmt           # Format
just watch-tmux   # Watch + run tmux daemon (zmx session)
just watch-niri   # Watch + run niri GTK daemon (zmx session)
```

## Architecture

Single binary with subcommands:

| Command | Description |
|---------|-------------|
| `track <event>` | Called by agent hooks, updates session state via daemon socket |
| `serve` | Run daemon (session cache + file watchers + Unix socket) |
| `serve --niri` | Daemon with niri GTK overlay (requires `niri` feature) |
| `tmux` | Daemonless tmux picker (fzf-based) |
| `list` | Dump all sessions as JSON |
| `cleanup` | Remove stale sessions |

## Source Layout

```
src/
├── main.rs     # CLI (clap) dispatch
├── daemon.rs   # Daemon: socket server, file watchers, session cache, codex log parsing
├── state.rs    # Session store (load/save ~/.local/state/agent-switch/sessions.json)
├── track.rs    # Hook event handler (stdin JSON → daemon socket)
├── tmux.rs     # Tmux picker UI (fzf)
└── niri.rs     # GTK4 layer-shell overlay for niri (behind `niri` feature)
```

## Features

- `niri` — GTK4 layer-shell overlay for the niri compositor. Linux only. Adds deps: gtk4, gtk4-layer-shell, niri-ipc, toml, shellexpand.

## State

Sessions stored in `~/.local/state/agent-switch/sessions.json`, keyed by window ID (tmux or niri). Daemon communicates via Unix socket at `$XDG_RUNTIME_DIR/agent-switch.sock` (or `/tmp/agent-switch.sock`).

## Hook Integration

Agents call `agent-switch track <event> --agent <name>` with JSON on stdin. Events: `session-start`, `session-end`, `prompt-submit`, `stop`, `notification`. The `--agent` flag is required when the JSON payload doesn't include an `agent` field (e.g. Claude hooks). The track command forwards to the daemon socket; falls back to direct file I/O if no daemon.

The JSON payload should include `transcript_path` (the session file) so the daemon can watch it for activity and keep `state_updated` current even if hook events are missed.

### JSON Payload

```json
{
  "session_id": "required — unique session identifier",
  "agent": "optional if --agent flag used (claude, codex, pi, opencode)",
  "cwd": "optional — working directory",
  "transcript_path": "optional — session file path for activity watching",
  "notification_type": "optional — e.g. permission_prompt (for notification events)",
  "niri_id": "optional — niri window ID override (auto-detected if omitted)"
}
```

The `tmux_id` is auto-detected from `$TMUX_PANE` / `tmux display-message`. The `niri_id` is auto-detected via `niri msg -j windows` (focused window) unless overridden in the payload.

### Claude Code Hooks

Configured in `~/.claude/settings.json` under `hooks`. Claude hooks pass the event JSON on stdin. The `--agent claude` flag supplies the agent name since Claude's hook payload doesn't include it.

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "agent-switch track session-start --agent claude" }] }],
    "SessionEnd":   [{ "hooks": [{ "type": "command", "command": "agent-switch track session-end --agent claude" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "agent-switch track prompt-submit --agent claude" }] }],
    "Stop":         [{ "hooks": [{ "type": "command", "command": "agent-switch track stop --agent claude" }] }],
    "Notification": [{ "matcher": "permission_prompt", "hooks": [{ "type": "command", "command": "agent-switch track notification --agent claude" }] }]
  }
}
```

### Codex Hooks

Configured in `~/.config/codex/hooks.json`. Commands are wrapped in `sh -lc` with an existence check so they fail silently if `agent-switch` isn't installed.

```json
{
  "hooks": {
    "SessionStart":     [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh -lc 'if command -v agent-switch >/dev/null 2>&1; then exec agent-switch track session-start --agent codex; fi'", "timeout": 5 }] }],
    "UserPromptSubmit": [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh -lc 'if command -v agent-switch >/dev/null 2>&1; then exec agent-switch track prompt-submit --agent codex; fi'", "timeout": 5 }] }],
    "Stop":             [{ "matcher": "", "hooks": [{ "type": "command", "command": "sh -lc 'if command -v agent-switch >/dev/null 2>&1; then exec agent-switch track stop --agent codex; fi'", "timeout": 5 }] }]
  }
}
```

### Pi Extension

Pi uses a TypeScript extension (`agent-switch.ts`) rather than shell hooks. The extension is installed at `~/.pi/agent/extensions/agent-switch.ts` (symlinked from `~/dotfiles/config/pi/extensions/agent-switch.ts`).

**Event mapping:**

| Pi event | agent-switch event |
|---|---|
| `session_start` | `session-start` |
| `session_shutdown` | `session-end` |
| `agent_start` | `prompt-submit` |
| `agent_end` | `stop` |
| `session_switch` / `session_fork` | `session-end` (previous) + `session-start` (new) |

**Key behaviors:**
- Session ID derived from Pi's session file basename (falls back to `pi-ephemeral-<pid>-<timestamp>`)
- Includes `transcript_path` from `ctx.sessionManager.getSessionFile()` for file watching
- Auto-disables on first error with a one-time warning notification (no retries)
- 800ms timeout on `execFileSync` calls to avoid blocking the UI

## Dev Shell

`flake.nix` provides a dev shell with GTK4/layer-shell system dependencies needed for the `niri` feature. Activated automatically via `.envrc`.
