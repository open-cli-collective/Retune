# Tauri desktop shell

This document is the current architecture contract for Retune's Tauri shell.
It covers webview authority, IPC and event delivery, native integration,
lifecycle ownership, platform configuration, and distribution trust. Domain
behavior belongs in the corresponding document under `docs/architecture/`.

The repository-wide numbered Tauri audit was completed and retired after its
durable decisions were incorporated here and in the code. Git history retains
the finding-by-finding evidence. New defects belong in code review or a new
time-bounded audit, not in a permanent completed-findings ledger.

## Boundary and invariants

- `retune-core`, provider adapters, and audio backends do not depend on Tauri.
- The Rust shell owns native windows, dialogs, drag-and-drop, external URL
  opening, filesystem provenance, persistent stores, credential stores, and
  process lifecycle.
- React routing is presentation, not authorization. Tauri capabilities and
  application-command permissions are the webview authority boundary.
- JavaScript never supplies an arbitrary local path to a privileged command.
  File paths enter through a Rust-owned native dialog or native drop event and
  are validated before use.
- Remote content is never navigated inside a Retune webview. The shell maps a
  closed external-destination enum to validated HTTPS URLs and opens them with
  the native opener.
- Core event payloads are safe for every webview permitted to listen. Private
  main-window state travels through the main-only command channel.
- Potentially blocking filesystem, serialization, credential-store, and
  network work does not run on the async executor thread.

## Windows and authority

Retune has two webview labels:

| Window | Purpose | Authority |
| --- | --- | --- |
| `main` | Library, settings, Spotify, playback, diagnostics, and import launch | `main-commands`, event listen/unlisten, and title mutation |
| `lastfm-importer` | Last.fm import review and application | `importer-commands` and event listen/unlisten |

`tauri.conf.json` activates exactly `default` and `lastfm-importer`; adding a
capability file does not silently enable it. The capability files name one
window apiece and avoid aggregate `core:default`, opener defaults, frontend
event emission, image-from-path, and reveal-in-directory permissions. Plugins
may be registered for Rust use without granting their JavaScript permissions.

`build.rs` declares every application command in a Tauri `AppManifest`.
`generate_handler!`, generated command permissions, `main.toml`, and
`importer.toml` must remain in parity. `scripts/check-tauri-acl.mjs` enforces
that parity, the exact window capability sets, the importer allow-list, the
absence of raw frontend path import, and the absence of sensitive core-event
payloads.

The importer receives the narrow appearance and genre queries it needs. It
does not receive full settings, Spotify connection or mutation, playback,
diagnostics, local import, or main-event subscription authority. Shared
commands still validate their resource and state inputs; the ACL answers only
which window may call them.

## IPC and events

Frontend calls are centralized in `apps/desktop/src/ipc.ts`; components do not
import the raw Tauri core invoke API. Commands use serializable DTOs for bounded
request/response work. Artwork uses a raw response path with canonical library
membership validation and an 8 MiB bound rather than JSON-encoded bytes.

The core event bus carries targeted, payload-free invalidations such as library,
playlist, connection, settings, appearance, and import-state changes. A
consumer responds by fetching an authorized snapshot. Main-only player state,
startup notices, operation errors, and local-import results use the private
main channel. Consumers subscribe before taking their initial snapshot so a
change cannot fall between snapshot and subscription.

Native menu and media-key actions enter the same Rust-owned application paths
as UI actions. Playback backends emit neutral events; the controller/reducer
owns queue advancement and visible playback state.

## Native integration

- The single-instance plugin is registered first on desktop. A second launch
  shows, unminimizes, and focuses the existing main window.
- Window-state persistence records size, position, and maximized state for the
  main window only. The transient importer is denied.
- Native dialogs perform backup selection, restore selection, local-file
  selection, and user-facing confirmations. Rust receives their results.
- Native file-drop events are accepted only for the intended window and are
  normalized through the same local-import boundary as dialog selections.
- External navigation uses the closed `ExternalDestination` command contract;
  no arbitrary frontend URL reaches the opener plugin.
- OAuth loopback servers, menu resources, global media controls, playback
  subscriptions, and importer tasks have explicit owners and shutdown paths.

## Runtime and lifecycle

Startup creates the stores and performs crash recovery before publishing
authoritative in-memory state. Long hydration and follow-up connection work is
owned by spawned tasks; startup notices are published through the main
channel. Persistent mutations use their owning gate and detached completion so
caller cancellation cannot leave durable state newer than an authoritative
cache. Related multi-file changes use recovery journals where an atomic file
replacement alone cannot express the logical transaction.

Filesystem serialization, writes, directory synchronization, credential-store
calls, and other blocking native work use Tokio's blocking pool. Mutexes are
held for short snapshots or publication, not across network calls or blocking
I/O. Network operations retain revision or identity checks before publishing
results so stale completion cannot replace current state.

Synchronous Tauri setup enters owned async work through
`tauri::async_runtime`; it must not call `tokio::spawn` or
`tokio::task::spawn_blocking` before an async runtime task has been entered.
Direct Tokio spawning is valid only from code already running inside that
runtime. This keeps packaged startup independent of incidental test or
development runtime context.

On exit, Retune prevents the first exit request while it cancels retry work,
drains playback persistence with a bounded timeout, shuts down Last.fm-owned
tasks, and flushes the Spotify catalog on the blocking pool. Later exit events
cannot start a second drain. Resume invalidates local playback resources so
device changes are observed.

