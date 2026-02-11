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

# Watch tmux daemon (rebuild on changes)
watch-tmux:
    RUST_LOG=debug watchexec -w src -e rs --restart -- cargo run -- serve

# Watch niri daemon (rebuild on changes)
watch-niri:
    RUST_LOG=debug watchexec -w src -e rs --restart -- cargo run --features niri -- niri

# Install to ~/.cargo/bin
install:
    cargo install --path . --locked --force {{ _niri }}

# Run clippy
clippy:
    cargo clippy --fix --allow-dirty --allow-staged --release {{ _niri }}

# Run tests
test:
    cargo test --release

# Format code
fmt:
    cargo fmt
