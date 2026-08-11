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
| `dev-tokens.json` | Development token state, mode 0600 |

The token record has an optional reusable built-in playback credential containing
the librespot username and AP authentication bytes. Its absence is the default,
so older token files remain readable. Release builds keep it inside encrypted
`tokens.enc`; development token files retain the existing mode-0600 boundary.
Refreshing the Web API token preserves the playback credential. Playback
rejection removes only this field, while explicit Spotify disconnect removes the
whole token record. It is machine-specific and never belongs in backup/export.

Built-in Spotify playback also maintains an `audio-cache` directory. Cache data
is disposable; library and settings files are not.

The playlist cache retains Spotify display metadata for every fetched track,
including disc/track numbers and album release date. Older caches deserialize
with defaults and are refreshed once before snapshot-based fetch skipping resumes.

Column visibility is UI state in `settings.json`: the Library has one hidden-column
list, and playlists have independent lists keyed by Spotify playlist ID.

## Spotify audio cache

The audio cache contains complete encrypted Spotify audio files keyed by
Spotify `FileId`. A cache miss downloads ranges into a sparse temporary file;
only a completed download is copied into `audio-cache`. Interrupted and abandoned
downloads therefore do not become persistent cache entries.

The cache has a fixed 2 GiB limit. When a completed file pushes it over the
limit, the least-recently-used entries are removed until it fits. Hits update the
in-process access order. After relaunch, that order is reconstructed from
filesystem access time, with modification or creation time as fallbacks, so
eviction is LRU-like rather than a durable exact playback history.

Cache identity is independent of Retune's library and search projections. Both
reuse the same entry when Spotify resolves them to the same `FileId`; a different
audio format or quality may resolve to a different file. The cache is disposable
and excluded from backup and restore.

## OAuth token security

Release builds encrypt `tokens.enc` with authenticated encryption. A random file
key is stored in the platform-native credential store under service
`com.rianjs.retune` and account `token-file-key`: macOS Keychain, Windows
Credential Manager, or Linux Secret Service. A legacy native token entry is
migrated into the encrypted file and then removed. An in-process cache avoids
repeated credential prompts. The keyring mock backend is not enabled for release
targets.

Debug builds and local bundles built with the `dev-token-store` feature use the
permission-restricted development token file. Ordinary release bundles never
enable that feature and retain the encrypted-file/native credential-store
boundary.

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
