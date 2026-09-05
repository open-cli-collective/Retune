# Playback

The desktop playback module is an application controller around three execution
paths: built-in Spotify playback, Spotify Connect, and local-file playback.

## Ownership

The controller owns the canonical track list, active order, current position,
repeat/shuffle state, generation, and volume. Backends do not mutate UI state.
They emit neutral events carrying generation/request identity; one reducer
rejects stale events, updates the snapshot, advances the queue, and persists play
history.

Load and backend changes are latest-intent-wins. Every prepared load carries its
generation, request, URI, and intent identity; a late result cannot start,
publish, count, or advance after a newer intent. A built-in runtime prepared
asynchronously is discarded if Connect or another backend intent wins before
installation.

`Playback` owns the controller, reducer, and backend implementations. Application
composition injects a current-provider resolver and consumes a closed
`PlaybackEffect` callback for player state, errors/recovery, authorization,
connection refresh, play completion, and listening facts. `playback/` has no
Tauri or `AppState` dependency. The shell maps those effects to main-window
events, media/artwork updates, connection refresh, and durable play/Last.fm
work. The callback runs on the serialized playback event path and must not
synchronously re-enter `Playback`; artwork publication remains spawned by the
shell.

Shuffle permutes only the future suffix and retains the canonical list so turning
shuffle off restores normal order without losing the current track. Repeat off,
all, and one are controller policies shared by every backend.
The persisted and IPC boundaries use closed lowercase `RepeatMode`
(`off`/`all`/`one`) and `PlaybackBackend` (`connect`/`local`) enums. The
controller passes repeat by value through reducer, routing, preload, and backend
translation; its internal runtime-backend state remains separate. Repeat changes
apply to the live backend before settings persistence.

Ordinary queues omit overlay tracks disabled by the user. Explicitly starting a
disabled track includes that track for the run while later advancement still skips
the other disabled tracks.

Navigation and resolved Library projections are view-only; they never mutate or
replace the active queue. An explicit Library, playlist, or other play/start
action establishes the canonical queue and current position. Repeat off stops
at that queue's end, repeat all may wrap, and repeat one remains on the current
track. The playback IPC accepts queues up to 100,000 tracks as a resource-safety
boundary; backend-specific request windows do not truncate the canonical queue.
Queue preparation runs on the blocking pool. It builds transient URI indexes
only for requested resources, preserves the first cached-playlist match and
duplicate queue entries, and carries an enabled flag beside each prepared row so
filtering does not rescan the library. Provider misses are hydrated afterward
through the shared Spotify client.

The reducer emits only neutral, listening-generation-scoped facts: natural
start, cumulative forward listening, discontinuity/seek, and completion. The
shell translates those facts into provider actions. Last.fm owns its
`scrobble_threshold_ms` eligibility, provider state, timestamps, and queueing;
the playback reducer has no Last.fm dependency or scrobble policy.

Incremental Last.fm reconciliation does not reuse that local eligibility
threshold. Accepted local scrobbles are recorded as receipts and deduplicated;
every accepted external scrobble in the exact sync window is already a
represented play and is applied additively after mapping. Playback remains
usable while signed out of Last.fm or Spotify.

## Backends

- Built-in Spotify uses librespot with the stored reusable playback credential,
  a login5 preflight, a soft mixer, normalization, gapless playback,
  compressed-audio read-ahead, and an app-data audio cache. Its quality tiers
  are Normal (96 kbps), High (160 kbps), and Very High (320 kbps).
- Spotify Connect controls the active Spotify device through the Web API and
  polls its state. It distinguishes natural completion, external takeover, and
  device disappearance. One narrow async operation gate orders command and
  poll-driven Spotify calls. The backend captures revision/context identity
  under its state mutex, awaits outside it, and commits only while that identity
  remains current, so delayed transport never blocks state reads. Spotify owns
  audio download, buffering, quality, and output in this mode; the Spotify
  desktop app is needed only for this mode.
- The local-file engine uses `retune-audio` and routes `file://` tracks there
  regardless of the selected Spotify backend. It decodes the source file through
  rodio and works while signed out; Spotify quality and cache settings do not
  apply. Clean decoder exhaustion emits natural completion; a late demux or
  decode failure emits `Unavailable` and never completion.

Mixed queues switch at URI boundaries. Only one execution path is allowed to be
audible; transitions pause or stop the counterpart before starting the next.
System Play and Pause commands set an explicit state; only Toggle inverts the
current state.

Native media controls and backend callbacks enter the same controller as
neutral play, pause, toggle, next, previous, seek, and volume intents. They do
not maintain a second state machine or advance the queue in a backend handler.

