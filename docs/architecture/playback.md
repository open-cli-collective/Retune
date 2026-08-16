# Playback

The desktop playback module is an application controller around three execution
paths: built-in Spotify playback, Spotify Connect, and local-file playback.

## Ownership

The controller owns the canonical track list, active order, current position,
repeat/shuffle state, generation, and volume. Backends do not mutate UI state.
They emit neutral events carrying generation/request identity; one reducer
rejects stale events, updates the snapshot, advances the queue, and persists play
history.

Shuffle permutes only the future suffix and retains the canonical list so turning
shuffle off restores normal order without losing the current track. Repeat off,
all, and one are controller policies shared by every backend.

Ordinary queues omit overlay tracks disabled by the user. Explicitly starting a
disabled track includes that track for the run while later advancement still skips
the other disabled tracks.

Navigation and resolved Library projections are view-only; they never mutate or
replace the active queue. An explicit Library, playlist, or other play/start
action establishes the canonical queue and current position. Repeat off stops
at that queue's end, repeat all may wrap, and repeat one remains on the current
track.

The reducer emits only neutral, listening-generation-scoped facts: natural
start, cumulative forward listening, discontinuity/seek, and completion. The
shell translates those facts into provider actions. Last.fm owns its
`scrobble_threshold_ms` eligibility, provider state, timestamps, and queueing;
the playback reducer has no Last.fm dependency or scrobble policy.

## Backends

- Built-in Spotify uses librespot with the stored reusable playback credential,
  a login5 preflight, a soft mixer, normalization, gapless playback,
  compressed-audio read-ahead, and an app-data audio cache. Its quality tiers
  are Normal (96 kbps), High (160 kbps), and Very High (320 kbps).
- Spotify Connect controls the active Spotify device through the Web API and
  polls its state. It distinguishes natural completion, external takeover, and
  device disappearance. Spotify owns audio download, buffering, quality, and
  output in this mode; the Spotify desktop app is needed only for this mode.
- The local-file engine uses `retune-audio` and routes `file://` tracks there
  regardless of the selected Spotify backend. It decodes the source file through
  rodio and works while signed out; Spotify quality and cache settings do not
  apply.

Mixed queues switch at URI boundaries. Only one execution path is allowed to be
audible; transitions pause or stop the counterpart before starting the next.
System Play and Pause commands set an explicit state; only Toggle inverts the
current state.

When built-in playback is selected, missing or rejected playback authorization
returns a typed outcome before a new queue is committed or the reducer advances
to the next track. It never falls through to Connect. The shell keeps the
requested selection outside the controller while it offers separate Spotify
authorization; Cancel leaves playback stopped. File URIs continue through the
local-file engine without Spotify playback authorization.

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
directory symlinks, canonicalizes paths, probes decodability, and reads tags and
artwork. Moving a file changes its identity and leaves the old record unavailable.
Missing files fail cleanly and playback advances according to controller policy.
Audiobook chapter playback is not currently supported.

## Change guidance

Put shared queue and state-transition behavior in the controller/reducer, not in
individual backends. A new backend must translate its native signals into neutral
events and obey generation cancellation.
