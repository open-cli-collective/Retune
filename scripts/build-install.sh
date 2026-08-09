#!/usr/bin/env bash
# Build the release bundle, install it to /Applications, and launch it.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
bundle="$repo/target/release/bundle/macos/Retune.app"
app="/Applications/Retune.app"

export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

cd "$repo/apps/desktop"
# --bundles app: skip the DMG step (slow, drives Finder via AppleScript,
# and we install straight into /Applications anyway).
npx tauri build --bundles app --features dev-token-store

# Tauri ad-hoc signs the bundle (bundle.macOS.signingIdentity is "-" in
# tauri.conf.json — same as release builds). Fail loudly if it didn't.
codesign --verify --deep --strict "$bundle"
codesign -dv "$bundle" 2>&1 | grep -q "Signature=adhoc" \
  || { echo "Expected an ad-hoc signature on $bundle" >&2; exit 1; }

# Quit a running copy before overwriting it, then install fresh.
osascript -e 'quit app "Retune"' >/dev/null 2>&1 || true
rm -rf "$app"
ditto "$bundle" "$app"
open "$app"
echo "Installed and launched $app"
