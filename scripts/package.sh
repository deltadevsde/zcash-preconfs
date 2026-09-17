#!/usr/bin/env bash
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
archive="${1:-/tmp/preconf-hosting.tar.gz}"
tar -czf "$archive" -C "$root" \
  --exclude='.git' --exclude='target' --exclude='.agents' --exclude='.codex' \
  --exclude='__pycache__' --exclude='preconf/runs' --exclude='preconf/data' \
  --exclude='preconf/.env' \
  preconf
printf 'Created %s (Zakura is pinned to GitHub)\n' "$archive"
