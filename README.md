# Retune

Retune is a macOS desktop music library for people who prefer the dense,
album-first workflow of early iTunes. It combines a local metadata overlay,
local audio files, and a user's Spotify library behind one three-column browser.

Retune is a Tauri application with a React/TypeScript UI and a Rust backend.
The local overlay is authoritative for Retune-specific metadata such as ratings,
normalized genres, and play counts. Those edits are never written to Spotify.
Explicit content actions—saving albums, following artists, and editing owned
playlists—do use Spotify as the canonical store.

## What works

- Genre → Artist → Album browsing with a configurable, resizable track table.
- Local metadata edits, album/track rating inheritance, play counts, and search.
- Local audio import and playback.
- Spotify library sync, search, artist/album drill-down, and library writes.
- Owned-playlist creation, membership, deletion, and reordering.
- Built-in Spotify playback through librespot, with Spotify Connect as an
  optional alternative.
- JSON and gzip backup, restore, and additive merge.

Spotify does not expose the contents of playlists owned by other users to this
app. Retune can show their cached metadata and track counts, but cannot load or
edit their tracks.

## Requirements

- macOS
- Rust stable
- Node.js 22 and npm
- A Spotify Premium account and Spotify application client ID for Spotify
  features

The Spotify desktop app is required only when using the Spotify Connect playback
backend. Local files and built-in Spotify playback do not depend on it.

## Run locally

```sh
cd apps/desktop
npm ci
npm exec tauri dev
```

See [Development](docs/DEVELOPMENT.md) for validation, packaging, credentials,
and troubleshooting.

## Documentation

- [Architecture map](ARCHITECTURE.md)
- [Library domain](docs/architecture/library.md)
- [Spotify integration](docs/architecture/spotify.md)
- [Playback](docs/architecture/playback.md)
- [Persistence](docs/architecture/persistence.md)
- [Development and validation](docs/DEVELOPMENT.md)

The UI design files are visual references, not production architecture. Current
behavior in the application and the architecture documents above are the source
of truth.
