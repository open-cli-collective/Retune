# Handoff: Retune — iTunes-style Spotify wrapper with a local metadata overlay

## Overview
Retune is a desktop app that wraps the Spotify desktop client and re-imposes the
circa-2004 **iTunes three-column browsing model** (Genre → Artist → Album →
Tracks, a progressive filter) on top of a user's Spotify library. Its defining
feature is a **local metadata overlay**: the user's own normalized tags, ratings,
and grouping that map onto Spotify track IDs without ever mutating anything on
Spotify's backend. The overlay is the point — it restores album-first, no-playlist
listening for people who hate playlist-centric modern Spotify.

The bundled prototype is a working demonstration of the **UX layout and
interaction model**, not the architecture. Read the "Product intent" section for
the engineering the user actually wants built.

## About the Design Files
`Retune.dc.html` in this bundle is a **design reference created in HTML** — a
prototype showing intended look and behavior. It is **not production code to copy
directly**. The task is to recreate this design's layout and interactions in a
real desktop app, using the framework chosen below and idiomatic patterns for it.
The HTML uses a small in-house component runtime (`support.js`); ignore that
runtime entirely — it is a prototyping tool, not part of the deliverable.

## Fidelity
**High-fidelity for layout and interaction; neutral on chrome styling.** The
column structure, progressive-filter behavior, rating inheritance, search-scope
toggle, menu organization, and overall information architecture are the spec and
should be recreated faithfully. The *visual chrome* (exact gradients, the faux
macOS window frame, traffic lights) is deliberately generic — the user explicitly
does not want brushed metal or specific button textures reproduced. Match native
platform conventions for chrome instead.

## Product intent (from the user — this drives the build, not just the mock)

**Workflow / process**
- Follow the **planning portion of the plan-execute-TDD workflow** before writing
  code: produce a plan, then execute test-first.
