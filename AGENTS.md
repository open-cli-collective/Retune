@/Users/rianjs/.codex/RTK.md

# Retune agent guide

Start with [ARCHITECTURE.md](ARCHITECTURE.md). Read only the domain document for
the behavior you are changing:

| Change | Read |
| --- | --- |
| Overlay model, browse projection, ratings, imports | [Library](docs/architecture/library.md) |
| Last.fm import matching, candidate ranking, ambiguity | [Import matching](docs/architecture/lastfm-import-matching.md) |
| OAuth, Spotify API, sync, playlists, rate limits | [Spotify](docs/architecture/spotify.md) |
| Queueing, backends, playback events, play counts | [Playback](docs/architecture/playback.md) |
| Files, tokens, backup/restore | [Persistence](docs/architecture/persistence.md) |
| Tauri shell, IPC, capabilities, lifecycle, platform config | [Tauri](docs/tauri.md) |
| Build, test, run, package, manual validation | [Development](docs/DEVELOPMENT.md) |
| Install, upgrade, uninstall, Spotify setup | [Installation](docs/INSTALL.md) |
| Completed SOLID audit evidence | [SOLID audit record](docs/SOLID.md) |
| Completed Rust audit evidence | [Rust audit record](docs/rust.md) |
| Current responsiveness work | [Responsiveness plan](docs/plans/responsiveness-burndown.md) |
| Responsiveness audit evidence | [Rust performance audit](docs/rust-slop-performance-audit.md), [Last.fm interaction audit](docs/lastfm-interaction-audit.md), [type-ahead audit](docs/typeahead-performance-audit.md) |

## Invariants

- `retune-core` stays deterministic and free of filesystem, network, async, UI,
  and Tauri concerns.
- Overlay metadata never writes to Spotify. Explicit content actions may.
- Spotify Web API requests go through the shared client and its request gate.
  OAuth token requests use the shared low-level transport outside that gate; do
  not add direct HTTP call paths.
- All playback backends emit neutral events. The controller/reducer owns queue
  advancement and UI-visible playback state.
- Local files remain usable while signed out of Spotify.
- Persistent app files are written atomically. Release OAuth tokens remain
  encrypted with their key in the platform credential store.

## Working agreement

1. Trace the affected flow through callers before editing.
2. Fix shared causes at the owning boundary and keep changes minimal.
3. Add the smallest regression test that proves non-trivial behavior.
4. Run the relevant checks in [Development](docs/DEVELOPMENT.md).
5. Update the owning architecture document in the same commit when a boundary,
   invariant, persisted format, or external API contract changes.

Plans are temporary working material. Move durable decisions into the current
architecture docs and delete the completed plan; Git is the history.

When changing Spotify behavior, verify the current official Spotify contract.
Do not encode assumptions from an old response or error message as policy.
