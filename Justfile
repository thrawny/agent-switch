# agent-switch task runner

_niri := if os() == "linux" { "--features niri" } else { "" }

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
    zmx run agent-switch-build 'watchexec -w src -e rs --debounce 5000ms -- cargo build'
    zmx run agent-switch-tmux 'RUST_LOG=debug watchexec --restart --debounce 3000ms -w target/debug/agent-switch -- ./target/debug/agent-switch serve'
    zmx attach agent-switch-tmux

# Watch niri daemon with build-gated restart (old process stays alive on compile errors)
watch-niri:
    cargo build --features niri
    zmx run agent-switch-build 'watchexec -w src -e rs --debounce 5000ms -- cargo build --features niri'
    zmx run agent-switch-niri 'RUST_LOG=debug watchexec --restart --debounce 3000ms -w target/debug/agent-switch -- ./target/debug/agent-switch niri'
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

# Format code
fmt:
    cargo fmt
