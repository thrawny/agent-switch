# agent-switch task runner

_pc_runtime_dir := env("XDG_RUNTIME_DIR", "/tmp") + "/agent-switch"
_pc_socket := _pc_runtime_dir + "/process-compose.sock"
_pc_log := _pc_runtime_dir + "/process-compose.log"
export PC_SOCKET_PATH := _pc_socket

# Default recipe
default:
    @echo "Run 'just --list' to see available recipes."

# Build
build:
    cargo build --release

# Run the niri daemon
run-niri:
    cargo run -- serve --niri

# Fail before touching a host-owned stack from inside an agent sandbox.
[private]
_require-host:
    @if [ "${SANDBOX:-0}" = "1" ]; then echo "agent-switch watch requires host Wayland/niri access; SANDBOX=1 is unsupported" >&2; exit 1; fi

# Start the Process Compose supervisor in the background if it is not running.
watch-start: _require-host
    #!/usr/bin/env bash
    set -euo pipefail
    socket="{{ _pc_socket }}"
    mkdir -p "{{ _pc_runtime_dir }}"
    if [[ -S "$socket" ]]; then
        if process-compose process list >/dev/null 2>&1; then
            exit 0
        fi
        rm -f "$socket"
    fi
    process-compose \
        --ordered-shutdown \
        --log-file "{{ _pc_log }}" \
        up --config process-compose.yaml --detached

# Start the detached build-gated stack if needed.
watch: watch-start

# Stop the complete stack and all of its process groups.
watch-stop: _require-host
    #!/usr/bin/env bash
    set -euo pipefail
    socket="{{ _pc_socket }}"
    if [[ ! -S "$socket" ]]; then
        exit 0
    fi
    if process-compose process list >/dev/null 2>&1; then
        process-compose down
    fi
    rm -f "$socket"

# Restart the stack and open its TUI.
watch-restart: watch-stop watch

# Show process state without opening the TUI. This read-only command is sandbox-safe.
watch-status:
    process-compose process list

# Follow recent logs from every process. This read-only command is sandbox-safe.
logs:
    process-compose process logs --namespace default --tail 200 --follow

# Install to ~/.cargo/bin
install:
    cargo install --path . --locked --force

# Run all post-change checks
check: fmt clippy test

# Clippy with denied warnings
_clippy-strict:
    cargo clippy -- -D warnings

# Run clippy and apply machine-applicable fixes
clippy:
    cargo clippy --fix --allow-dirty --allow-staged

# Run tests
test:
    cargo test

# Run overlay demo with mock data (optional theme: just demo default)
demo theme="":
    cargo run -- demo {{ if theme != "" { "--theme " + theme } else { "" } }}

# PROTOTYPE: ticket-06 sidebar demo with mock data (throwaway)
demo-sidebar:
    cargo run -- demo-sidebar

# PROTOTYPE: switch the supervised live sidebar between popup and dock modes.
demo-sidebar-live mode="popup": watch-start
    #!/usr/bin/env bash
    set -euo pipefail
    pc=(process-compose process)
    if [[ "{{ mode }}" == "dock" ]]; then
        "${pc[@]}" stop sidebar || true
        "${pc[@]}" start sidebar-dock
    else
        "${pc[@]}" stop sidebar-dock || true
        "${pc[@]}" start sidebar
    fi

# Format code
fmt:
    cargo fmt
