# agent-switch task runner

_build_stamp := "target/debug/agent-switch-built.stamp"

# Default recipe
default:
    @just --list

# Build
build:
    cargo build --release

# Run the niri daemon
run-niri:
    cargo run -- serve --niri

# Watch daemon + sidebar with build-gated restart (old processes stay alive on compile errors)
watch:
    -zmx kill agent-switch-build
    -zmx kill agent-switch-sidebar
    if [ "$${ZMX_SESSION:-}" != "agent-switch-niri" ]; then zmx kill agent-switch-niri || true; fi
    sleep 0.2
    cargo build
    touch {{ _build_stamp }}
    zmx run agent-switch-build -d watchexec --postpone -w src -w Cargo.toml -e rs --debounce 5s --on-busy-update queue -- 'cargo build && touch {{ _build_stamp }}'
    zmx run agent-switch-sidebar -d watchexec --restart --debounce 250ms -w {{ _build_stamp }} -- ./target/debug/agent-switch demo-sidebar --live
    sleep 0.2
    if [ "$${ZMX_SESSION:-}" = "agent-switch-niri" ]; then env RUST_LOG=debug watchexec --restart --debounce 250ms -w {{ _build_stamp }} -- ./target/debug/agent-switch serve --niri; else zmx attach agent-switch-niri env RUST_LOG=debug watchexec --restart --debounce 250ms -w {{ _build_stamp }} -- ./target/debug/agent-switch serve --niri; fi

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

# PROTOTYPE: ticket-08 live sidebar daemon in zmx; Mod+S toggles it.
# `just watch` runs this under the same build-stamp restart loop — use this
# recipe only to (re)start the sidebar without the full watch stack.
# Logs: zmx history agent-switch-sidebar
demo-sidebar-live:
    cargo build
    -zmx kill agent-switch-sidebar
    zmx run agent-switch-sidebar -d watchexec --restart --debounce 250ms -w {{ _build_stamp }} -- ./target/debug/agent-switch demo-sidebar --live

# Format code
fmt:
    cargo fmt
