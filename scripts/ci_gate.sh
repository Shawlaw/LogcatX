#!/usr/bin/env bash
# The single source of truth for the push gate: .github/workflows/ci.yml
# invokes this exact script, so "green locally" and "green on GitHub" can
# never drift apart. Run it before every push to master.
set -euo pipefail

cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --quiet
