#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOT_RUST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SOURCE_ENV_PATH="$BOT_RUST_DIR/.env.example"
TARGET_ENV_PATH="$BOT_RUST_DIR/.env"

if [[ ! -f "$SOURCE_ENV_PATH" ]]; then
  echo "Source template not found: $SOURCE_ENV_PATH" >&2
  exit 1
fi

if [[ -f "$TARGET_ENV_PATH" ]]; then
  echo "Target env already exists: $TARGET_ENV_PATH" >&2
  exit 1
fi

cp "$SOURCE_ENV_PATH" "$TARGET_ENV_PATH"
echo "Copied $SOURCE_ENV_PATH -> $TARGET_ENV_PATH"
echo "For RELAYER_SAFE mode, run: cd \"$BOT_RUST_DIR\" && npm install"
