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
lastfm_env="$repo/.env.lastfm.local"
if [ -f "$lastfm_env" ]; then
  mode="$(stat -f '%Lp' "$lastfm_env")"
  if [ "$mode" != 600 ]; then
    chmod 600 "$lastfm_env"
  fi
  set -a
  # shellcheck disable=SC1090
  . "$lastfm_env"
  set +a
else
  echo "::warning::.env.lastfm.local is missing; this local build will not include Last.fm." >&2
fi
npx tauri build --bundles app --features dev-token-store
unset RETUNE_LASTFM_API_KEY RETUNE_LASTFM_SHARED_SECRET

# Seal this local-only bundle without using production signing credentials.
codesign --force --deep --sign - "$bundle"
codesign --verify --deep --strict "$bundle"
codesign -dv "$bundle" 2>&1 | grep -F "Signature=adhoc" >/dev/null \
  || { echo "Expected an ad-hoc signature on $bundle" >&2; exit 1; }

# Quit a running copy before overwriting it, then install fresh.
osascript -e 'quit app "Retune"' >/dev/null 2>&1 || true
rm -rf "$app"
ditto "$bundle" "$app"
open "$app"
echo "Installed and launched $app"