- **Idiomatic SOLID.** Keep the overlay/domain logic decoupled from Spotify I/O
  and from the UI so future layouts and future media types drop in without
  rewrites (see "Extensibility" — the layout is explicitly "one *possible*
  layout").
- **GitHub Actions CI/CD** from the start.
- The core library should be **Rust**, with its capabilities exposed through
  natural menu / menu-bar items in the UI. (Framework for the shell is open —
  Electron, Tauri, or native macOS are all acceptable. Tauri pairs naturally with
  a Rust core; call this out in the plan and let the user confirm.)
- Set up **computer-use / local testing** for the Spotify-dependent paths, or
  give the user step-by-step instructions to do so.

**Spotify integration rules**
- The user must have the **Spotify desktop app installed and be logged in** — that
  is acceptable.
- **The overlay never writes back to Spotify; content operations do.** Overlay
  *metadata* (custom genre/artist/album tags, ratings) is local-only — "read-only"
  refers to Spotify's backend, and the overlay is fully user-editable locally.
  *Content* operations are the deliberate exception and treat Spotify as the
  canonical store: adding to the library, adding tracks/albums to a playlist, and
  reordering playlist tracks all make real Spotify API writes. (Updated 2026-07-24
  with the playlist workstream.)
- **Everything in the user's Spotify library populates the local library.** Using
  Spotify's own "Add to library" should also add to the local overlay library.
- **Play counts**: librespot cannot expose Spotify play counts, so this feature
  was cut — there is no Plays column and no play-count settings. (If a future
  backend can supply them, reintroduce a Plays column + a Preferences section.)

**Persistence & portability**
- Persistence should be **platform-idiomatic** (app data dir, etc.), with the
  overlay serializable to **JSON**.
- **Backup / Export**: export to `.json` and to `.json.gz`.
- **Import** must accept either `.json` or `.json.gz`.
- **Restore** = replace the current library. **Merge** = additive import
  (dedupe by Spotify track ID). Both belong in the File menu.

## Screens / Views

The app is a **single window** with fixed regions. There is one primary view; the
only modal is Get Info.

### Main window
Fixed layout, top to bottom:

1. **Title bar** (34px) — window chrome + centered title "Retune — Library" +
   a theme toggle (☾/☀) on the right.
2. **Menu bar** (24px) — Retune · **File** (functional) · Edit · View · Controls ·
   Account · Help. On native macOS this is the system menu bar, not an in-window
   strip; the strip in the mock only exists to show where File items live.
3. **Transport bar** (60px) — 3-column grid `200px | 1fr | 268px`:
   - Left: prev / play-pause / next, volume icon + slider.
   - Center: an "LCD" now-playing readout — track title, `artist — album`,
     elapsed time, progress bar, remaining time.
   - Right: **search scope toggle** (Library / Spotify pills) + search field.
4. **Body** — 2-column grid `190px | 1fr`:
   - **Sidebar** (190px): "Library" section listing the media-type sources
     (Music / Podcasts / Audiobooks) with per-source counts; a **"Playlists"
     section** (see Playlists below); a footer note about the overlay staying local.
   - **Content**: the three-column browser, an optional album-rating strip, the
     track list, and a status bar (see below).
5. **Status bar** (26px) — "+" add button (left), "N songs, H:MM hours" (center),
   "N overlay edits" (right).

### Three-column browser (the core of the UX)
Fixed height ~200px, 3 equal columns: **Genre | Artist | Album** (labels change
per media type — see Extensibility). Each column: a 20px header + a scrollable
list. Each list has an "All (N …)" row at top that clears that filter level.

**Progressive filter behavior (critical):**
- Selecting a Genre filters the Artist column to artists having tracks in that
  genre, and filters everything downstream.
- Selecting an Artist filters the Album column and the track list.
- Selecting an Album filters the track list to that album.
- Selecting a narrower level never clears a broader one; selecting a broader level
  resets the narrower selections (artist/album/track).
- The track list always reflects the current intersection of selections.

### Album-rating strip (conditional)
Appears (28px) between the browser and the track list **only when an album is
selected**. Shows the album name, a 5-star control that sets the album-level
rating, and a hint: "applies to all tracks unless individually overridden."

### Track list
Header row + rows. Grid columns:
`22px | Name (minmax 160px,1.6fr) | Time 52px | Artist 1.1fr | Album 1.1fr | Genre 0.9fr | Rating 84px`.
- Col 1: playing indicator (▶ playing / ❚❚ paused) on the current track.
- Genre cell shows a leading "●" when the track's genre was overridden from
  Spotify's original.
- Rating: 5 stars. **Effective rating = track override ?? album rating ?? unrated.**
  Inherited stars render gray/muted; explicit overrides render gold (#e0a53d);
  empty stars are faint. Clicking a star sets a track override; clicking the
  currently-set star clears the override (reverts to inherited).
- Row selection highlights in the accent color; double-click plays.
- Zebra striping on alternate rows (toggleable).

### Get Info modal
Opened via the ⓘ affordance, File → Get Info, or ⌘I on the selected track.
Shows: read-only **Spotify ID**;
editable **Name / Artist / Album / Genre** fields (genre with the hint
'normalize freely, e.g. "Operatic Rock" → "Rock"'); a **track rating** star
control with inherited/override/clear semantics; an info banner that, when the
track's genre differs from Spotify's, reads 'Spotify reports this as "…". Your
overlay wins in Retune.' Buttons: Cancel / **Save Overlay**.

### First-run / empty state
When the library is empty (fresh install, or File → Set Up Library…), the three
columns and track list render blank and the track area shows a centered prompt:
♪ glyph, "Your library is empty", a one-line explainer, and a **Set Up Library…**
button. The status bar reads "No library — set up to begin".

### Set Up / Sync modal
A blocking dialog with three confirmations, then Sync:
1. **Spotify app Client ID** — text field (helper: create one at
   developer.spotify.com → Dashboard → your app).
2. **Web API enabled** — a checkbox (checked by default) confirming the app's Web
   API scope is on.
3. **Spotify desktop app** — an auto-detected status row: green pulsing dot +
   "Running & logged in" + "✓ auto-detected". The app should actually probe for a
   running, logged-in Spotify client and reflect it here (green when found; show
   a red/neutral state + guidance when not).
Buttons: **Cancel** (dismisses the dialog, leaving the empty state) and **Sync**
(disabled until a Client ID is present, Web API is checked, and Spotify is
detected). Sync begins the import.

### Sync / progressive population
Import runs on a **poll loop** (not event-based): the UI populates incrementally
as tracks arrive — genres, artists, albums, and the track list all fill in a
growing slice while the sync runs, so the user watches their library build. The
status bar switches to an **iTunes↔iPod-style sync indicator**: "⟳ Syncing from
Spotify…", a progress meter, and an **X / Y tracks synced** counter. When the poll
reports completion the meter clears and the normal status line returns. The mock
simulates this with a timer revealing a growing slice of the library; the real
app polls the importer for progress and refreshes the visible set each tick.

## Interactions & Behavior
- **Search scope toggle**: Library scope filters the local library in place.
  **Spotify scope + a non-empty query replaces the whole content area** (the
  three-column browser and album strip are hidden) with a dedicated Spotify
  results view — this is a concrete example of the "one possible layout"
  architecture: the same window hosts a different projection.
  - **Filter tabs** across the top: **All · Artists · Albums · Tracks**, each with
    a live count (All is the default). This is the answer to "how to filter by
    album vs track vs artist."
  - **Grouped results**: an Artists section (rows with a round thumbnail + name +
    descriptor + "View albums ›"), an Albums section (cover thumb + title +
    "artist · year" + a **+ Add / ✓ Added** toggle), and a Tracks section (thumb +
    title + "artist · album" + duration). Under All, all three sections stack;
    a single tab shows just that section.
  - **Navigation to Artist / Album pages** (the app will wire these to a
    right-click ▸ *Go to artist / Go to album / Go to track* menu; Go to track
    opens the album page with that track highlighted). Both are full-content
    pages reached via a **nav stack** — double-click an album row (or "View
    albums ›" on an artist) to push a page; a **Back** link at the top pops it,
    with a context-aware label ("‹ Back to results" / "‹ Back to artist" /
    "‹ Back to album").
    - **Album page**: large cover, album type (e.g. "Album · Cast Recording"),
      title, **clickable artist name** (→ artist page), an **album star rating**
      (5 stars) + "year · N tracks · duration", a prose **description**, **Play**
      and **+ Add to Library / ✓ In Library — Remove** buttons, and the full track
      list with **per-track star ratings** (inheriting the album rating, gold when
      overridden) and durations. Double-click a track to preview; when arrived via
      "Go to track", that row is highlighted with an accent bar. The goal is
      *recognition* — enough to tell the 1987 original London cast from the 2004
      film or 2022 revival — not a clone of Spotify's product page.
    - **Artist page**: circular artist image, name, "descriptor · N albums",
      **Play** and **+ Follow / ✓ Following** buttons, a **Discography** list
      (cover + title + "year · type" + Add toggle, click → album page), and a
      **Top Tracks** list (number, title, source album, duration).
  - Adding an album (from a row, the album page, or an artist's discography) pulls
    it and its tracks into the local overlay. In the mock this is a stubbed
    toggle; wire it to the real importer. Search results are a small mock catalog
    keyed to "phantom of the opera" for demonstration.
- **Rating inheritance (confirmed precedence)**: effective rating = per-track
  override, else album rating, else unrated. Album rating cascades to its tracks;
  per-track overrides win; re-clicking the set star clears the override.
- **Genre normalization**: editing a track's genre records the Spotify original
  the first time it diverges, and flags the track with "●".
- **Playlists** (left sidebar): lists the user's Spotify playlists under a
  "Playlists" heading with a "+" (new playlist) affordance. **Owned vs. followed
  playlists are visually distinguished** — the user's own playlists show *no*
  leading icon; playlists owned by others get a distinct glyph (☍ in the mock) and
  their owner shown as a subtitle in menus. Each row shows its track count.
  - **Add to Playlist** works from anywhere a track or album appears — library
    track rows, Spotify album/track rows, and the album page. Two paths:
    - **Right-click ▸ Add to Playlist…** opens a small context menu (the mock also
      lists disabled Go-to-Album/Artist items the app will wire up), which opens
      an **Add to Playlist popover**: the item's label, every playlist with a ✓
      when it already contains the item, owner subtitles on others' playlists, and
      a **+ New Playlist** action.
    - **Drag-and-drop**: track/album rows are draggable; playlist sidebar rows are
      drop targets that highlight in the accent color on drag-over and add the
      dragged item on drop.
  - In the mock, playlists are seed data and membership is in-memory. The real app
    wires these to the Spotify playlist API as canonical: adds and reorders write
    through to Spotify (snapshot_id concurrency), per the write-back guardrail
    above — superseding the mock's original "never writes back" caveat here.
- **Playback (prototype-only)**: transport updates the LCD and advances a timer;
  in the real app this drives / reflects the Spotify player.
- **Keyboard shortcuts**: Space = play/pause, ← / → = prev/next track (within the
  current filtered list), ⌘I = Get Info on selected track, ⌘L = focus Library
  scope, Esc = close modal/menu. Expand as appropriate for the platform.
- **File menu**: Set Up Library… · Get Info · Preferences (⌘,) · Add to Library from Spotify ·
  Back Up (.json) · Export (.json.gz) · Restore · Merge. The "Retune" app menu
  also opens Preferences.
- **UI zoom / text size**: Cmd/Ctrl + `=`/`-` to grow/shrink, Cmd/Ctrl + `0` to
  reset, and Cmd/Ctrl + scroll wheel. Implemented as a whole-window zoom factor
  (clamped ~0.7–1.8) so text and rows scale together; wire an equivalent into the
  View menu in the real app. Native platforms may prefer a dynamic-type / text-
  size setting instead of CSS zoom.
- **Preferences modal** (⌘, / Retune menu): a **tabbed** dialog (tabs are the
  idiom here). Three tabs:
  - **Appearance** — Theme radio group: System / Light / Dark (System follows
    the OS; mirrors the title-bar cycle).
  - **Library** — Spotify Client ID text field; "Automatically add my entire
    Spotify library" toggle; "Connect to Spotify automatically at launch" toggle.
  - **Audio** — Streaming quality slider (Low / Normal / High / Very High, with a
    kbps readout); Playback engine radio (Spotify app (Connect) / Built-in
    (librespot)); "Normalize volume across tracks" and "Gapless album playback"
    toggles.
  Footer buttons: Cancel / Save. (A Play Counts section was removed — see Play
  counts above.)
- **Theme**: light, dark, and **system** (follows OS `prefers-color-scheme`, live-
  updating). Cycled via the ☀/☾/🖥 icon in the title bar and the View menu; also a
  prop. (Original iTunes had no dark mode; the user wants one anyway.)

## State Management
Domain state (belongs in the Rust core / a UI-agnostic store):
- `library`: per media type (`music` / `podcasts` / `audiobooks`), a list of
  overlay track records: `{ id, spotifyId, cat, art, alb, name, time,
  rating|null, orig|null }` where `cat/art/alb` are the generic
  category/artist/album fields (see Extensibility), `rating` is a per-track
  override or null, and `orig` records the pre-normalization genre or null.
- `albumRatings`: per media type, a map keyed by `artist␞album` → 1–5.

UI state (belongs in the shell):
- `source` (active media type), `genre` / `artist` / `album` (filter selections),
  `selectedTrack`, `playing` / `isPlaying` / `elapsed`, `query` / `searchScope`,
  `theme`, and modal/menu open flags.
- First-run / sync: `emptyMode`, `setupOpen`, `clientId`, `webApi` (bool),
  `spotifyOk` (detected, bool), `syncing`, `syncDone`, `syncTotal`. During sync
  the visible library is a `slice(0, round(len * syncDone/syncTotal))` of the
  imported set — replace with the real importer's running progress + partial
  results in the target app.
- Playlists / add-to-playlist: `playlists` (`{id, name, owner, mine, tracks[]}`),
  `selectedPlaylist`, `addToPlaylistFor` ({kind:'track'|'album', label, ids} — the
  popover's subject, null = closed), `ctxMenu` ({x, y, item} — right-click menu),
  `dragItem` / `dropTarget` (drag-and-drop). Membership is stored as track ids on
  each playlist; adding an album adds all its track ids.
- Spotify search / navigation: `searchScope`, `spotifyFilter`
  (all/artists/albums/tracks), `nav` (a stack of `{type:'artist'|'album', id,
  highlight?}` — empty = results list, push to drill in, pop to go back),
  `addedAlbums`, `spotAlbumRatings`, `spotTrackRatings`. The right-click *Go to*
  actions just push the matching entry onto `nav` (Go to track sets `highlight`).

## Extensibility (do not design into a corner)
The mock stores tracks with **generic fields `cat / art / alb`** and a per-source
**label map** that renames the columns per media type:
- Music → Genre / Artist / Album (track = Song)
- Podcasts → Category / Podcaster / Show (track = Episode)
- Audiobooks → Category / Author / Book (track = Chapter)

Adding a media type is a new source + label map, **not a new layout**. The
three-column browser is one *possible* projection of the overlay; keep the overlay
model and the layout decoupled so alternate layouts (e.g. a flat search list, a
grid) can consume the same data. This decoupling is an explicit user requirement.

## Design Tokens
Layout is the spec; exact chrome colors are illustrative (match platform native).
- **Accent (selection)**: `#3f7fd6` (also a configurable prop in the mock).
- **Star colors**: explicit/override `#e0a53d` (gold); inherited `rgba(150,150,150,.7)`;
  empty `rgba(150,150,150,.4)`.
- **Override marker**: leading "●" in the genre cell.
- **Light theme** (illustrative): desk `#b9bcc2`, list `#ffffff`, zebra alt
  `#edf3fe`, text `#1a1a1a`, borders `#b7b7b7`, headers `#f0f0f0→#e2e2e2`,
  sidebar `#e6ebf2→#dbe2ec`, LCD `#eef2f7→#e2e8f0`.
- **Dark theme** (illustrative): desk `#161719`, list `#1f2124`, zebra alt
  `#26282d`, text `#e9e9ec`, borders `#45474c`, headers `#35373c→#2c2e32`.
- **Type**: dense UI text, 11–12.5px; system font (Lucida Grande / system-ui).
  Section labels 10px uppercase, letter-spacing .04em.
- **Row heights**: browser rows 17px, track rows 18px, column headers 20px.

## Screenshots
Each state is provided in both light and dark themes (`screenshots/`):
- `playing-light.png` / `playing-dark.png` — track playing (LCD populated, ▶ row marker).
- `menu-light.png` / `menu-dark.png` — File menu open.
- `viewmenu-light.png` / `viewmenu-dark.png` — View menu (text size + theme).
- `getinfo-light.png` / `getinfo-dark.png` — Get Info overlay editor.
- `prefs-appearance-{light,dark}.png` / `prefs-library-{light,dark}.png` /
  `prefs-audio-{light,dark}.png` — the three Preferences tabs, each theme.
Toggle themes via the ☀/☾/🖥 icon in the title bar, the View menu, or the theme prop.

## Assets
None. No images, icons, or external fonts — glyphs are Unicode (▶ ❚❚ ⏮ ⏭ ★ ☆ ●
🔊 ♪ 🎙 📖 🔒 ⌕). Album art is a striped placeholder in the mock; the real app
should use Spotify artwork.

## Files
- `screenshots/` — light + dark reference renders: playing, File menu, View menu,
  Get Info, Preferences, plus first-run set-up (`setup-*`), empty state
  (`empty-*`), mid-sync progressive population (`syncing-*`), and the Spotify
  search experience: results list (`spotify-results-light`), a single filter tab
  (`spotify-albums-filter-light`), the **Artist page** (`artist-page-{light,dark}`,
  plus `artist-page-results-light` showing it in context), and the **Album page**
  (`album-page-{light,dark}`), and the **Playlists** feature: right-click context
  menu (`playlist-context-menu-{light,dark}`), Add-to-Playlist popover
  (`playlist-add-popover-{light,dark}`), and a drag-and-drop drop-target highlight
  (`playlist-drag-drop-light`).
- `Retune.dc.html` — the full interactive prototype (layout, progressive filter,
  ratings + inheritance, search-scope toggle, theme, JSON/JSON.gz backup/restore/
  merge, Get Info, keyboard shortcuts). Open in a browser to interact.
