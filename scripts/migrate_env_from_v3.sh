#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOT_RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
V3_ENV_PATH="$BOT_RUST_DIR/../bot v3.0/.env"
TARGET_ENV_PATH="$BOT_RUST_DIR/.env"

if [[ ! -f "$V3_ENV_PATH" ]]; then
  echo "Source env not found: $V3_ENV_PATH" >&2
  exit 1
fi

if [[ -f "$TARGET_ENV_PATH" ]]; then
  echo "Target env already exists: $TARGET_ENV_PATH" >&2
  exit 1
fi

cp "$V3_ENV_PATH" "$TARGET_ENV_PATH"
echo "Copied $V3_ENV_PATH -> $TARGET_ENV_PATH"
