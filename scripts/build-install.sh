#!/usr/bin/env bash
# Build the release bundle, install it to /Applications, and launch it.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
bundle="$repo/target/release/bundle/macos/Retune.app"
app="/Applications/Retune.app"

export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

cd "$repo/apps/desktop"
npx tauri build

# Quit a running copy before overwriting it, then install fresh.
osascript -e 'quit app "Retune"' >/dev/null 2>&1 || true
rm -rf "$app"
ditto "$bundle" "$app"
open "$app"
echo "Installed and launched $app"
