# Historical Context

Extracted from the `thrawny/dotfiles` repo (`rust/agent-switch/`) in Feb 2026.

## Origin

Originally built as a workspace member in `dotfiles/rust/` alongside `bash-validator` and `voice`. Shared workspace deps (serde, dirs, log, env_logger) and release profile (opt-level=z, lto, strip). The daemon refactor consolidated the legacy `tmux-fzf-switcher` crate into agent-switch by adding the `serve` command and socket-based `list` protocol.

## Design Decisions

**Window ID as primary key**: Sessions are keyed by window ID (tmux `@N` or niri numeric ID), not session_id. This means one session per window. If a user starts a new agent in the same window, the old session is replaced.

**Window ID capture timing**: Window ID is only captured on `session-start` and `prompt-submit` (when session not found). Other events (`stop`, `notification`) look up by `session_id`, never re-query window. This avoids mis-registration when the agent works in background while the user focuses another window. Known race condition: ~100ms between session start and niri query where user could switch windows. Accepted tradeoff — use `fix` command to recover.

**Daemon vs daemonless**: The tmux picker (`tmux` subcommand) was originally daemonless — loaded state file directly. After adding Codex support (which requires parsing JSONL rollout files), the daemon became necessary for performance. The tmux picker now queries the daemon via `list` socket command. Falls back to direct file read (without Codex) if daemon isn't running.

**Codex session detection**: Codex doesn't have hooks like Claude. Sessions are discovered by scanning `~/.codex/sessions/` for `rollout-*.jsonl` files. State is inferred from record types: `user_message` → responding, `agent_message` → idle, `function_call`/`reasoning` → responding. A stale timeout (10s with no updates while "responding") marks state as unknown. If the last assistant message ends with `?`, state is set to "waiting".

**Codex deduplication**: Multiple rollout files can exist per cwd. Only the most recently modified file per cwd is processed. Sessions older than 7 days are ignored.

**Headless daemon lifecycle**: Without niri, the daemon monitors tmux sockets at `/tmp/tmux-$UID/`. When no sockets exist (tmux server died), daemon exits. This is only for lifecycle — session tracking is compositor-agnostic.

**`serve --niri` layering**: The niri GTK overlay layers on top of the core daemon. Same socket, same cache, same file watchers. Adds: GTK4 layer-shell window, niri focus tracking, keyboard-driven overlay UI. The `niri` subcommand is deprecated in favor of `serve --niri`.

## Integration with dotfiles

The binary is installed to `~/.cargo/bin/agent-switch` (on PATH via Home Manager `sessionPath`). Referenced by:

- **tmux.nix**: Ctrl+backtick (Mac) / Ctrl+< (Linux) opens `agent-switch tmux` picker. Tmux startup spawns `agent-switch serve &` if not running.
- **niri/switcher.nix**: Startup runs `agent-switch niri`. Mod+S toggles overlay via `agent-switch niri --toggle`.
- **Claude hooks**: `settings.example.json` registers SessionStart/SessionEnd/Stop/Notification/UserPromptSubmit hooks calling `agent-switch track <event>`.
- **Headless servers**: Hooks are stripped from seeded config (`nix/home/nixos/headless.nix`) because the binary isn't built there.
- **niri/default.nix**: Layer shell rule for GTK window: `{ app-id = "^com\\.thrawny\\.agent-switch$"; }`.

## Pending / Future Work

- `fix` command is stubbed (`todo!`). Should: query focused window, find orphan session, re-associate.
- macOS mode: daemonless, similar to tmux but queries Terminal.app or iTerm windows.
- Nix flake packaging: add crane build + overlay so dotfiles can consume as a flake input (see extraction plan below).

## Extraction Plan (from dotfiles docs)

Two options for Nix integration with the dotfiles flake:

**Option A — Flake with packages (recommended)**: This repo exports its own packages via crane. Dotfiles adds `inputs.agent-switch.url = "github:thrawny/agent-switch"`.

**Option B — Source-only input**: Keep build logic in dotfiles, reference this repo as a non-flake input: `inputs.agent-switch-src = { url = "..."; flake = false; }`.

Local dev override: `nix build --override-input agent-switch path:/home/thrawny/code/agent-switch`.
