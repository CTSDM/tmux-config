#!/usr/bin/env bash
# agentd/check.sh: what must pass before handing over (design.md, Rules 7 and 8).
set -euo pipefail
cd "${BASH_SOURCE[0]%/*}"
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo deny --locked check --hide-inclusion-graph
