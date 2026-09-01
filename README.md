# Retune

Retune is a dense, album-first desktop music library inspired by the
three-column browser in early iTunes.

| Light | Dark |
| :---: | :---: |
| ![Retune browsing and playing a music library in light mode](screenshots/library-light.png) | ![Retune browsing and playing a music library in dark mode](screenshots/library-dark.png) |

[See all screenshots](screenshots/)

Retune brings local audio files and a Spotify library into one browser while
keeping ratings, normalized genres, play counts, and other personal metadata
local.

## Why Retune?

Retune is for people who want to maintain a music library, not just stream one.

* Browse by genre, artist, and album in a compact, resizable interface.
* Mix local files and Spotify content without treating either as secondary.
* Edit personal metadata without rewriting Spotify's catalog.
* Rate albums and tracks, track plays, search, and manage owned playlists.
* Back up, restore, or merge the library as JSON or gzip.

## Install

| Platform | Package | Supported architecture |
| --- | --- | --- |
| macOS 11+ | Homebrew | Apple Silicon |
| Windows 10/11 (WebView2 105+) | Winget | x64, ARM64 |
| Ubuntu 22.04 and compatible Debian/Ubuntu | APT | amd64, arm64 |

macOS:

```sh
brew install --cask open-cli-collective/tap/retune
```

Windows:

```powershell
winget install --exact --id OpenCLICollective.Retune
```

Debian/Ubuntu, after adding the signed Open CLI Collective APT repository:

```sh
sudo apt install retune
```

See the **[installation and Spotify setup guide](docs/INSTALL.md)** for APT
repository setup, upgrades, uninstall commands, direct downloads, platform
trust warnings, and the exact Spotify callback.

On first launch, import local audio files, connect Spotify, or use both. Local
files remain available without a Spotify account.

## Run from source

See [Development](docs/DEVELOPMENT.md) for platform prerequisites, then run:

```sh
cd apps/desktop
npm ci
npm exec tauri dev
```

## How Retune treats your library

Retune stores ratings, genres, play counts, and other overlay edits locally.
Those edits are never written to Spotify. Explicit content actions such as
saving albums, following artists, and editing owned playlists do update
Spotify.

Built-in Spotify playback does not require the Spotify desktop app. The desktop
app is needed only when using the optional Spotify Connect backend.

Spotify does not expose playlist contents to Retune unless the current user owns
the playlist. Retune can retain cached metadata and track counts for other
playlists, but cannot load or edit their tracks.

## Documentation

* [Architecture map](ARCHITECTURE.md)
* [Library domain](docs/architecture/library.md)
* [Spotify integration](docs/architecture/spotify.md)
* [Playback](docs/architecture/playback.md)
* [Persistence](docs/architecture/persistence.md)
* [Installation and Spotify setup](docs/INSTALL.md)
* [Development and validation](docs/DEVELOPMENT.md)

## License

[MIT](LICENSE)
