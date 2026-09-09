# Tauri performance experiments

Long-lived branch: `codex/tauri-performance-experiments`, starting at `7998670`.
Keep these changes separate from main until each result has been assessed.

| Step | Experiment | Measurement and behavior checks | Status |
| --- | --- | --- | --- |
| 1 | Isolate elapsed-only playback presentation | Library and playlist React duration, long tasks, frame gaps, rendered scope; pause, seek, track changes, external playback, queue authority | Fixture comparison complete; native validation pending |
| 2 | Window large playlists; avoid rebuilding order/queue on unrelated renders | Opening, selection, scrolling, ticks, DOM count; duplicate entries, focus, multiselection, drag/reorder | Fixture comparison complete; native validation pending |
| 3 | Suspend hidden presentation and decorative animation | Visible/hidden/minimized CPU and render activity; fresh state on show, uninterrupted controller/audio work | Document-visibility probe complete; native CPU/minimize pending (Mac locked) |
| 4a | Remove unnecessary startup settings writes | Warm/cold startup stage durations and write count; defaults and recovery | Store benchmark complete; native launch timing pending |
| 4b | Load only the selected window entry | Production bundle/startup timing for main and importer; first interaction | Bundle/startup fixture complete; no demonstrated latency gain |
| 4c | Move remaining eligible startup I/O off first-paint path | Native first-paint/usable-data timing, overlay/cache/playlist hydration, recovery-before-publication | File-load benchmark complete; native first paint pending |
| 5 | Reduce full importer queue refresh work | Queue IPC count/bytes/time, first useful paint and refresh; global sort/filter, combine/selection, stale responses | Burst fixture complete; native IPC pending |

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

## 3. Hidden presentation

Baseline `71467f3`, candidate `ca9a4ee`. The fixture now uses a long first-track
title and an explicitly simulated document-visibility seam. This isolates the
visibility event contract; it does **not** simulate macOS occlusion, App Nap,
native minimization, or process CPU.

Across three 12-tick samples, hidden React render work fell from 8.2 ms median
(7.7–9.0) to zero commits and zero render duration. The baseline title animation
remained running; the candidate animation was paused in all three samples.
The hidden transport retained its initial elapsed display, then immediately
displayed the latest native position (12) on visibility restoration and resumed
the animation. Raw data: `03-{baseline,after}-hidden.json`.

The store continues accepting every position while hidden; semantic player
updates still use the reducer, and simulated playback/controller timing is
unchanged. Tests additionally cover hidden 11→42 updates staying unpainted until
show, without reading Library data. All 96 Node checks and 71 interaction tests,
lint, and production build pass.

An isolated release-mode native build (development token store, no credentials)
and a generated 4,000-file silent-audio fixture are prepared using
`performance/native.config.json` and `performance/native_fixture.py`. The
native baseline bundle is preserved separately. Computer Use reported that the
Mac is locked, so native visible/hidden/minimized CPU and real WebKit visibility
delivery remain **UNVERIFIED**. No native savings are inferred from these samples.

## 4a. Startup settings writes

Baseline `70c4a94` extracts the unchanged setup load/save sequence into
`FsSettingsStore::load_for_startup` and benchmarks that real boundary. Candidate
`eb7fae6` returns a valid loaded value directly, saving defaults only when absent.
The ignored `startup_settings_performance` release test measures three temporary
directories, each with one missing-file load and 100 existing-file loads.

Existing-file load cost per invocation falls from 4.812 ms median
(4.674–4.851) to 0.045 ms (0.044–0.054). Missing-file/default creation remains a
real atomic write and is noisy: baseline 4.715 ms median (4.606–10.043), candidate
5.339 ms (4.900–11.578); no first-launch improvement is demonstrated. This is
warm local filesystem measurement, not a cold OS-cache or native launch result.
Raw samples: `04a-{baseline,after}-settings.json`.

