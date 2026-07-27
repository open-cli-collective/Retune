# Persistence

The Tauri shell stores Retune state in the platform application-data directory.
All JSON state writes use a temporary file followed by atomic rename.

## Files

| File | Contents |
| --- | --- |
| `library.json` | Versioned core library and overlay |
| `settings.json` | UI, sync, and playback preferences |
| `playlists.json` | Playlist metadata/content cache |
| `cooldowns.json` | Typed Spotify endpoint cooldowns |
| `artist-genres.json` | Persistent Spotify artist-genre cache |
| `tokens.enc` | Encrypted release OAuth token state |
| `dev-tokens.json` | Debug-only token state, mode 0600 |

Built-in Spotify playback also maintains an `audio-cache` directory. Cache data
is disposable; library and settings files are not.

## OAuth token security

Release builds encrypt `tokens.enc` with authenticated encryption. A random file
key is stored in macOS Keychain under service `com.rianjs.retune` and account
`token-file-key`. A legacy Keychain token entry is migrated into the encrypted
file and then removed. An in-process cache avoids repeated credential prompts.

Debug builds deliberately use the permission-restricted development token file
to make local iteration practical. That exception must not enter release builds.

## Recovery and portability

A corrupt library file is quarantined with a timestamped `.corrupt-*` suffix;
Retune starts with an empty library and reports the problem rather than
overwriting the evidence.

Backup/export produces JSON or gzip containing the core library plus portable
settings and playlist cache. Restore validates and replaces the library and
applies exported portable settings/playlists after confirmation. Merge is
additive library-only: it ignores exported settings/playlists, deduplicates by
URI, and preserves existing overlay values.

Machine-specific credentials and Spotify client configuration are not portable
backup data.

## Change guidance

Persist only state that must survive relaunch. Keep caches reconstructible, use
atomic replacement for new state files, and define migration/default behavior in
tests whenever a serialized shape changes.
