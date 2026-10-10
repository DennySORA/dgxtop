# Local quality gates (same commands as CI; all builds use the committed Cargo.lock).
ci:
    cargo fmt --all -- --check
    cargo check --workspace --all-targets --all-features --locked
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --all-features --locked
    just boundaries
    python3 -m unittest discover -s scripts -p 'test_*.py'
    python3 scripts/check_validation.py

# Production dependency boundaries between workspace crates.
boundaries:
    mkdir -p target
    cargo metadata --format-version 1 --locked > target/metadata.json
    python3 spec/dgxtop-v2/tools/check_boundaries.py target/metadata.json

# Hardware suites (real procfs/sysfs, NVML, pidfd on disposable children).
hardware:
    cargo test --workspace --all-features --locked -- --ignored hardware_

# End-to-end suites, run on request (D-53): PTY sessions, fault injection
# and real adapter host processes.
e2e:
    cargo test --workspace --all-features --locked -- --ignored --skip hardware_

# Release build.
release:
    cargo build --release --locked