When built-in playback is selected, missing or rejected playback authorization
returns a typed outcome before a new queue is committed or the reducer advances
to the next track. It never falls through to Connect. The shell keeps the
requested selection outside the controller while it offers separate Spotify
authorization; Cancel leaves playback stopped. File URIs continue through the
local-file engine without Spotify playback authorization.
Authorization prompts carry both the target track ID and URI. React correlates
the URI with its latest play-intent queue before offering authorization; the ID
then identifies the matching row to retry. This avoids positional synthetic IDs
from correlating a stale album or search request with a newer one.

## Built-in Spotify data path

The Spotify access-point session supplies track metadata and the audio key. The
CDN supplies encrypted, compressed audio ranges identified by Spotify `FileId`.

```text
Spotify AP (metadata + audio key)       Spotify CDN (encrypted audio ranges)
                 │                                      │
                 └──────────────────┐                   ▼
                                    │       sparse temporary download
                                    │          │                 │
                                    │          │ complete        │ available ranges
                                    │          ▼                 │
                                    │       FileId cache         │
                                    │          └────────┬────────┘
                                    ▼                   ▼
                                  decrypt → decode → ~0.5 s PCM queue
                                                       │
                                                       ▼
                                                rodio → CoreAudio
```

An incomplete download remains a sparse temporary file and is never promoted to
the persistent cache. A cache hit bypasses the CDN download, but still requires
decryption and decoding. Library and search results share cache entries whenever
they resolve to the same `FileId`; the Retune view that supplied the URI is not
part of cache identity.

During playback Retune requests about 30 seconds of compressed audio ahead of the
decoder, calculated from the nominal bitrate. This is an asynchronous request
window, not a guaranteed startup buffer. The decoded rodio queue remains about
half a second so pause, seek, and track changes stay responsive.

The AP session and player have separate lifetimes. If the session becomes
invalid, Retune leaves current playback alone. Before the next Spotify load or
preload, it reconnects the session and installs it on the existing player,
preserving the current decoder, CDN download, cache handle, mixer, output queue,
and playback position. Unexpected player-thread failure rebuilds the local
runtime and resumes from the millisecond position reported by the player. Explicit
backend and audio-setting changes may also replace the runtime.

Near the end of a track, librespot asks Retune to preload the next item. The
controller rejects stale generation, request, or URI signals, then selects the
next built-in Spotify track from the active queue order. Shuffle order is already
reflected there; repeat-all may wrap, while repeat-one, the end of a repeat-off
queue, and a transition to a local file do not issue a built-in preload. A ready
preload supplies the next load for gapless playback.

## Play history

The reducer records a play once when the configured 50%, 75%, 90%, or completion
threshold is crossed. Completion is a fallback for short or imprecisely reported
tracks. Skipping before the threshold does not increment the count. Playback
events are generation-scoped so late backend events cannot count or advance a
newer track.

The same generation-scoped reducer emits neutral listening facts when playback
starts, advances, seeks, or completes; it does not know about Last.fm or its
thresholds. The Tauri shell and Last.fm service track cumulative forward time,
use the original start timestamp, and decide eligibility for tracks longer than
30 seconds at `min(duration / 2, 240 seconds)`. Explicit seeks, discontinuous
position jumps, and stale backend events do not advance that listening total,
while completion is a fallback when no immediate eligibility decision was
observed. The shell handles Last.fm HTTPS, credential storage, queue
persistence, and retries for built-in Spotify, Spotify Connect, and local/tagged
overlay playback. Disabling scrobbling stops new requests and flushing without
deleting queued items; reconnecting or re-enabling drains the queue.

## Local files

`retune-audio` recursively scans supported audio extensions without following
directory symlinks, canonicalizes paths, probes decodability, and imports basic
tags without materializing embedded pictures. Artwork is read on demand on a
blocking worker and rejected above 8 MiB, so tag I/O and base64 encoding do not
stall the async executor. The hard pre-parse ceiling currently covers ID3
PIC/APIC art in MP3 files, native FLAC PICTURE blocks, and MP4/M4A `covr` data.
The ceiling applies to the aggregate declared bytes of every embedded picture,
not only to each picture independently.
APE-tagged MPEG and comment-encoded FLAC pictures are rejected explicitly, as
are picture-capable AAC, AIFF, WAV, APE, Musepack, Ogg, Speex, and WavPack
containers until they have equally bounded parser paths. Moving a file changes
its identity and leaves the old record unavailable.
Missing files fail cleanly and playback advances according to controller policy.
Audiobook chapter playback is not currently supported.

## Change guidance

Put shared queue and state-transition behavior in the controller/reducer, not in
individual backends. A new backend must translate its native signals into neutral
events and obey generation cancellation.