## Webview and platform configuration

The webview loads only bundled content. The CSP denies base, object, frame, and
form destinations; permits scripts from self; permits artwork from self,
`https://i.scdn.co`, and data URLs; and limits connections to Tauri IPC. Adding
a remote origin requires an explicit architecture review.

Vite binds `127.0.0.1` on strict port `5173` and chooses the frontend target
appropriate to the native webview floor. Tauri's development URL must remain
identical. Platform overlays define the supported bundle contract:

| Platform | Bundle | Minimum |
| --- | --- | --- |
| macOS arm64 | `.app` inside a `ditto`-created ZIP | macOS 11 |
| Windows x64/ARM64 | NSIS | WebView2 105; downgrade disabled |
| Ubuntu amd64/arm64 | Debian package | Ubuntu 22.04 / WebKitGTK 4.1 |

The Windows application manifest is an explicit build input and `build.rs`
emits `cargo:rerun-if-changed` for it. Tauri's build helper owns capability,
permission, icon, and configuration inputs. `scripts/check-release.mjs`
enforces platform target and packaging parity.

## Production distribution trust

Local release-mode bundles are development artifacts. Only the release
workflow produces distributable artifacts.

On macOS, CI imports a `Developer ID Application` certificate and gives Tauri
the signing identity plus App Store Connect API credentials. Tauri signs with
the hardened runtime and secure timestamp, submits the app for Apple
notarization, waits for acceptance, and staples the ticket. Before packaging,
CI verifies the executable and app signatures, expected team identifier,
Developer ID authority, runtime flag, timestamp, stapled ticket, and Gatekeeper
assessment. The base Tauri configuration must not force an ad-hoc identity.
CI creates the distributable ZIP with Apple's `ditto -c -k --keepParent`
contract, extracts it with `ditto`, and repeats code-signature, stapled-ticket,
and Gatekeeper verification against the extracted app.

On Windows, supported x64 CI runners cross-build the explicit x64 and ARM64
MSVC targets. Tauri owns the complete Microsoft Artifact Signing lifecycle and
invokes a pinned Artifact Signing CLI through object-form
`bundle.windows.signCommand`. Tauri patches its bundle marker before signing
the shipped `retune-desktop.exe`, then
signs the NSIS uninstaller and outer installer through the same command. This
ordering prevents bundling from invalidating an already-signed payload. The
command explicitly requests SHA-256 file and timestamp digests plus Microsoft's
RFC 3161 timestamp service rather than depending on client defaults.
Verification requires `Get-AuthenticodeSignature` status `Valid`,
the configured signer subject, a timestamp certificate, and a successful
SignTool policy check.

Signing actions are pinned to immutable commits. Certificate material, API
credentials, and publication tokens exist only as secrets in the protected
GitHub Actions environment named `release`. Its deployment policy allows only
`main` and `v*` refs and requires maintainer review. Every job that consumes
those credentials declares that environment. The Apple API key is decoded to
an owner-only runner-temporary file and removed even when the job fails.
Release and manual release dry-run jobs fail before packaging when required
trust credentials are absent. The release-writing aggregate job uses the same
protected environment and waits for native Windows install verification before
publication. Linux package dispatch may remain optional; signing and
notarization may not.

The credential names and maintainer setup are documented in
`docs/DEVELOPMENT.md`. User-visible verification and install behavior are
documented in `docs/INSTALL.md`.

## Verification contract

The deterministic repository gates are:

```sh
node scripts/check-tauri-acl.mjs
node scripts/check-release.mjs
node scripts/check-docs.mjs
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/desktop
npm ci
npx tauri permission list
npm run test
npm run lint
npm run build
```

Static gates prove configuration, command/permission parity, source-level
ownership rules, workflow structure, and deterministic application behavior.
They do not prove native platform integration or possession of valid signing
credentials.

Before release, run the Release workflow by `workflow_dispatch` on the exact
candidate commit after it is reachable from `main`. The prepare job rejects an
off-main ref before any build job can access signing credentials. Its five
build jobs must produce the declared asset set. The macOS job must complete
live Apple notarization and pass `codesign`, `stapler`, and Gatekeeper
verification. Both Windows target builds must pass live Artifact Signing,
verify the post-patch payload and installer, and then pass a downstream silent
install check on native x64 and ARM64 Windows runners. That check verifies the
outer installer, installed payload, signer, timestamp, SignTool policy, and PE
machine type. It deliberately does not launch the GUI: headless hosted runners
cannot provide reliable evidence for a tray/window application, OAuth, media
controls, or audio devices. No release may claim live trust proof from a
credential-free local run.

Native smoke validation remains manual: launch the packaged application on the
oldest supported macOS, Windows x64, Windows ARM64, Ubuntu amd64, and Ubuntu
arm64 environments; verify first launch, single-instance focusing, main-window
state restoration, importer non-restoration, native dialogs and drops,
external links, OAuth callbacks, media controls, graceful shutdown, and local
file use while signed out. Record the candidate version and environments with
the release evidence.

## Completion policy

An audit finding is complete only when its owning boundary is corrected, the
smallest deterministic regression proof exists, relevant package checks pass,
and any required native/manual evidence is identified honestly. Once complete,
move durable decisions into this document or the owning domain architecture
document and retire the finding text. Do not retain stale severity counts,
line-number evidence, or accepted-deviation language as current architecture.