The regression proves missing defaults are persisted, valid existing bytes are
left unchanged, and invalid bytes remain intact while returning an error.
Existing-file startup performs zero settings saves; missing-file startup still
performs one. Normalization still occurs during load and future explicit writes.
The gain is about 4.77 ms at this boundary, so it should not be presented as a
large app-startup improvement. Native first-paint impact remains **UNVERIFIED**.

## 4b. Window entry loading

Baseline `67f09f6`, corrected candidate `48456a6`. The real `src/main.tsx` now
chooses one dynamic view loader; common theme/dialog CSS remains eager. The
first candidate (`280e840`) demonstrated a functional regression: Vite combined
the conditional loader's preload helper, requesting App dependencies for the
importer and omitting importer CSS. Its failed resource observations are kept
in `04b-rejected-conditional-loader.json`. Separate loader functions fix that
association; `performance/check-startup.mjs` checks both windows' observed
resource lists and specifically guards this regression.

Production JavaScript required for either window falls from 446,859 bytes to
377,118 (main) / 377,075 (importer), about 15.6%. Independently gzipping each
required file with Python's default compression gives 130,008 bytes before and
112,673 / 112,533 after, about 13.3%. These are byte counts, not native memory
measurements. Manifests, file hashes, and sizes are in
`04b-{baseline,after}-bundle.json`.

The production startup fixture uses the actual entry and views with deterministic
native responses, three warm-cache reloads per window, and the same 1280×720
Chromium session. Initial cold navigations warm the resources and are excluded.

| Window / metric | Baseline median (range), ms | Candidate median (range), ms |
| --- | --- | --- |
| Main DOM ready | 30.8 (29.7–33.3) | 31.0 (30.8–31.0) |
| Main ready + two animation frames | 51.1 (36.2–51.3) | 48.9 (42.7–54.2) |
| Importer DOM ready | 16.6 (14.9–18.4) | 17.2 (17.2–19.3) |
| Importer ready + two animation frames | 44.3 (34.6–44.6) | 49.0 (34.3–52.5) |

No latency improvement clears the observed noise. Splitting adds dependency
requests and provides an explicit loading/error/retry UI. The corrected importer
requests its own stylesheet and no App chunk; main requests no importer chunk.
The importer shortcuts dialog opens correctly with keyboard focus. Three entry
tests cover selected-module isolation and accessible chunk-load failure; the
96 Node and 71 existing interaction checks also pass. Build, lint, documentation,
and Tauri ACL checks pass.

Result: demonstrated per-window byte reduction; **no demonstrated startup speed
gain**. Keep the experiment for comparison, but do not merge it solely as a
latency optimization. Native WebKit startup and first-use costs remain
**UNVERIFIED** while the Mac is locked. Raw timings are in
`04b-{baseline,after}-startup.json`.

## 4c. Startup file hydration

Baseline `78c76f9`, candidate `78ad327`. Recovery and initial overlay, membership,
playlist, settings, and cooldown reads now execute together on the blocking pool.
Setup can return while they run. The application dispatcher waits for successful
initialization, so no command receives an empty authoritative substitute. Recovery
failure rejects pending commands. Native menu/media composition stays on the main
thread; early exit waits for recovery before using the ordinary shutdown path.

Three release-mode runs copy the same generated 4,000-track library/settings into
fresh temporary directories on the warm local filesystem. The baseline load call
occupies its caller for 10.448 ms median (9.535–14.910). The candidate returns a
pending future in less than 0.013 ms, and awaiting it takes 9.463 ms median
(8.349–10.726), compared with 10.448 ms baseline. These overlapping ranges do not
establish faster data readiness. They isolate file-load scheduling, **not** full
setup return time, rendering, native first paint, or cold disk startup.

Startup checks pass for gating success/failure, abandoned initialization,
corrupt-library quarantine, invalid settings preservation, and existing journal
recovery. The native main-thread menu/media callback still needs packaged UI
validation. This is the broadest experiment (it moves composition behind the file
load), and the small fixture cost does not yet justify merging it. Native benefit
remains **UNVERIFIED**. Raw data: `04c-{baseline,after}-files.json`.

