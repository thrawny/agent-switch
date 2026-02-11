# agent-switch

Track and switch between AI coding agent sessions (Claude, Codex, OpenCode) across **tmux** and **niri**.

## Build / install

```bash
just build
just install
```

This installs `agent-switch` to `~/.cargo/bin/agent-switch`.

---

## Run in tmux

### 1) Start the daemon

```bash
agent-switch serve
```

(Usually run this in the background from tmux startup.)

### 2) Open the tmux picker

```bash
agent-switch tmux
```

- 2-key mode: first key picks session, second key picks window
- `/` switches to fzf search
- `q` or `Esc` cancels

Direct fzf mode:

```bash
agent-switch tmux --fzf
```

### Optional tmux keybinding

```tmux
bind-key -n C-` display-popup -E -w 60% -h 60% "agent-switch tmux"
```

---

## Run in niri (Linux)

The niri overlay needs the `niri` Cargo feature.

### 1) Start daemon + GTK overlay service

```bash
agent-switch serve --niri
```

If running from source:

```bash
cargo run --features niri -- serve --niri
```

### 2) Toggle the overlay

```bash
agent-switch niri --toggle
```

(Recommended to bind this in niri config.)

### Optional niri keybinding

```kdl
Mod+S { spawn "agent-switch" "niri" "--toggle"; }
```

### Optional startup entry

```kdl
spawn-at-startup "agent-switch" "serve" "--niri"
```

---

## Agent hook integration (for live state)

To populate session state (waiting/working/idle), configure your agent hooks to call:

- `agent-switch track session-start`
- `agent-switch track prompt-submit`
- `agent-switch track stop`
- `agent-switch track notification`
- `agent-switch track session-end`

Without hooks, window switching still works, but status labels will be limited.

---

## Useful commands

```bash
agent-switch list      # print sessions as JSON
agent-switch cleanup   # remove stale sessions
```

Socket path: `$XDG_RUNTIME_DIR/agent-switch.sock` (fallback: `/tmp/agent-switch.sock`).

---

## License

MIT — see [`LICENSE`](./LICENSE).
