# Development

## Prerequisites

- macOS arm64, Windows x64/ARM64, or Ubuntu 22.04 amd64/arm64
- Rust stable, Node.js 22, and npm
- Xcode command-line build tools on macOS
- Tauri's Linux build dependencies plus `libdbus-1-dev` on Ubuntu
- A Spotify Premium account and Spotify application client ID for Spotify paths

Configure the Spotify application's redirect URI to the loopback address shown
by Retune. Debug builds and `scripts/build-install.sh` use the app's development
token file; release builds encrypt tokens and use macOS Keychain for the
encryption key.

## Run

```sh
cd apps/desktop
npm ci
npm exec tauri dev
```

Package a production-like release with `npm exec tauri build` from
`apps/desktop`. On macOS, for local release-mode testing without repeated native
credential prompts, run `scripts/build-install.sh` from the repository root.

Native CI builds the Tauri app bundle on macOS arm64, Windows x64/ARM64, and
Ubuntu 22.04 amd64/arm64. The Windows and Linux jobs run release Rust tests,
including local-file import/playback tests, before building their native bundle;
those jobs are the cross-platform proof that release builds select persistent
native credential stores.

## Checks

Run the same checks used by CI:

```sh
node scripts/check-docs.mjs
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/desktop
npm ci
npm run test
npm run lint
npx tsc --noEmit -p tsconfig.app.json
npm run build
```

OAuth loopback tests need permission to bind a local socket in restricted test
environments. Tests requiring a real audio device are intentionally ignored by
default.

## Manual UI validation

Automated tests cover domain behavior and extracted UI logic, but native window,
drag, menu, scrolling, and audio behavior still need an app-level pass. For a
change in those areas:

1. Launch with `npm exec tauri dev`.
2. Exercise the smallest affected journey with both narrow and wide windows.
3. Check light and dark appearances when styling is involved.
4. Inspect the terminal and in-app error notices.
5. For Spotify changes, verify both signed-in success and disconnected/error UI.

Do not make CI depend on live Spotify credentials or an audio device.

## Documentation rule

`AGENTS.md` is the index, `ARCHITECTURE.md` is the system map, and
`docs/architecture/` contains current domain truth. If code changes an
architectural boundary, invariant, persistence format, or external contract,
update that document in the same commit. Delete completed plans after durable
decisions have been incorporated.
