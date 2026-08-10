# agent-switch

Track and switch between AI coding agent sessions (Claude, Codex, Pi, OpenCode) on niri.

## 1) How to execute tasks

- Prefer `just` recipes over raw commands.
- Primary dev loop: `just watch` (starts the detached Process Compose stack if needed).
- The watch stack requires host Wayland/niri access and must fail immediately when `SANDBOX=1`.

## 2) After code changes

- Do NOT run `cargo build` directly. Instead, ensure the watcher is running — it rebuilds automatically on file changes.
- Run `just check` after every code change (runs fmt, clippy, test).
- To inspect or control the watcher:
  - `just watch-status`
  - `just logs`
  - `just watch-stop`
- Lifecycle and sidebar-switching recipes require the host. Read-only status/log
  recipes are safe in a sandbox and may inspect the host-owned stack.

## Task Runner

```bash
just --list       # List all recipes
just build        # Build with release profile
just install      # Install to ~/.cargo/bin
just test         # Run tests
just clippy       # Lint
just fmt           # Format
just watch        # Start the detached Process Compose dev stack
just watch-stop   # Stop the complete dev stack
just watch-status # Show supervised process state
just logs         # Follow all supervised process logs
just demo         # Run overlay demo with mock data
```

## Architecture

Single binary with subcommands:

| Command | Description |
|---------|-------------|
| `track <event>` | Called by agent hooks, updates session state via daemon socket |
| `serve` | Run headless daemon (session cache + file watchers + Unix socket) |
| `serve --niri` | Daemon with the GTK agents overlay |
| `toggle` | Toggle the agents overlay (sends to running daemon) |
| `demo [--theme <name>]` | Show the overlay with mock data |
| `list` | Dump all sessions as JSON |
| `focused` | Dump the focused niri window's session as JSON |
| `cleanup` | Remove stale sessions |

App/workspace switching is NOT part of this project (handled externally by
nirius + xremap chords and niri-project-picker).

## Source Layout

```
src/
├── main.rs        # CLI (clap) dispatch
├── daemon.rs      # Daemon: socket server, file watchers, session cache, codex log parsing
├── state.rs       # Session store (load/save ~/.local/state/agent-switch/sessions.json)
├── track.rs       # Hook event handler (stdin JSON → daemon socket)
├── niri.rs        # GTK4 layer-shell agents overlay
├── config.rs      # config.toml loading (workspace ignore rules, theme)
└── themes.rs      # Overlay color themes
```

## State

Sessions stored in `~/.local/state/agent-switch/sessions.json`, keyed by niri window ID. Daemon communicates via Unix socket at `$AGENT_SWITCH_SOCKET` if set, otherwise `$XDG_RUNTIME_DIR/agent-switch/agent-switch.sock` (or `/tmp/agent-switch/agent-switch.sock`).

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

The `niri_id` is auto-detected via `niri msg -j windows` (focused window) unless overridden in the payload.

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
| `before_agent_start` | `prompt-submit` |
| `agent_settled` | `stop` |
| `session_switch` / `session_fork` | `session-end` (previous) + `session-start` (new) |

**Key behaviors:**
- Session ID derived from Pi's session file basename (falls back to `pi-ephemeral-<pid>-<timestamp>`)
- Includes `transcript_path` from `ctx.sessionManager.getSessionFile()` for file watching
- Auto-disables on first error with a one-time warning notification (no retries)
- Uses prompt-level `before_agent_start` and fully settled `agent_settled` events so automatic retries and compaction do not reset the timer
- 800ms timeout on `execFileSync` calls to avoid blocking the UI

## Dev Shell

`flake.nix` provides a dev shell with Process Compose and the GTK4/layer-shell system dependencies. Activated automatically via `.envrc`.

The repository-local `process-compose.yaml` owns the build watcher, niri daemon, and popup/dock sidebar process groups. Do not wrap this stack in zmx or add fallback `pkill` cleanup; Process Compose is the sole lifecycle owner.

`.envrc` exports the repository stack's `PC_SOCKET_PATH`, so ordinary commands
such as `process-compose process list`, `process-compose process logs niri`, and
`process-compose attach` find the detached supervisor without socket flags.