## 5. Superseded importer refreshes

Baseline `0c09cf8`, candidate `fea5335`. Both full and queue-only refresh callers
now pass their existing shared generation to the queue loader. After each page
arrives, an obsolete loader stops before requesting another page; the existing
publication guard still prevents its result from changing the UI. The newest
request retains every row for global sorting, filtering, selection, and combine.
There is no new native API, cache invalidation policy, or persisted format.

The real importer fixture has 8,000 generated batches, 1,000 per response. A burst
sends six invalidations one millisecond apart; each mock page adds a fixed 15 ms
wait and performs real JSON encoding/decoding. In all three baseline samples this
causes 48 page calls and 15,850,986 bytes. All three candidate samples use 13 calls
and 4,283,276 bytes, a 73.0% reduction. The still-pending page from each obsolete
request finishes; the other seven pages are avoided. Mounted queue buttons stay
at 13, and global selection still sees 8,000 batches.

Refresh-to-two-frames latency is unchanged within noise: 172.7 ms median baseline
(166.0–180.7), 173.1 ms candidate (169.9–174.2). Total React render duration is
small and slightly higher in this sample (3.9 vs 5.9 ms median), so this is a
reduction in wasted requests/bytes, **not** a demonstrated interaction speed gain.
Three candidate samples were collected after background build/test work finished;
one preliminary overlapping sample was excluded before collecting those three.
Raw data: `05-{baseline,after}-refresh.json`.

Two focused checks prove obsolete loads stop after their pending page, never
publish, and leave the newest 2,100-row queue complete; pagination retry and
malformed-response rejection remain intact. Existing global filter/combine,
selection, and stale-response interaction tests pass. The browser filter reaches
batch 8,000 beyond the first response page.

The ordinary isolated refresh still loads the complete queue. Revisioned or
incremental snapshots could reduce that cost, but this experiment does not
establish their value. Burst efficiency is demonstrated only in the fixture;
native IPC, memory, account-bound interaction, and steady-refresh benefit remain
**UNVERIFIED**. Start the existing performance Vite server and open
`/performance/importer.html?window=lastfm-importer&queueFixture=1` to reproduce.

## Combined validation and current assessment

All seven discrete experiments have before/after measurements in this directory.
The strongest fixture results are elapsed-render isolation and playlist windowing.
Avoiding the existing-settings rewrite saves about 4.77 ms per startup load.
Hidden presentation eliminates the remaining hidden transport work in the
visibility seam. Entry splitting reduces bytes, background file loading changes
scheduling, and importer cancellation reduces obsolete burst traffic; none of
those last three demonstrates faster readiness in the measured fixture.

The complete Rust workspace tests, Rust formatting and lint, frontend tests
(96 Node checks plus 76 Vitest tests; 8 existing manual benchmarks skipped),
frontend lint/build, Tauri ACL/permission checks, release contract, and documentation
checks pass. A separate release-mode candidate bundle also builds successfully.
Browser checks confirm global importer filtering/selection through batch 8,000
and dark playlist End navigation through row 4,100 at verified 760×600 and
1280×720 viewports. `validation.json` records scope and limitations.

`recommendations.json` preserves the comparison inputs and full commit lineage.
The skill's structural CLI was run for each input without an evidence registry;
`structural-validation.json` records **UNVERIFIED** for all seven. No trusted
registry or user acceptance threshold was invented. These exploratory local
measurements support further evaluation, not an authenticated merge decision.

**Remaining work:** unlock the Mac and measure the isolated native bundles,
including visible/hidden/minimized CPU, WebKit visibility delivery, first paint,
menu/media/exit behavior, and uninterrupted local playback. No native memory or
CPU saving has been established. `native-candidate.json` identifies the prepared
candidate. All changes remain on the experiment branch; main is not a candidate
for merging until the native checks and per-step assessment are complete.
