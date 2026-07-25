# Local file playback — architecture plan

Scope chosen from the dependency-tier analysis (2026-07-25): the pure-Rust stack only —
rodio + CPAL output, Symphonia decoding with expanded feature flags, `symphonia-adapter-libopus`
for Opus, Lofty for tags/artwork. Explicitly excluded: FFmpeg, GStreamer, AVFoundation,
Media Foundation, DRM'd `.m4p`, WMA/APE/DSD, tag writing, folder watching, file relocation,
Spotify's own local-files feature, and syncing local tracks to Spotify.

Format coverage: MP3, FLAC, Ogg Vorbis, raw AAC-LC, AAC/M4A, ALAC/M4A, WAV, AIFF,
Ogg Opus, WebM Opus.

## Validated facts the design rests on

Verified against the lockfile, crate registries, and source (2026-07-25):

1. **Symphonia 0.5.5, rodio 0.21.1, cpal 0.16 are already in our tree** via librespot's
   `rodio-backend` — but rodio is compiled **sink-only** (no decoder features). librespot
   uses rodio purely as a PCM output; the decode glue for files does not exist yet.
2. **`rodio::Decoder` cannot accept a custom Symphonia `CodecRegistry`** — and the Opus
   adapter requires registering `OpusDecoder` in one. Therefore we do not use
   `rodio::Decoder` at all. We write our own Symphonia decode driver implementing
   `rodio::Source` (+ `try_seek`); the registry is ours, and Opus becomes one
   registration line instead of a framework fight.
3. rodio `Sink`/`Source` support `try_seek`; `UniformSourceIterator` resamples arbitrary
   sample rates/channel counts automatically (the 48 kHz fixtures exercise this).
   Feature `playback` alone suffices for output.
4. Cargo feature unification: enabling Symphonia features on our direct dependency lights
   up codecs in the same Symphonia build librespot uses. Feature names confirmed in
   0.5.5: `mp3, flac, vorbis, ogg, aac, isomp4, alac, pcm, wav, aiff, mkv` (aiff/wav via
   `symphonia-format-riff`; mkv needed for WebM Opus).
5. **`symphonia-adapter-libopus` 0.2.5** is the correct pin (0.2.x ↔ symphonia-core
   ^0.5.4; 0.3+ requires symphonia 0.6). Its default `bundled` feature statically
   compiles libopus via `opusic-sys`, which needs cmake + a C compiler at build time —
   both preinstalled on GitHub macOS runners.
6. Symphonia seeking is packet-accurate (not sample-accurate) and gapless decode is
   supported for MP3/FLAC/OGG (`FormatOptions::enable_gapless`) but not AAC/MP4 —
   acceptable for a music player; noted, not worked around.
7. The playback `EventReducer` is backend-agnostic: any engine emitting `NeutralEvent`s
   with the current generation tag plugs in without state-machine changes.
8. `PlayerBackend` is a one-at-a-time enum (Connect | librespot Local) and switching
   variants tears down sessions. A file engine as a third variant would thrash
   re-auth on alternating spotify↔file queues — so it lives **beside** the enum, not in it.

## Decisions

- **Track identity = canonicalized `file://` URI.** It is the existing dedupe key,
  requires no model migration, and matches the Simple/Surgical bar. Accepted
  consequence: moving a file orphans its overlay edits (rating, genre). A future
  relink feature can heuristic-match on (filename, size, duration); content-hash
  identity was considered and rejected for v1 (import cost, side index, breaks on
  external retagging).
- **No new media source.** Local tracks are Music tracks; `isLocal` is derived from the
  URI in view construction. Browse/ratings/Get Info work unchanged.
- **New crate `crates/retune-audio`**, mirroring the `retune-spotify` adapter pattern:
  owns probe/decode (Symphonia driver → `rodio::Source`), duration, seek, and tag/artwork
  reading (Lofty). No Tauri, no async runtime, no Spotify. Fixture-driven tests live here
  and run without an audio device.
