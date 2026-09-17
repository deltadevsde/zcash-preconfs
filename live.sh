#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
cargo build --release --locked
exec python3 scripts/live.py "$@"
