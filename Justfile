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

# Watch daemon with build-gated restart (old process stays alive on compile errors)
watch:
    -zmx kill agent-switch-build
    if [ "$${ZMX_SESSION:-}" != "agent-switch-niri" ]; then zmx kill agent-switch-niri || true; fi
    sleep 0.2
    cargo build
    touch {{ _build_stamp }}
    zmx run agent-switch-build -d watchexec --postpone -w src -w Cargo.toml -e rs --debounce 5s --on-busy-update queue -- 'cargo build && touch {{ _build_stamp }}'
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

# Format code
fmt:
    cargo fmt