- **File engine lives in `apps/desktop/src-tauri/src/playback/`** beside `PlayerBackend`:
  owns its rodio `OutputStream`/`Sink`, is always available, needs no credentials, emits
  `NeutralEvent`s with the current generation. The `Playback` controller routes each
  track by URI scheme (`file:` → file engine; `spotify:` → active backend) and
  pauses/stops the counterpart on transitions. Disk playback is automatic per-track,
  never a preference.
- **Write-back guardrail extension:** local files are read-only inputs; no local URI is
  ever sent to the Spotify API. Mutations (playlist add, library add) containing any
  local track fail atomically, naming the local tracks, before any upstream request.
- **Local artwork rides the existing `track_artwork` path:** `file:` URIs resolve via
  Lofty embedded art to a `data:` URL (CSP already allows `data:` images), cached like
  Spotify artwork; Control Center republish comes along for free.
- Loudness normalization and gapless are librespot-side features and stay out of the
  file engine v1. Volume routes to the file engine's sink like it routes to the mixer.

## Ticket sequence

Each ticket: TDD (failing tests first), full gate suite, one commit. Doer: gpt-5.6-sol,
medium reasoning. Fixtures: 15 CC0 files + manifest (10 formats, 3 tagged with known
values, 1 garbage negative; WAV/AIFF are 48 kHz on purpose).

1. **T1 — `retune-audio` crate + codec proof.** Workspace member; deps
   `rodio { default-features = false, features = ["playback"] }`, `symphonia
   { default-features = false }` with the 11 features above, `symphonia-adapter-libopus
   = "0.2.5"`, `lofty`. Decode driver with custom registry; fixtures committed.
   Accept: every fixture probes, decodes to EOF, duration within tolerance, seeks;
   Opus decodes through the adapter; Lofty reads the known tags + artwork;
   `not-audio.mp3` fails probe cleanly.
2. **T2 — Import.** `retune-audio` scan (canonicalize, skip dir symlinks, extension
   filter) + tag→record mapping; `import_local` commands; File menu "Add Local Files…" /
   "Add Local Folder…"; batch result {imported, duplicate, failed} surfaced once;
   library persistence via existing store. Accept: mixed-format dir imports, one record
   per canonical path (symlink dedupes), bad files reported without aborting the batch,
   edits survive restart, source files' bytes/mtime untouched, removal never deletes files.
3. **T3 — File engine.** Load/play/pause/seek/volume/stop/position ticks/EndOfTrack via
   `NeutralEvent`s; missing/undecodable file → Unavailable. Device-dependent tests are
   skip-gated in CI; decode logic is already covered device-free in T1.
4. **T4 — Mixed-queue routing.** Controller routes per-track; remove Spotify-only
   assumptions on the file path (URI parse, credential checks — file playback must work
   signed out); reducer-level transition tests: file↔spotify × auto-advance/next/prev,
   repeat one/all, mid-queue file-missing, Spotify-unavailable-but-files-play.
5. **T5 — UI.** Local badge on rows, Get Info shows path (not Spotify ID), missing-file
   warning state, import summary, `track_artwork` handles `file:` URIs → LCD +
   Control Center art.
6. **T6 — Mutation guard.** One guard at the shared Spotify mutation boundary +
   frontend affordances (local tracks excluded from Add-to-Playlist with hint);
   README/PLAN guardrail text updated.

Deferred beyond v1: Windows/Linux build readiness (codec choices are already
cross-platform; nothing else in the app targets them yet), folder watching, relink,
loudness normalization for files, gapless across engines.

## Done means

On a clean install: import a directory containing every supported format; see tracks
with imported metadata; edit metadata without touching files; restart and keep edits;
play a queue alternating local and Spotify tracks with seek/pause/skip/auto-advance;
keep playing local files with Spotify signed out; get an atomic refusal when a playlist
add includes a local track; remove a local track without deleting the file.
