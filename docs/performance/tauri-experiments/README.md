# Tauri performance experiments

Long-lived branch: `codex/tauri-performance-experiments`, starting at `7998670`.
Keep these changes separate from main until each result has been assessed.

| Step | Experiment | Measurement and behavior checks | Status |
| --- | --- | --- | --- |
| 1 | Isolate elapsed-only playback presentation | Library and playlist React duration, long tasks, frame gaps, rendered scope; pause, seek, track changes, external playback, queue authority | Fixture comparison complete; native validation pending |
| 2 | Window large playlists; avoid rebuilding order/queue on unrelated renders | Opening, selection, scrolling, ticks, DOM count; duplicate entries, focus, multiselection, drag/reorder | Fixture comparison complete; native validation pending |
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

## 1. Native elapsed presentation

The experiment at `c504db219711d1b25f7133f939325a4d70fc86c2` sends native
position changes to a per-player store read by the transport. Semantic changes
and simulated playback retain the app reducer. No native controller or IPC
contract changed.

| 12 one-second updates | Baseline median (range), ms | Candidate median (range), ms |
| --- | --- | --- |
| Library total React render duration | 303.4 (303.1–309.0) | 9.8 (9.3–10.0) |
| Playlist total React render duration | 3932.9 (3787.7–3938.1) | 9.0 (8.8–9.9) |
| Playlist largest animation-frame gap | 317.4 (301.0–450.6) | 33.3 (18.7–50.0) |

Three samples per cell. Both variants ended at elapsed 12 with zero IPC calls
during ticks. Library retained 41 mounted rows and the playlist retained 4,100.
Playlist baseline had repeated long tasks (180–462 ms); all three candidate
samples had none. A regression check uses a counted Library DTO getter to prove
elapsed-only events do not re-read that view, while pause still does. The
baseline's second playlist commit per tick also disappears.

Conditions: same Codex in-app Chromium session on the macOS arm64 host, 1280×720,
Vite development mode without StrictMode, no added CPU/network throttling,
signed-out deterministic native mock, warm loaded data, no service worker or
extension added by this fixture. Exact Chromium version and native WebKit costs
were not captured. Baseline Library source `cdb2648`, playlist source `8868b59`
(only its seed-row selector changed); candidate source `c504db2`.
Raw samples are the adjacent `01-{baseline,after}-{library,playlist}.json` files.

Checks: 96 Node checks and 70 interaction tests passed (8 existing manual
benchmarks skipped), lint and production build passed, documentation check
passed after routing this report from AGENTS.md. The real fixture's keyboard
seek changed 12→13 and Pause changed the accessible control to Play. Existing
interaction coverage plus the added assertions exercise track replacement,
external playback, selection while playing, and stop resetting/disabling seek.

Result: strong reduction within this fixture; native-app recommendation remains
**UNVERIFIED**. The Chromium fixture does not establish native CPU, memory,
WebKit painting, or real audio behavior. Keep on the experiment branch and
validate at the native boundary before proposing a merge.

## 2. Playlist windowing

Baseline `22342c7`, candidate `04ad86a`, same Chromium fixture and conditions as
step 1. The candidate reuses the Library's fixed-row windowing, keeps the focused
row mounted, and memoizes the complete playlist order and playback queue. Selection
and mutations retain upstream positions; dragging computes its insertion position
from the complete list geometry with CSS zoom accounted for.

| Action to two animation frames | Baseline median (range), ms | Candidate median (range), ms |
| --- | --- | --- |
| Open playlist | 769.7 (730.3–797.4) | 32.0 (32.0–50.4) |
| Select first row | 350.3 (333.9–370.3) | 31.8 (31.2–32.0) |
| End key | 420.7 (416.4–437.6) | 33.4 (33.3–33.4) |
| Shift+Home | 620.5 (590.3–628.0) | 33.3 (33.3–33.4) |
| Sort by name | 536.4 (525.3–582.3) | 33.3 (33.3–33.4) |
| Scroll to middle | 31.2 (30.9–33.0) | 33.3 (33.2–33.3) |

Three samples in `02-{baseline,after}-interactions.json`. Mounted rows fall from
4,100 to 47–48; all candidate actions avoid long tasks. Scrolling was already
within one two-frame wait and shows no improvement; it now incurs a small React
render to replace visible rows. Two-frame timings have a roughly 33 ms floor at
this refresh rate, so they do not resolve smaller improvements.

Checks: 96 Node checks and 71 interaction tests pass, lint/build pass. The new
4,000-entry regression crosses the viewport with End and Shift+Home, plays the
last duplicate with its distinct synthetic ID, removes the full selected range,
and verifies a drag at offset 2,000 sends global insertion index 2,002. Existing
Library window/focus checks still pass. Browser navigation at 760×600 retained
focus and correct position/total metadata through the last row; the normal
viewport was restored afterward.

Result: strong fixture improvement for opening/selection/sort; scrolling unchanged
within the measurement's resolution. Native-app recommendation remains
**UNVERIFIED** pending WebKit/native validation.
