# Desktop UI validation — 2026-10-14

Tested the release app with the existing local library and Spotify account. The final build was installed at `/Applications/Retune.app` and launched successfully. Tests below used native computer interaction unless explicitly marked automated.

## Playback origin and track identity

| Procedure | Expected result | Observed result |
| --- | --- | --- |
| Play from Cinematic Rock → Nathan Wagner → Introspection, visit Chill and Recently Added, then click the playing title. | Restore all three facets and reveal the current track without restarting it. | Passed: all facets restored, Die selected, playback continued. |
| Sort Metalcore by Song, play I Can't Breathe, leave the playlist, then click the playing title. | Restore the playlist's captured sort and select the playing row. | Passed; playback continued. |
| Play I'm Gone in Metalcore, search Spotify for Nightwish, then click the playing title. | Return to Metalcore and reveal the offscreen playing row. | Passed: row 91 selected and bold; elapsed time continued. |
| Search Spotify for Nightwish Nemo, choose Tracks, play Nemo, visit Library, then click the playing title. | Restore the query and Tracks tab. | Passed: query and Tracks results restored; playback continued. |
| Open Once from Spotify search, play Higher Than Hope, visit Music, then click the playing title and use Back. | Restore the album and playing row; Back returns to search results. | Passed after correcting the restored navigation root: Back returned to Nightwish Nemo results. |
| Play Follow You from a temporary Release Radar shadow playlist, leave and return through the playing title, then add the track to the library. | Restore the shadow playlist and retain the playing highlight when the library ID changes. | Passed: correct row highlighted before and after adding it. |

Automated interaction tests additionally cover queue advancement to another URI, asynchronous library loading, repeated title clicks without replay, and Spotify album restoration. Playback origins are session-local; externally started playback has no captured origin.

## Browsing, selection, and metadata

| Procedure | Expected result | Observed result |
| --- | --- | --- |
| Type a track prefix in Metalcore, External Covers, a shadow playlist, and the filtered library. | Select and reveal the matching row without playing it. | Passed for `i'm`, `heyya`, `follow`, and `manof`. |
| Rapidly type a prefix whose first letter moves focus offscreen. | Keep subsequent letters in the same prefix. | Found a focus race; fixed focus timing and verified `i'm` selects I'm Gone in the rebuilt native app. Regression tests omit inter-letter animation-frame waits. |
| Exercise track prefix navigation at 80% and 100% zoom. | Selection and focus remain usable. | Passed. Automated coverage includes playlist, shadow playlist, library, genre, artist, and album contexts at both zoom levels. |
| Double-click genre, artist, and album with shuffle on, then repeat with shuffle off. | Shuffle applies to the chosen collection; disabling shuffle starts its first track. | Passed: shuffled starts included Die and Homesick; ordered starts were Dreams of my Downfall for the album and Angels vs Demons for artist/genre. |
| Inspect collection footers. | Counts and duration describe the displayed collection. | Passed: Metalcore 110 / 6:39, External Covers 93 / 6:11, Release Radar 30 / 1:47; filtered library counts also matched the chosen scope. |
| Rate a playing playlist track, then edit its genre through Get Info. | Preserve scroll, selection, and playing highlight. | Passed for a four-star rating and blank genre → Metalcore. Both edits were restored afterward. |
| Inspect keyboard focus in light and dark themes. | Visible solid focus outline. | Passed visually. Sidebar name-prefix behavior also has automated coverage for ordinary/shadow playlists, arrow navigation, and avoiding text-input interception. |

## Preferences

| Procedure | Expected result | Observed result |
| --- | --- | --- |
| Open the separate Preferences window and use native zoom/maximize, then restore it. | Independent native window can enlarge and restore. | Passed; it expanded beyond the main window's dimensions. |
| Change settings and Cancel. | Discard draft changes. | Passed. |
| Enable Ignore leading “The” and Save. | Artist and album sorting use the setting. | Passed: The Barden Bellas and The Beginning Of The End appeared among B entries. |
| Change zoom from 100% to 80% and back. | Preferences and application remain usable. | Passed. |
| Click a label and select all. | Labels do not become selectable text. | No label text selection observed. |

Native title-bar dragging and manual edge resizing/minimum-size enforcement could not be conclusively verified through the automation driver. Earlier browser layout checks covered 680×640 and 480×360, with scrolling and the footer accessible; these do not establish native window behavior.

## Automated checks and installation

- Final frontend suite: 98 Node tests and 122 Vitest tests passed; six existing tests skipped.
- Lint, TypeScript/Vite build, native release app build, and whitespace checks passed.
- Full Rust workspace tests passed with the required local socket access. The opt-in live session-recovery probe also passed; other live/audio tests remain opt-in.
- Strict workspace Clippy passed; the existing atomic API deprecations are corrected by the latest main-branch change incorporated into this PR.
- The release bundle was ad-hoc signed, passed strict deep signature verification, copied to Applications, and launched successfully.

A follow-up review also found that unrelated settings saves cancelled playback intent. Settings now request a backend switch only when that setting changes; a focused regression test covers ordinary preference changes and an explicit backend switch.

## Test side effects and limitations

One initial Follow You playback attempt hit a native decoder/cache failure and skipped to MADARA. The application log recorded an invalid MPEG/end-of-stream failure followed by session refresh/cache eviction. A retry played Follow You successfully. Investigation identified an AP connection closure and audio-key timeout immediately before this error: librespot attempted to decode encrypted bytes without the key. The controller now reconnects and retries that same load once. An opt-in native fault-injection test forced an unavailable load on a disconnected session and verified Follow You decoded past one second without advancing the queue; it passed using muted output and a temporary cache.

The temporary Release Radar shadow playlist was removed. Follow You was unsaved from Spotify and moved to Retune's Removed tracks, restoring the library count to 7,966; its soft-removed metadata remains. Temporary genre/rating edits and preferences were restored. The installed app was left open with playback stopped, light theme, 100% zoom, Ignore leading “The” disabled, shuffle enabled, and repeat-all enabled. Playback testing may have generated normal listening history/scrobbles.
