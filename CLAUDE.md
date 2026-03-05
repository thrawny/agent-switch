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

Agents call `agent-switch track <event>` with JSON on stdin. Events: `session-start`, `session-end`, `prompt-submit`, `stop`, `notification`. The track command forwards to the daemon socket; falls back to direct file I/O if no daemon.

## Dev Shell

`flake.nix` provides a dev shell with GTK4/layer-shell system dependencies needed for the `niri` feature. Activated automatically via `.envrc`.
