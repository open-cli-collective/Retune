# Tauri performance experiments

Long-lived branch: `codex/tauri-performance-experiments`, starting at `7998670`.
Keep these changes separate from main until each result has been assessed.

| Step | Experiment | Measurement and behavior checks | Status |
| --- | --- | --- | --- |
| 1 | Isolate elapsed-only playback presentation | Library and playlist React duration, long tasks, frame gaps, rendered scope; pause, seek, track changes, external playback, queue authority | Baseline preparation |
| 2 | Window large playlists; avoid rebuilding order/queue on unrelated renders | Opening, selection, scrolling, ticks, DOM count; duplicate entries, focus, multiselection, drag/reorder | Pending |
| 3 | Suspend hidden presentation and decorative animation | Visible/hidden/minimized CPU and render activity; fresh state on show, uninterrupted controller/audio work | Pending |
| 4a | Remove unnecessary startup settings writes | Warm/cold startup stage durations and write count; defaults and recovery | Pending |
| 4b | Load only the selected window entry | Production bundle/startup timing for main and importer; first interaction | Pending |
| 4c | Move remaining eligible startup I/O off first-paint path | Native first-paint/usable-data timing, overlay/cache/playlist hydration, recovery-before-publication | Pending |
| 5 | Reduce full importer queue refresh work | Queue IPC count/bytes/time, first useful paint and refresh; global sort/filter, combine/selection, stale responses | Pending |

Measure at least three comparable samples before and after each discrete change.
Preserve raw data and exact source revisions. Browser fixtures and React timings
are separate from native WebKit and process measurements. An unmeasured or noisy
result is inconclusive, not a demonstrated improvement. Preserve negative
experiments in branch history and identify them explicitly.

The browser playback fixture runs the real App and CSS with a deterministic
4,000-track Library and 4,100-entry playlist (100 deliberate duplicates). Its
native mock emits 12 authoritative player-state messages at one-second intervals,
matching the native backends' cadence. It performs no network, audio, or user-data
operations. Start from `apps/desktop` with:

```sh
npm exec vite -- --config performance/vite.config.mjs
```

Open `http://127.0.0.1:5185/performance/`, wait for loading, and use the measurement
button. Repeat on the playlist. The visible JSON panel records React Profiler
commits/durations, animation-frame gaps, long tasks when supported, IPC count,
rendered rows, and the final elapsed display.
