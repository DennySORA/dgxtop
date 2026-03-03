ci:
    cargo build
    cargo check --workspace --all-targets --all-features
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
