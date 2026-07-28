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

## Backends

- Built-in Spotify uses librespot with the current OAuth token, a soft mixer,
  configurable bitrate/normalization/gapless behavior, read-ahead, and an
  app-data audio cache. The workspace pins a patched librespot fork.
- Spotify Connect controls the active Spotify device through the Web API and
  polls its state. It distinguishes natural completion, external takeover, and
  device disappearance. The Spotify desktop app is needed only for this mode.
- The local-file engine uses `retune-audio` and routes `file://` tracks there
  regardless of the selected Spotify backend. It works while signed out.

Mixed queues switch at URI boundaries. Only one execution path is allowed to be
audible; transitions pause or stop the counterpart before starting the next.

## Play history

The reducer records a play once when the configured 50%, 75%, 90%, or completion
threshold is crossed. Completion is a fallback for short or imprecisely reported
tracks. Skipping before the threshold does not increment the count. Playback
events are generation-scoped so late backend events cannot count or advance a
newer track.

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
