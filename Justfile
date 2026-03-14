# agent-switch task runner

_niri := if os() == "linux" { "--features niri" } else { "" }
_build_stamp := "target/debug/agent-switch-built.stamp"

# Default recipe
default:
    @just --list

# Build
build:
    cargo build --release {{ _niri }}

# Run the tmux daemon
run-tmux:
    cargo run -- serve

# Run the niri daemon
run-niri:
    cargo run --features niri -- niri

# Watch tmux daemon with build-gated restart (old process stays alive on compile errors)
watch-tmux:
    cargo build
    touch {{ _build_stamp }}
    zmx run agent-switch-build "watchexec -w src -w Cargo.toml -e rs --debounce 5s --on-busy-update queue -- 'cargo build && touch {{ _build_stamp }}'"
    zmx run agent-switch-tmux 'RUST_LOG=debug watchexec --restart --debounce 250ms -w {{ _build_stamp }} -- ./target/debug/agent-switch serve'
    zmx attach agent-switch-tmux

# Watch niri daemon with build-gated restart (old process stays alive on compile errors)
watch-niri:
    cargo build --features niri
    touch {{ _build_stamp }}
    zmx run agent-switch-build "watchexec -w src -w Cargo.toml -e rs --debounce 5s --on-busy-update queue -- 'cargo build --features niri && touch {{ _build_stamp }}'"
    zmx run agent-switch-niri 'RUST_LOG=debug watchexec --restart --debounce 250ms -w {{ _build_stamp }} -- ./target/debug/agent-switch niri'
    zmx attach agent-switch-niri

# Install to ~/.cargo/bin
install:
    cargo install --path . --locked --force {{ _niri }}

# Run all post-change checks
check:
    just fmt
    cargo clippy {{ _niri }} -- -D warnings
    just test

# Run clippy
clippy:
    cargo clippy --fix --allow-dirty --allow-staged --release {{ _niri }}

# Run tests
test:
    cargo test --release

# Run niri overlay demo with mock data (optional theme: just demo default)
demo theme="":
    cargo run --features niri -- niri --demo {{ if theme != "" { "--theme " + theme } else { "" } }}

# Format code
fmt:
    cargo fmt
