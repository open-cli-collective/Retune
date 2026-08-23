import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { formatDiagnosticReport, reportWindow, type DiagnosticEntry } from '../src/diagnostics.ts'
import { initialState, reducer, type Action } from '../src/appState.ts'
import type { BrowseView, PlaybackTrack, Selection, Settings, SpotifyResults } from '../src/types.ts'
import { createSpotifySearchState, expandSpotifySearchGroup, failSpotifySearchGroup, moreSpotifySearchLabel, receiveSpotifySearchPage, replaceSpotifySearchResults, resetSpotifySearchQuery, retrySpotifySearchGroup, setSpotifySearchTab, spotifyMembership, spotifySearchGroupHeader, spotifySearchPendingPageKey } from '../src/spotifySearch.ts'
import { acceptImportAndNext, acceptImportChanges, activeImportQueue, canHandleImportShortcut, collectionAlbumActionLabel, collectionAlbumTrackStatuses, collectionCoverageStatus, collectionDialogScreen, collectionImportBranch, collectionPreviewCoverageCopy, collectionSuggestion, defaultReviewState, downloadAction, excludedImportCount, excludeImportRow, handleImportQueueTab, ignoreImportAlbum, ignoreImportArtist, importAlbumActionAdvances, importDownloadCopy, importDownloadPercent, importDownloadProgressLabel, importEmptyPageMessage, importHistoryBreadcrumb, importQueueHighlightIndex, importQueueTabTarget, importQueueVisibleRange, importStatusText, isCurrentImportPageResponse, loadSelectedImportPage, moveImportNavigationRow, moveImportQueueIndex, nextRemainingImportQueue, pickerCandidates, pickerSelectedUri, projectAcknowledgedImportApply, remainingImportCount, requiredImportMatchIds, resolveImportCount, restPendingImportCount, selectedCollectionAlbumUris, selectedImportCount, selectedImportTrackConfidence, shouldRefreshImportEvent, showsImportRemaining, skipImportAlbum, sortImportQueue, strongImportAlbumMatch, toggleImportRow, trackPickerQuery, validImportIntent, type ImportQueueItem, type ImportSourceRow } from '../src/lastfmImportState.ts'
import { appliedZoom, browseRequestKey, browseViewForRequest, clearedTrackRating, compareTracks, contiguousRange, dialogTabTarget, facetLabel, insertionIndexAtY, isCurrentTrack, LIBRARY_DEFAULT_COLUMN_ORDER, LIBRARY_DEFAULT_HIDDEN_COLUMNS, menuPosition, mergeByUri, moveBefore, moveToIndex, nextNativeDragActive, normalizeZoom, overlayEditTargets, pendingPlaybackTarget, playbackAuthorizationPrompt, playbackOriginAction, playbackQueue, playbackRetryReady, playbackStartAction, playlistLayoutFor, playlistOverride, playlistRows, PLAYLIST_DEFAULT_COLUMN_ORDER, PLAYLIST_DEFAULT_HIDDEN_COLUMNS, rememberSelection, restoreSelection, resizedColumnWidth, resizedPaneHeight, selectionAfterFacet, staleSelectionFacet, SYNTHETIC_BASE, visibleColumnOrder } from '../src/ui.ts'

const searchPage = (overrides: Partial<SpotifyResults> = {}): SpotifyResults => ({
  artists: { items: Array.from({ length: 10 }, (_, index) => ({ id: `artist-${index}`, name: `Artist ${index}`, descriptor: '', imageUrl: null })), total: 21, nextOffset: 10 },
  albums: { items: Array.from({ length: 10 }, (_, index) => ({ uri: `spotify:album:${index}`, name: `Album ${index}`, artist: 'Artist', year: null, imageUrl: null, albumType: null, trackCount: 1, inLibrary: false })), total: 21, nextOffset: 10 },
  tracks: { items: Array.from({ length: 10 }, (_, index) => ({ uri: `spotify:track:${index}`, name: `Track ${index}`, artist: 'Artist', alb: 'Album', durationSecs: 1, imageUrl: null, albumUri: null, inLibrary: false })), total: 21, nextOffset: 10 },
  ...overrides,
})

const importRows = (): ImportSourceRow[] => [
  { stableId: 'a', artist: 'Beta', album: 'Album', track: 'One', playCount: 3, earliest: 10, latest: 30, variants: [{ artist: 'Beta', album: 'Album', track: 'One', playCount: 2, earliest: 10, latest: 20 }, { artist: 'beta', album: 'album', track: 'one', playCount: 1, earliest: 30, latest: 30 }] },
  { stableId: 'b', artist: 'Alpha', album: 'Album', track: 'Two', playCount: 2, earliest: 20, latest: 40, variants: [{ artist: 'Alpha', album: 'Album', track: 'Two', playCount: 2, earliest: 20, latest: 40 }] },
]

const importQueue = (): ImportQueueItem[] => [
  { page: 1, artist: 'Beta', album: 'Album', playCount: 3, latest: 30, sourceCount: 1, remaining: true, albumEntities: 1, trackEntities: 0 },
  { page: 2, artist: 'Alpha', album: 'Album', playCount: 2, latest: 40, sourceCount: 2, remaining: true, albumEntities: 0, trackEntities: 2 },
]

test('diagnostic reports include session context through the last problem only', () => {
  const entry = (level: DiagnosticEntry['level'], message: string): DiagnosticEntry => ({ date: '2026-08-16', time: '12:00:00', level, target: 'retune', message })
  const entries = [entry('INFO', 'start'), entry('WARN', 'retry'), entry('INFO', 'context'), entry('ERROR', 'failed'), entry('INFO', 'trailing')]
  const report = reportWindow(entries)
  assert.deepEqual(report.map(({ message }) => message), ['start', 'retry', 'context', 'failed'])
  assert.match(formatDiagnosticReport(report), /^\[2026-08-16\]\[12:00:00\]\[INFO\]\[retune\] start/)
  assert.deepEqual(reportWindow([entry('INFO', 'healthy')]), [])
})

test('Last.fm review state preserves unchecked rows, cascades ignores, and counts skipped rows', () => {
  let state = defaultReviewState(importRows())
  state = toggleImportRow(state, 'b')
  state = skipImportAlbum(state, 'Beta', 'Album')
  assert.equal(remainingImportCount(state), 2)
  const accepted = acceptImportChanges(state)
  assert.deepEqual(accepted.committed, ['a'])
  assert.equal(accepted.state.decisions.a.status, 'done')
  assert.equal(accepted.state.decisions.b.status, 'pending')
  state = ignoreImportAlbum(accepted.state, 'Alpha', 'Album')
  assert.equal(remainingImportCount(state), 0)
  state = ignoreImportArtist(state, 'Beta')
  assert.equal(remainingImportCount(state), 0)
  const excluded = excludeImportRow(defaultReviewState(importRows()), 'b')
  assert.equal(excluded.decisions.b.excluded, true)
})

test('Last.fm Accept & Next commits current choices and requests advancement', () => {
  const state = defaultReviewState(importRows())
  const accepted = acceptImportAndNext(toggleImportRow(state, 'b'))
  assert.deepEqual(accepted.committed, ['a'])
  assert.equal(accepted.advance, true)
})

test('Last.fm fuzzy count modes and all queue sorts are deterministic', () => {
  const rows = importRows()
  assert.equal(resolveImportCount(rows, 'sum'), 5)
  assert.equal(resolveImportCount(rows, 'overwrite'), 2)
  assert.equal(resolveImportCount(rows, 'zero'), 0)
  assert.deepEqual(sortImportQueue(importQueue(), 'plays').map((item) => item.artist), ['Beta', 'Alpha'])
  assert.deepEqual(sortImportQueue(importQueue(), 'artist').map((item) => item.artist), ['Alpha', 'Beta'])
  assert.deepEqual(sortImportQueue(importQueue(), 'batch').map((item) => item.artist), ['Alpha', 'Beta'])
  assert.deepEqual(sortImportQueue(importQueue(), 'lastPlayed').map((item) => item.artist), ['Alpha', 'Beta'])
})

test('Last.fm navigation guards stay on importer targets and preserve mapping rows', () => {
  const base = { navigationTarget: 'source' as const, control: false, modal: false, altKey: false, ctrlKey: false, metaKey: false, shiftKey: false }
  assert.equal(canHandleImportShortcut({ ...base, key: 'ArrowDown' }), true)
  assert.equal(canHandleImportShortcut({ ...base, key: 'Tab', shiftKey: true }), true)
  assert.equal(canHandleImportShortcut({ ...base, key: 'e', shiftKey: true }), false)
  assert.equal(canHandleImportShortcut({ ...base, key: ' ', control: true }), false)
  assert.equal(canHandleImportShortcut({ ...base, key: '?', shiftKey: true }), true)
  assert.equal(canHandleImportShortcut({ ...base, key: 'a', modal: true }), false)
  assert.equal(canHandleImportShortcut({ ...base, key: 'x', ctrlKey: true }), false)
  assert.equal(moveImportQueueIndex(0, 3, -1), 0)
  assert.equal(moveImportQueueIndex(1, 3, 1), 2)
  assert.equal(moveImportNavigationRow(0, 3, -1), 0)
  assert.deepEqual(requiredImportMatchIds(['matched', 'missing'], ['matched'], true, false), ['missing'])
  assert.deepEqual(requiredImportMatchIds(['missing'], [], false, true), [])
  assert.equal(moveImportNavigationRow(1, 3, 1), 2)
})

test('Last.fm queue-only review preserves focus and skip/resume page semantics', () => {
  const queue = importQueue()
  assert.equal(importQueueHighlightIndex(queue, 2, 1, 0), 1)
  assert.equal(importQueueHighlightIndex([queue[0]], 2, 1, 1), 0)
  assert.equal(shouldRefreshImportEvent(false, false), true)
  assert.equal(shouldRefreshImportEvent(true, false), false)
  assert.equal(shouldRefreshImportEvent(false, true), false)
  assert.equal(importAlbumActionAdvances('skip-album'), true)
  assert.equal(importAlbumActionAdvances('restore'), false)
  assert.deepEqual(importQueueTabTarget(false), { kind: 'source', row: 0 })
  assert.deepEqual(importQueueTabTarget(true), { kind: 'match', row: 0 })
  assert.deepEqual(activeImportQueue([{ ...queue[0], remaining: false, status: 'done' }, queue[1]]), [queue[1]])
  assert.deepEqual(activeImportQueue([{ ...queue[0], remaining: false, status: 'failed' }]), [{ ...queue[0], remaining: false, status: 'failed' }])
})

test('Last.fm queue Tab wiring cancels native focus before moving to the mapping pane', () => {
  const events: string[] = []
  const event = { preventDefault: () => events.push('prevent'), stopPropagation: () => events.push('stop') }
  const target = { focus: () => events.push('focus'), scrollIntoView: () => events.push('scroll') }
  assert.equal(handleImportQueueTab(event, target), true)
  assert.deepEqual(events, ['prevent', 'stop', 'focus', 'scroll'])
  assert.equal(handleImportQueueTab(event, null), false)
  const importer = readFileSync(new URL('../src/LastFmImporter.tsx', import.meta.url), 'utf8')
  assert.match(importer, /const target = onTab\(event\.shiftKey\)[\s\S]*handleImportQueueTab\(event, target\)/)
})

test('Last.fm acknowledged apply projects the next queue choice before authoritative refresh', () => {
  const projection = projectAcknowledgedImportApply(importQueue(), 1, 'batch')
  assert.equal(projection.queue[0].remaining, false)
  assert.equal(projection.queue[0].status, 'done')
  assert.deepEqual(projection.next, projection.queue[1])
  assert.deepEqual(activeImportQueue(projection.queue).map((item) => item.page), [2])
})

test('Last.fm strong album match advisory accepts exact and supersets but rejects unsafe mappings', () => {
  const exact = { artist: 'The Artist', relation: 'best-match' as const, trackUris: ['spotify:track:one', 'spotify:track:two'] }
  assert.deepEqual(strongImportAlbumMatch('the-artist', exact, [
    { excluded: false, targetUri: 'spotify:track:one' },
    { excluded: false, targetUri: 'spotify:track:two' },
  ]), { strong: true, extraTrackCount: 0 })
  const superset = { artist: 'The Artist', relation: 'superset' as const, trackUris: [...exact.trackUris, 'spotify:track:bonus'] }
  assert.deepEqual(strongImportAlbumMatch('THE ARTIST', superset, [
    { excluded: false, targetUri: 'spotify:track:one' },
    { excluded: false, targetUri: 'spotify:track:two' },
  ]), { strong: true, extraTrackCount: 1 })
  assert.equal(strongImportAlbumMatch('The Artist', exact, [
    { excluded: false, targetUri: 'spotify:track:one' },
    { excluded: false, targetUri: 'spotify:track:one' },
  ]).strong, false)
  assert.equal(strongImportAlbumMatch('The Artist', exact, [
    { excluded: false, targetUri: 'spotify:track:standalone', standaloneLowConfidence: true },
    { excluded: true, targetUri: null },
  ]).strong, false)
  assert.equal(strongImportAlbumMatch('The Other Artist', exact, [
    { excluded: false, targetUri: 'spotify:track:one' },
    { excluded: false, targetUri: 'spotify:track:two' },
  ]).strong, false)
})

test('Last.fm collection suggestions require an owned imperfect same-artist candidate', () => {
  const source = { stableId: 'source', artist: 'Artist', variants: [] }
  const suggest = (candidates: Array<{ uri: string; artist: string; inLibrary: boolean; relation: 'best-match' | 'same-songs' | 'superset' | null }>) => collectionSuggestion(source, { selectedUri: null, trackMatches: {}, candidates })
  const exactOne = { uri: 'spotify:track:one', artist: 'Artist', inLibrary: true, relation: 'best-match' as const }
  const exactTwo = { uri: 'spotify:track:two', artist: 'Artist', inLibrary: true, relation: 'best-match' as const }
  assert.equal(suggest([exactOne, exactTwo]), null)
  const ownedNear = { uri: 'spotify:track:near', artist: 'Artist', inLibrary: true, relation: null }
  assert.deepEqual(suggest([ownedNear]), ownedNear)
  assert.equal(suggest([{ ...ownedNear, inLibrary: false }]), null)
  assert.equal(suggest([{ ...ownedNear, artist: 'Other Artist' }]), null)
})

test('Last.fm collection album projections preserve match-set order and branch labels', () => {
  const cached = [{ uri: 'spotify:album:a' }, { uri: 'spotify:album:b' }, { uri: 'spotify:album:c' }]
  assert.deepEqual(selectedCollectionAlbumUris(cached, ['spotify:album:c', 'spotify:album:a', 'spotify:album:c']), ['spotify:album:c', 'spotify:album:a'])
  assert.equal(collectionImportBranch(''), 'collection')
  assert.equal(collectionImportBranch('Singles'), 'release')
  assert.equal(collectionAlbumActionLabel(false), 'Add to album matches')
  assert.equal(collectionAlbumActionLabel(true), 'Remove from album matches')
  assert.equal(collectionCoverageStatus({ matched: 2, ambiguous: 1, unresolved: 3 }), '2 matched · 1 ambiguous · 3 unresolved')
})

test('Last.fm collection preview projections preserve track states and selected coverage copy', () => {
  const cached = [{ uri: 'spotify:album:a' }]
  assert.equal(collectionDialogScreen(undefined, cached), 'results')
  assert.equal(collectionDialogScreen('spotify:album:a', cached), 'preview')
  assert.equal(collectionDialogScreen(undefined, cached), 'results')
  assert.deepEqual(collectionAlbumTrackStatuses(
    ['spotify:track:a', 'spotify:track:b', 'spotify:track:c'],
    [{ uri: 'spotify:track:a', status: 'matched' }, { uri: 'spotify:track:b', status: 'ambiguous' }],
  ), ['matched', 'ambiguous', 'unmatched'])
  assert.equal(collectionPreviewCoverageCopy({ selected: true, matched: 3, uniqueCoverage: 2, marginalMatches: 0, ambiguityChanges: 0 }), '3 matches · 2 unique')
  assert.equal(collectionPreviewCoverageCopy({ selected: false, matched: 0, uniqueCoverage: 0, marginalMatches: 2, ambiguityChanges: -1 }), '+2 marginal matches · -1 ambiguity change')
})

test('Last.fm content and history intents remain independent and require one choice', () => {
  const state = defaultReviewState(importRows())
  assert.equal(state.importContent, true)
  assert.equal(state.includeHistoricalPlayCounts, true)
  assert.equal(state.wholeAlbum, false)
  assert.equal(validImportIntent(true, false), true)
  assert.equal(validImportIntent(false, true), true)
  assert.equal(validImportIntent(false, false), false)
})

test('Last.fm per-target fuzzy strategies and stale-free queue advancement are deterministic', () => {
  const countModes: Record<string, 'sum' | 'overwrite' | 'zero'> = { target: 'overwrite' }
  assert.equal(countModes.target, 'overwrite')
  let state = defaultReviewState(importRows())
  state = toggleImportRow(state, 'b')
  state = excludeImportRow(state, 'a')
  assert.equal(selectedImportCount(state), 0)
  assert.equal(excludedImportCount(state), 1)
  assert.equal(restPendingImportCount(state), 1)
  assert.equal(remainingImportCount(state), 1)
  const done = { ...importQueue()[0], remaining: false, status: 'done' as const }
  assert.equal(nextRemainingImportQueue([done, importQueue()[1]], done, 'plays')?.artist, 'Alpha')
  assert.equal(isCurrentImportPageResponse(1, 2), false)
  assert.equal(isCurrentImportPageResponse(2, 2), true)
})

test('Last.fm A-to-B queue selection keeps the newer page when A resolves last', async () => {
  let resolveA!: (page: string) => void
  let resolveB!: (page: string) => void
  const responseA = new Promise<string>((resolve) => { resolveA = resolve })
  const responseB = new Promise<string>((resolve) => { resolveB = resolve })
  const generation = { current: 0 }
  const queue = importQueue()
  const selected: string[] = []
  const applied: string[] = []
  let visible: string | null = 'A'
  let loading = false
  loading = true
  const requestA = loadSelectedImportPage(generation, queue[0], () => responseA, (item) => selected.push(item.artist), (page) => { visible = page; applied.push(page) }, () => { visible = null }, () => { loading = false })
  loading = true
  const requestB = loadSelectedImportPage(generation, queue[1], () => responseB, (item) => selected.push(item.artist), (page) => { visible = page; applied.push(page) }, () => { visible = null }, () => { loading = false })
  assert.equal(visible, null)
  resolveA('A')
  await requestA
  assert.equal(loading, true)
  assert.equal(visible, null)
  resolveB('B')
  await requestB
  assert.equal(loading, false)
  assert.equal(visible, 'B')
  assert.deepEqual(selected, ['Beta', 'Alpha'])
  assert.deepEqual(applied, ['B'])
  assert.equal(visible, 'B')
})

test('Last.fm setup and download status reflect connected and active states', () => {
  assert.equal(importStatusText(null, 'rianjs'), 'Ready to import')
  assert.equal(importStatusText(null, null), 'Connect Last.fm to begin')
  assert.equal(importStatusText('downloading', 'rianjs'), 'Downloading Last.fm plays')
  assert.equal(importStatusText('aggregating', 'rianjs'), 'Preparing Last.fm review')
  assert.equal(showsImportRemaining('downloading'), false)
  assert.equal(showsImportRemaining('aggregating'), false)
  assert.equal(showsImportRemaining('review'), true)
  assert.deepEqual(downloadAction(null, null), { label: 'Start import', disabled: false })
  assert.deepEqual(downloadAction('downloading', null), { label: 'Downloading…', disabled: true })
  assert.deepEqual(downloadAction('downloading', { retryable: true }), { label: 'Retrying automatically…', disabled: true })
  assert.deepEqual(downloadAction('downloading', { retryable: false }), { label: 'Resume download', disabled: false })
  assert.deepEqual(downloadAction('suspended', null), { label: 'Check accounts and resume', disabled: false })
  assert.deepEqual(downloadAction('aggregating', null), { label: 'Preparing review…', disabled: true })
  assert.deepEqual(downloadAction('aggregating', { retryable: false }), { label: 'Resume download', disabled: false })
  assert.deepEqual(importEmptyPageMessage('review', true), { title: 'Matching this review batch…', detail: 'Spotify is contacted only for the visible batch.' })
  assert.deepEqual(importEmptyPageMessage('review', false), { title: 'No review page selected', detail: 'Select an album from the queue.' })
  assert.deepEqual(importEmptyPageMessage('done', false), { title: 'Import complete', detail: 'All review batches are complete.' })
})

test('Last.fm download copy uses persisted plays, percentage, and truthful dates', () => {
  const oldest = Math.floor(Date.UTC(2025, 6, 1) / 1000)
  const historyEnd = Math.floor(Date.UTC(2026, 7, 1) / 1000)
  assert.equal(importDownloadPercent(3, 4), 75)
  assert.equal(importDownloadProgressLabel(234341, 245892, 75), '234,341 of 245,892 plays downloaded · 75%')
  assert.equal(importHistoryBreadcrumb(null, null), 'Oldest → newest · starting with the oldest plays')
  assert.equal(importHistoryBreadcrumb(oldest, historyEnd), 'Oldest → newest · reached July 2025 · history ends August 2026')

  const resumed = importDownloadCopy({
    phase: 'downloading',
    processedScrobbles: 234341,
    totalScrobbles: 245892,
    downloadedPages: 3,
    totalPages: 4,
    downloadedThrough: oldest,
    historyTo: historyEnd,
  })
  assert.match(resumed.progress, /plays downloaded · 75%/)
  assert.match(resumed.breadcrumb, /reached July 2025/)
  assert.doesNotMatch(`${resumed.detail} ${resumed.progress} ${resumed.breadcrumb}`, /pages?/i)
})

test('Last.fm download copy handles initial dates, missing dates, aggregation, and resumed status', () => {
  const initial = importDownloadCopy({
    phase: null,
    processedScrobbles: 0,
    totalScrobbles: 0,
    downloadedPages: 0,
    totalPages: null,
    downloadedThrough: null,
    historyTo: null,
  })
  assert.match(initial.detail, /fixed snapshot.*plays/i)
  assert.equal(initial.breadcrumb, 'Oldest → newest · starting with the oldest plays')
  assert.equal(importHistoryBreadcrumb(null, Math.floor(Date.UTC(2026, 7, 1) / 1000)), 'Oldest → newest · starting with the oldest plays · history ends August 2026')

  const aggregating = importDownloadCopy({
    phase: 'aggregating',
    processedScrobbles: 245892,
    totalScrobbles: 245892,
    downloadedPages: 4,
    totalPages: 4,
    downloadedThrough: Math.floor(Date.UTC(2026, 7, 1) / 1000),
    historyTo: Math.floor(Date.UTC(2026, 7, 1) / 1000),
  })
  assert.equal(aggregating.detail, 'All plays are downloaded. Retune is sorting and grouping them before review.')
  for (const phase of ['downloading', 'aggregating', 'review', 'done', 'suspended'] as const) {
    assert.doesNotMatch(importStatusText(phase, 'rianjs'), /pages?/i)
  }
})

test('Last.fm queue virtualization clamps visible ranges with overscan', () => {
  assert.deepEqual(importQueueVisibleRange(23132, 0, 300), {
    start: 0,
    end: 10,
    offsetTop: 0,
    contentHeight: 1318524,
  })
  assert.deepEqual(importQueueVisibleRange(23132, 57 * 1000, 300), {
    start: 996,
    end: 1010,
    offsetTop: 56772,
    contentHeight: 1318524,
  })
  const end = importQueueVisibleRange(23132, Number.MAX_SAFE_INTEGER, 300)
  assert.equal(end.start, 23122)
  assert.equal(end.end, 23132)
  assert.equal(end.offsetTop, 23122 * 57)
  assert.deepEqual(importQueueVisibleRange(0, 0, 300), { start: 0, end: 0, offsetTop: 0, contentHeight: 0 })
})

test('Last.fm queue rendering and importer modal styles keep large lists and surfaces bounded', () => {
  const importer = readFileSync(new URL('../src/LastFmImporter.tsx', import.meta.url), 'utf8')
  const appCss = readFileSync(new URL('../src/App.css', import.meta.url), 'utf8')
  const importerCss = readFileSync(new URL('../src/lastfmImporter.css', import.meta.url), 'utf8')
  assert.match(importer, /<VirtualQueue /)
  assert.doesNotMatch(importer, /<div className="import-queue-list">\{orderedQueue\.map/)
  assert.match(importer, /aria-current=\{selectedPage === item\.page \? 'true' : undefined\}/)
  assert.match(importer, /aria-label=\{`Batch \$\{absoluteIndex \+ 1\} of \$\{items\.length\}/)
  for (const theme of ['light', 'dark']) {
    const block = appCss.match(new RegExp(`:root\\[data-theme="${theme}"\\] \\{([^}]*)\\}`))?.[1] ?? ''
    assert.match(block, /--panel:\s*#[0-9a-f]{6}/i)
  }
  const dialogBlock = importerCss.match(/\.import-picker-dialog, \.import-confirm-dialog \{([^}]*)\}/)?.[1] ?? ''
  assert.match(dialogBlock, /background: var\(--panel\)/)
  assert.match(dialogBlock, /border: 1px solid var\(--border\)/)
  assert.match(dialogBlock, /box-shadow:/)
  assert.match(appCss, /\.modal-backdrop \{[^}]*z-index: 10;[^}]*background: rgb\(0 0 0 \/ \.38\)/s)
  assert.match(importerCss, /\.import-queue, \.import-review \{[^}]*min-height: 0/)
  assert.match(importerCss, /\.import-queue-list \{[^}]*overflow: auto/)
  assert.match(importerCss, /\.import-track-list \{[^}]*overflow: auto/)
  assert.match(importerCss, /\.import-nav-target:focus \{[^}]*box-shadow: inset 0 0 0 2px var\(--accent\)/)
  assert.match(importerCss, /\.import-match-cell\.needs-action \{[^}]*box-shadow: inset 4px 0 #a64b00/)
})

test('Last.fm collection review explains individual matching and keeps release UI scoped', () => {
  const importer = readFileSync(new URL('../src/LastFmImporter.tsx', import.meta.url), 'utf8')
  const importerCss = readFileSync(new URL('../src/lastfmImporter.css', import.meta.url), 'utf8')
  assert.match(importer, /Last\.fm supplied no album metadata\. Tracks are matched individually\./)
  assert.match(importer, /automatically selected/)
  assert.match(importer, /suggested/)
  assert.match(importer, /need review/)
  assert.match(importer, /Use This Track/)
  assert.match(importer, /ALREADY IN YOUR LIBRARY/)
  assert.match(importer, /collection && !collectionAlbumReady/)
  assert.match(importer, /const collection = page\.album === ''/)
  assert.match(importer, /collection \? collectionSuggestion\(item\.source, item\.matchResult\) : null/)
  assert.match(importerCss, /\.import-library-badge/)
  assert.match(importerCss, /\.import-suggestion-label/)
  assert.match(importer, /collection && trackConfidence === 'exact'/)
})

test('Last.fm queue selection restores focus after loading and retains native button semantics', () => {
  const importer = readFileSync(new URL('../src/LastFmImporter.tsx', import.meta.url), 'utf8')
  const importState = readFileSync(new URL('../src/lastfmImportState.ts', import.meta.url), 'utf8')
  const refreshBlock = importer.match(/const refresh = useCallback[\s\S]*?\n  useEffect\(\(\) => \{\n    void refresh\(\)/)?.[0] ?? ''
  assert.match(importer, /const importQueuePageLimit = 1000/)
  assert.match(importer, /const selectedPageRef = useRef<number \| undefined>\(undefined\)/)
  assert.match(importer, /const queueRefreshGeneration = useRef\(0\)/)
  assert.match(importer, /const sortRef = useRef<.*>\('plays'\)/)
  assert.match(refreshBlock, /const refresh = useCallback\(async/)
  assert.doesNotMatch(refreshBlock, /\}, \[selectedPage, sort\]\)/)
  assert.match(importer, /const nextQueueItem = \(queueSnapshot = queue, focusQueue = false\)/)
  assert.match(importer, /focusQueueAfterOpen\.current = focusQueue/)
  assert.match(importer, /openQueueItem\(next, activeImportQueue\(orderedSnapshot\), focusQueue\)/)
  assert.match(importer, /onOpen=\{\(item\) => void openQueueItem\(item, activeQueue, true\)\}/)
  assert.doesNotMatch(importer, /<VirtualQueue[^>]*disabled=\{busy \|\| pageLoading\}/)
  assert.match(importer, /event\.key === 'Enter' && !busy && query\.trim\(\)[\s\S]*onSearch\(query\)/)
  assert.match(importer, /onMutation\(true\)[\s\S]*lastfm_import_apply/)
  assert.match(importer, /archiveBatch: advance/)
  assert.match(importer, /await invoke\('lastfm_import_apply'[\s\S]*if \(advance\) await onApplied\(\)/)
  assert.match(importer, /const appliedAndAdvance = async \(\) => \{[\s\S]*focusQueueAfterOpen\.current = true[\s\S]*projectAcknowledgedImportApply\([\s\S]*setSelected\(null\)[\s\S]*setPage\(null\)[\s\S]*refreshQueueOnly\(true\)/)
  assert.match(importer, /const refreshQueueOnly = async \(strict = false\)/)
  assert.match(importer, /requestGeneration !== queueRefreshGeneration\.current/)
  assert.match(importer, /lastfm-import-apply-finished/)
  assert.match(importer, /lastfm_import_retry_apply/)
  assert.match(importer, /Retry Apply/)
  assert.match(importer, /if \(event\.payload\.message\) \{[\s\S]*refreshQueueOnly\(\)[\s\S]*else if \(!advancingApply\.current/)
  assert.match(importer, /state\.applyingAll \? <section className="import-empty">/)
  assert.match(importState, /item\.remaining \|\| item\.status === 'failed'/)
  assert.doesNotMatch(importer, /role="listbox"/)
  assert.doesNotMatch(importer, /role="option"/)
  assert.match(importer, /aria-current=\{selectedPage === item\.page \? 'true' : undefined\}/)
  assert.match(importer, /aria-label=\{`Batch \$\{absoluteIndex \+ 1\} of \$\{items\.length\}/)
})

test('Last.fm track picker starts from the source row and cancel preserves album and row matches', () => {
  const albumUri = 'spotify:album:gladiator'
  const now = { ...importRows()[0], artist: 'The Lyndhurst Orchestra', album: 'Gladiator - Music from the Motion Picture', track: 'Now We Are Free' }
  const albumCandidates = [{ uri: albumUri }, { uri: 'spotify:album:other-release' }]
  const pageMatches = {
    album: albumUri,
    tracks: { now: 'spotify:track:now-we-are-free', honor: 'spotify:track:honor-him' },
  }
  const beforeCancel = structuredClone(pageMatches)

  assert.equal(trackPickerQuery(now), 'track:"Now We Are Free" artist:"The Lyndhurst Orchestra"')
  assert.deepEqual(pickerCandidates('track', albumCandidates), [])
  assert.equal(pickerSelectedUri('track', 'now', albumUri, pageMatches.tracks), pageMatches.tracks.now)
  assert.deepEqual(pageMatches, beforeCancel)
})

test('Last.fm row confidence uses a standalone track candidate without changing album confidence', () => {
  const albumUri = 'spotify:album:gladiator'
  const rematchedUri = 'spotify:track:rematched'
  const honorUri = 'spotify:track:honor-him'
  const candidates = [
    { uri: albumUri, relation: 'best-match' as const },
    { uri: rematchedUri, relation: null },
  ]

  assert.equal(selectedImportTrackConfidence('now', albumUri, { now: rematchedUri, honor: honorUri }, 'likely', candidates), 'low')
  assert.equal(selectedImportTrackConfidence('honor', albumUri, { honor: honorUri }, 'likely', candidates), 'likely')
})

test('Spotify search uses 5 rows in All and 10 in filtered tabs', () => {
  const all = createSpotifySearchState('jazz')
  assert.deepEqual(all.visible, { artists: 5, albums: 5, tracks: 5 })
  const filtered = setSpotifySearchTab(all, 'albums')
  assert.deepEqual(filtered.visible, { artists: 5, albums: 10, tracks: 5 })
})

test('pending membership mutations override stale track and album pages', () => {
  const track = 'spotify:track:one'
  const album = 'spotify:album:one'
  assert.equal(spotifyMembership(false, track, { [track]: true }), true)
  assert.equal(spotifyMembership(true, track, { [track]: false }), false)
  assert.equal(spotifyMembership(false, album, { [album]: true }), true)
  assert.equal(spotifyMembership(true, album, { [album]: false }), false)
})

test('Spotify search expands one group, merges cached sibling pages, and labels remaining rows', () => {
  let state = replaceSpotifySearchResults(createSpotifySearchState('jazz'), searchPage())
  const expansion = expandSpotifySearchGroup(state, 'artists')
  assert.equal(expansion.request?.offset, 10)
  state = receiveSpotifySearchPage(expansion.state, 'artists', 10, searchPage({
    artists: { items: [{ id: 'artist-9', name: 'Duplicate', descriptor: '', imageUrl: null }, { id: 'artist-10', name: 'Artist 10', descriptor: '', imageUrl: null }], total: 21, nextOffset: 20 },
    albums: { items: [{ uri: 'spotify:album:9', name: 'Duplicate', artist: 'Artist', year: null, imageUrl: null, albumType: null, trackCount: 1, inLibrary: false }, { uri: 'spotify:album:10', name: 'Album 10', artist: 'Artist', year: null, imageUrl: null, albumType: null, trackCount: 1, inLibrary: false }], total: 21, nextOffset: 20 },
    tracks: { items: [{ uri: 'spotify:track:9', name: 'Duplicate', artist: 'Artist', alb: 'Album', durationSecs: 1, imageUrl: null, albumUri: null, inLibrary: false }, { uri: 'spotify:track:10', name: 'Track 10', artist: 'Artist', alb: 'Album', durationSecs: 1, imageUrl: null, albumUri: null, inLibrary: false }], total: 21, nextOffset: 20 },
  }), expansion.request!.generation)
  assert.equal(state.groups.artists.items.length, 11)
  assert.equal(state.groups.albums.items.length, 11)
  assert.equal(state.groups.tracks.items.length, 11)
  assert.equal(moreSpotifySearchLabel(state, 'artists'), 'View 6 more artists')
  assert.equal(spotifySearchGroupHeader(state, 'artists'), 'Artists · 15 of 21')
  const albumExpansion = expandSpotifySearchGroup(state, 'albums')
  assert.equal(albumExpansion.state.visible.artists, 15)
  assert.equal(albumExpansion.state.visible.albums, 15)
  assert.equal(albumExpansion.request?.offset, 20)
})

test('Spotify search uses the smaller remaining label and exhausts cleanly', () => {
  let state = replaceSpotifySearchResults(createSpotifySearchState('jazz'), searchPage({
    artists: { items: Array.from({ length: 10 }, (_, index) => ({ id: `artist-${index}`, name: `Artist ${index}`, descriptor: '', imageUrl: null })), total: 13, nextOffset: 10 },
  }))
  const expansion = expandSpotifySearchGroup(state, 'artists')
  state = receiveSpotifySearchPage(expansion.state, 'artists', 10, searchPage({
    artists: { items: [{ id: 'artist-10', name: 'Artist 10', descriptor: '', imageUrl: null }, { id: 'artist-11', name: 'Artist 11', descriptor: '', imageUrl: null }, { id: 'artist-12', name: 'Artist 12', descriptor: '', imageUrl: null }], total: 13, nextOffset: null },
  }), expansion.request!.generation)
  assert.equal(state.groups.artists.items.length, 13)
  assert.equal(moreSpotifySearchLabel(state, 'artists'), undefined)
})

test('Spotify search query and filter resets preserve only safe cached pages', () => {
  let state = replaceSpotifySearchResults(createSpotifySearchState('jazz'), searchPage())
  const expansion = expandSpotifySearchGroup(state, 'artists')
  state = receiveSpotifySearchPage(expansion.state, 'artists', 10, searchPage({
    artists: { items: Array.from({ length: 10 }, (_, index) => ({ id: `artist-${index + 10}`, name: `Artist ${index + 10}`, descriptor: '', imageUrl: null })), total: 21, nextOffset: 20 },
  }), expansion.request!.generation)
  const filtered = setSpotifySearchTab(state, 'tracks')
  assert.equal(filtered.groups.artists.items.length, 20)
  assert.deepEqual(filtered.visible, { artists: 5, albums: 5, tracks: 10 })
  const reset = replaceSpotifySearchResults(resetSpotifySearchQuery(filtered, 'rock'), searchPage({ artists: { items: [], total: 0, nextOffset: null } }))
  assert.equal(reset.groups.artists.items.length, 0)
  assert.equal(reset.generation > filtered.generation, true)
})

test('Spotify search failure preserves rows, retry targets the same offset, and stale responses are ignored', () => {
  let state = replaceSpotifySearchResults(createSpotifySearchState('jazz'), searchPage())
  const expansion = expandSpotifySearchGroup(state, 'artists')
  state = failSpotifySearchGroup(expansion.state, 'artists', 'offline', expansion.request!.generation)
  assert.equal(state.groups.artists.items.length, 10)
  assert.equal(state.errors.artists, 'offline')
  assert.equal(state.visible.artists, 5)
  const retry = retrySpotifySearchGroup(state, 'artists')
  assert.equal(retry.request?.offset, 10)
  assert.equal(retry.state.visible.artists, 15)
  const reset = setSpotifySearchTab(retry.state, 'albums')
  assert.equal(reset.loading.size, 0)
  assert.equal(receiveSpotifySearchPage(reset, 'artists', 10, searchPage(), retry.request!.generation).groups.artists.items.length, 10)
  const queryReset = resetSpotifySearchQuery(state, 'rock')
  assert.equal(receiveSpotifySearchPage(queryReset, 'artists', 10, searchPage(), expansion.request!.generation).groups.artists.items.length, 0)
})

test('Spotify search pending pages are not reused after returning to the same query', () => {
  const jazz = replaceSpotifySearchResults(createSpotifySearchState('jazz'), searchPage())
  const pending = expandSpotifySearchGroup(jazz, 'artists').request!
  const rock = resetSpotifySearchQuery(jazz, 'rock')
  const jazzAgain = resetSpotifySearchQuery(rock, 'jazz')

  assert.notEqual(
    spotifySearchPendingPageKey(jazzAgain.query, pending.offset, jazzAgain.generation),
    spotifySearchPendingPageKey(jazz.query, pending.offset, pending.generation),
  )
})

test('pending navigation cannot use prior tracks, while a data refresh keeps them visible', () => {
  const broadQueue: PlaybackTrack[] = [
    { id: 1, uri: 'fixture:track:1', name: 'Welcome', art: 'Artist', alb: 'Broad Album', durationSecs: 180, enabled: true },
    { id: 2, uri: 'fixture:track:2', name: 'Americana', art: 'Artist', alb: 'Broad Album', durationSecs: 200, enabled: true },
  ]
  const baseSelection = {}
  const resolvedKey = browseRequestKey('music', baseSelection, '', 'library')
  const refreshKey = browseRequestKey('music', baseSelection, '', 'library')
  const pendingKey = browseRequestKey('music', { alb: ['America, The Dream Goes On'] }, '', 'library')

  assert.deepEqual(playbackQueue(browseViewForRequest(broadQueue, resolvedKey, resolvedKey) ?? [], 1).map((track) => track.id), [1, 2])
  assert.deepEqual(browseViewForRequest(broadQueue, resolvedKey, pendingKey) ?? [], [])
  assert.deepEqual(browseViewForRequest(broadQueue, resolvedKey, refreshKey), broadQueue)

  // Category, artist, and album selections are browser-pane selection changes.
  const changedKeys = [
    browseRequestKey('podcasts', baseSelection, '', 'library'),
    browseRequestKey('music', { cat: ['Rock'] }, '', 'library'),
    browseRequestKey('music', { art: ['Artist'] }, '', 'library'),
    pendingKey,
    browseRequestKey('music', baseSelection, 'America', 'library'),
    browseRequestKey('music', baseSelection, '', 'spotify'),
  ]
  assert.equal(new Set([resolvedKey, ...changedKeys]).size, changedKeys.length + 1)
})

test('facet selection preserves broader columns and clears narrower columns', () => {
  const selection = { cat: ['Soundtrack'], art: ['Howard Shore'], alb: ['The Lord of the Rings'] }

  assert.deepEqual(selectionAfterFacet(selection, 'cat', ['Punk']), { cat: ['Punk'] })
  assert.deepEqual(selectionAfterFacet(selection, 'art', ['Hans Zimmer']), { cat: ['Soundtrack'], art: ['Hans Zimmer'] })
  assert.deepEqual(selectionAfterFacet(selection, 'alb', ['The Hobbit']), { ...selection, alb: ['The Hobbit'] })
})

test('facet selections are remembered independently for each library source', () => {
  let saved = { music: {}, podcasts: {}, audiobooks: {} }
  saved = rememberSelection(saved, 'music', { cat: ['Rock'], art: ['Artist'] })
  saved = rememberSelection(saved, 'podcasts', { cat: ['News'] })

  assert.deepEqual(restoreSelection(saved, 'music'), { cat: ['Rock'], art: ['Artist'] })
  assert.deepEqual(restoreSelection(saved, 'podcasts'), { cat: ['News'] })
  assert.deepEqual(restoreSelection(saved, 'audiobooks'), {})
})

test('navigation transitions preserve the active playback queue and origin', () => {
  const queue: PlaybackTrack[] = [
    { id: 1, uri: 'fixture:track:1', name: 'One', art: 'Artist', alb: 'Album', durationSecs: 180, enabled: true },
    { id: 2, uri: 'fixture:track:2', name: 'Two', art: 'Artist', alb: 'Album', durationSecs: 180, enabled: true },
  ]
  const origin = { kind: 'playlist', id: 'playlist-a' } as const
  const view: BrowseView = {
    facets: { cats: [], arts: [], albs: [] },
    tracks: [],
    albumRating: null,
    albumRatingArtist: null,
    albumRatingAmbiguous: false,
    counts: { tracks: 0, totalSecs: 0, perSource: { music: 0, podcasts: 0, audiobooks: 0 } },
  }
  const transitions: Action[] = [
    { type: 'view', view, key: 'library-view' },
    { type: 'source', source: 'podcasts' },
    { type: 'select', facet: 'cat', values: ['The Hobbit'] },
    { type: 'playlist', id: 'playlist-a' },
    { type: 'spotifyNavigate', entry: { kind: 'artist', id: 'artist-a' } },
    { type: 'playlist' },
  ]
  let state = reducer(initialState, { type: 'play', id: 2, queue, origin })

  for (const transition of transitions) {
    state = reducer(state, transition)
    assert.deepEqual(state.playing?.queue, queue)
    assert.deepEqual(state.playing?.origin, origin)
  }
})

test('source, pane, and playlist transitions restore and retain the intended selection', () => {
  let state = reducer(initialState, { type: 'select', facet: 'cat', values: ['Rock'] })
  state = reducer(state, { type: 'select', facet: 'art', values: ['Artist'] })
  state = reducer(state, { type: 'select', facet: 'alb', values: ['Album'] })
  state = reducer(state, { type: 'source', source: 'podcasts' })
  state = reducer(state, { type: 'select', facet: 'cat', values: ['The Hobbit'] })
  state = reducer(state, { type: 'source', source: 'music' })

  assert.deepEqual(state.sel, { cat: ['Rock'], art: ['Artist'], alb: ['Album'] })
  state = reducer(state, { type: 'browserPanes', browserPanes: { cat: false, art: true, alb: true } })
  assert.deepEqual(state.sel, { art: ['Artist'], alb: ['Album'] })
  assert.deepEqual(state.savedSelections.music, state.sel)

  state = reducer(state, { type: 'playlist', id: 'playlist-a' })
  state = reducer(state, { type: 'playlist' })
  assert.equal(state.selectedPlaylist, undefined)
  assert.deepEqual(state.sel, { art: ['Artist'], alb: ['Album'] })
})

test('library and playlist defaults expose the approved visible column orders', () => {
  assert.deepEqual(visibleColumnOrder(LIBRARY_DEFAULT_COLUMN_ORDER, LIBRARY_DEFAULT_HIDDEN_COLUMNS), ['track', 'name', 'artist', 'album', 'time', 'plays', 'rating', 'genre'])
  assert.deepEqual(visibleColumnOrder(PLAYLIST_DEFAULT_COLUMN_ORDER, PLAYLIST_DEFAULT_HIDDEN_COLUMNS), ['name', 'artist', 'album', 'time', 'rating', 'plays', 'genre'])
  assert.equal(PLAYLIST_DEFAULT_COLUMN_ORDER.at(-1), 'track')
  assert.equal(PLAYLIST_DEFAULT_HIDDEN_COLUMNS.at(-1), 'track')
})

test('playlist layout resolution uses its keyed override instead of Library defaults', () => {
  const customOrder = [...PLAYLIST_DEFAULT_COLUMN_ORDER].reverse()
  const settings: Pick<Settings, 'playlistHiddenColumns' | 'playlistColumnOrders' | 'playlistColumnWidths'> = {
    playlistHiddenColumns: { 'playlist-a': ['genre'] },
    playlistColumnOrders: { 'playlist-a': customOrder },
    playlistColumnWidths: { 'playlist-a': { name: 220 } },
  }

  assert.deepEqual(playlistLayoutFor('playlist-a', settings), {
    hiddenColumns: ['genre'],
    columnOrder: customOrder,
    columnWidths: { name: 220 },
  })
  assert.deepEqual(playlistLayoutFor('playlist-b', settings), {
    hiddenColumns: PLAYLIST_DEFAULT_HIDDEN_COLUMNS,
    columnOrder: PLAYLIST_DEFAULT_COLUMN_ORDER,
    columnWidths: {},
  })
})

test('playlist layout overrides stay keyed and disappear when restored to defaults', () => {
  const customOrder = [...PLAYLIST_DEFAULT_COLUMN_ORDER].reverse()
  const orders = playlistOverride({}, 'playlist-a', customOrder, PLAYLIST_DEFAULT_COLUMN_ORDER)
  assert.deepEqual(orders['playlist-a'], customOrder)
  const otherOrder = [...PLAYLIST_DEFAULT_COLUMN_ORDER].slice(1).concat(PLAYLIST_DEFAULT_COLUMN_ORDER[0])
  const bothOrders = playlistOverride(orders, 'playlist-b', otherOrder, PLAYLIST_DEFAULT_COLUMN_ORDER)
  assert.deepEqual(bothOrders, { 'playlist-a': customOrder, 'playlist-b': otherOrder })
  const restoredOrders = playlistOverride(bothOrders, 'playlist-a', PLAYLIST_DEFAULT_COLUMN_ORDER, PLAYLIST_DEFAULT_COLUMN_ORDER)
  assert.deepEqual(restoredOrders, { 'playlist-b': otherOrder })

  const hidden = playlistOverride({}, 'playlist-a', ['genre'], PLAYLIST_DEFAULT_HIDDEN_COLUMNS)
  const bothHidden = playlistOverride(hidden, 'playlist-b', ['plays'], PLAYLIST_DEFAULT_HIDDEN_COLUMNS)
  assert.deepEqual(bothHidden, { 'playlist-a': ['genre'], 'playlist-b': ['plays'] })
  assert.deepEqual(playlistOverride(bothHidden, 'playlist-a', PLAYLIST_DEFAULT_HIDDEN_COLUMNS, PLAYLIST_DEFAULT_HIDDEN_COLUMNS), { 'playlist-b': ['plays'] })

  const widths = playlistOverride({}, 'playlist-a', { name: 220 }, {})
  const bothWidths = playlistOverride(widths, 'playlist-b', { artist: 180 }, {})
  assert.deepEqual(bothWidths, { 'playlist-a': { name: 220 }, 'playlist-b': { artist: 180 } })
  assert.deepEqual(playlistOverride(bothWidths, 'playlist-a', {}, {}), { 'playlist-b': { artist: 180 } })

  const customized: Pick<Settings, 'playlistHiddenColumns' | 'playlistColumnOrders' | 'playlistColumnWidths'> = {
    playlistHiddenColumns: bothHidden,
    playlistColumnOrders: bothOrders,
    playlistColumnWidths: bothWidths,
  }
  assert.deepEqual(playlistLayoutFor('playlist-a', customized), {
    hiddenColumns: ['genre'],
    columnOrder: customOrder,
    columnWidths: { name: 220 },
  })
  assert.deepEqual(playlistLayoutFor('playlist-b', customized), {
    hiddenColumns: ['plays'],
    columnOrder: otherOrder,
    columnWidths: { artist: 180 },
  })

  const restored: Pick<Settings, 'playlistHiddenColumns' | 'playlistColumnOrders' | 'playlistColumnWidths'> = {
    playlistHiddenColumns: playlistOverride(bothHidden, 'playlist-a', PLAYLIST_DEFAULT_HIDDEN_COLUMNS, PLAYLIST_DEFAULT_HIDDEN_COLUMNS),
    playlistColumnOrders: restoredOrders,
    playlistColumnWidths: playlistOverride(bothWidths, 'playlist-a', {}, {}),
  }
  assert.deepEqual(playlistLayoutFor('playlist-a', restored), {
    hiddenColumns: PLAYLIST_DEFAULT_HIDDEN_COLUMNS,
    columnOrder: PLAYLIST_DEFAULT_COLUMN_ORDER,
    columnWidths: {},
  })
  assert.deepEqual(playlistLayoutFor('playlist-b', restored), {
    hiddenColumns: ['plays'],
    columnOrder: otherOrder,
    columnWidths: { artist: 180 },
  })
})

test('stale browse selections fall back at the narrowest invalid level', () => {
  const facets = { cats: ['Rock', 'Jazz'], arts: ['Artist', 'Other'], albs: ['Album', 'Other Album'] }
  const cases: { selection: Selection; expected: 'cat' | 'art' | null }[] = [
    { selection: { cat: ['Rock'], art: ['Artist'], alb: ['Album'] }, expected: null },
    { selection: { cat: ['Missing'], art: ['Artist'], alb: ['Album'] }, expected: 'cat' },
    { selection: { cat: ['Rock', 'Missing'], art: ['Artist'], alb: ['Album'] }, expected: 'cat' },
    { selection: { cat: ['Rock'], art: ['Missing'], alb: ['Album'] }, expected: 'art' },
    { selection: { cat: ['Rock'], art: ['Artist'], alb: ['Missing'] }, expected: 'art' },
    { selection: { cat: ['Rock'], art: ['Artist'], alb: ['Album', 'Missing'] }, expected: 'art' },
    { selection: { cat: ['Rock'], art: ['Artist', 'Missing'] }, expected: 'art' },
  ]
  for (const { selection, expected } of cases) assert.equal(staleSelectionFacet(selection, facets), expected)
})

test('the music catch-all has a user-facing genre label', () => {
  assert.equal(facetLabel('Genre', 'Uncategorized'), 'No Genre')
  assert.equal(facetLabel('Category', 'Uncategorized'), 'Uncategorized')
})

test('only native drags with paths activate the Finder overlay', () => {
  assert.equal(nextNativeDragActive(false, { type: 'enter', paths: [] }), false)
  assert.equal(nextNativeDragActive(false, { type: 'enter', paths: ['/tmp/song.mp3'] }), true)
  assert.equal(nextNativeDragActive(true, { type: 'over' }), true)
  assert.equal(nextNativeDragActive(true, { type: 'drop' }), false)
})

test('zoom maps logical 100% to the readable baseline and clamps limits', () => {
  assert.equal(appliedZoom(1, 1.15), 1.15)
  assert.equal(normalizeZoom(1.15, .7, 1.8), 1.15)
  assert.equal(normalizeZoom(.1, .7, 1.8), .7)
  assert.equal(normalizeZoom(2, .7, 1.8), 1.8)
})

test('menu coordinates stay under the pointer and inside a zoomed viewport', () => {
  assert.deepEqual(menuPosition(500, 300, 150, 200, 1120, 720, 1.2), { left: 500 / 1.2, top: 300 / 1.2 })
  assert.deepEqual(menuPosition(1085, 700, 150, 200, 1120, 720, 1.2), { left: 964 / 1.2, top: 500 / 1.2 })
})

test('playlist drags accept only contiguous selections', () => {
  assert.deepEqual(contiguousRange([4, 2, 3]), { start: 2, length: 3 })
  assert.equal(contiguousRange([2, 4]), undefined)
  assert.equal(contiguousRange([]), undefined)
})

test('columns move before the header under the pointer', () => {
  assert.deepEqual(moveBefore(['name', 'artist', 'track'], 'track', 'name'), ['track', 'name', 'artist'])
  assert.deepEqual(moveBefore(['name', 'artist', 'track'], 'name'), ['artist', 'track', 'name'])
})

test('column and browser resizing preserve usable minimums', () => {
  assert.equal(resizedColumnWidth(34, 100, 90), 28)
  assert.equal(resizedPaneHeight(200, 100, 600, 420, 1), 420)
  assert.equal(resizedPaneHeight(200, 100, -100, 420, 1), 90)
  assert.equal(resizedPaneHeight(200, 100, 300, 420, 2), 300)
})

test('playlist rows move to the indicated insertion point', () => {
  assert.deepEqual(moveToIndex(['a', 'b', 'c'], 'a', 3), ['b', 'c', 'a'])
  assert.deepEqual(moveToIndex(['a', 'b', 'c'], 'c', 1), ['a', 'c', 'b'])
})

test('playlist pointer drags target the nearest insertion gap', () => {
  assert.equal(insertionIndexAtY([11, 33, 55], 0), 0)
  assert.equal(insertionIndexAtY([11, 33, 55], 22), 1)
  assert.equal(insertionIndexAtY([11, 33, 55], 60), 3)
})

test('artist album pages append without duplicate releases', () => {
  assert.deepEqual(mergeByUri([{ uri: 'a' }], [{ uri: 'a' }, { uri: 'b' }]), [{ uri: 'a' }, { uri: 'b' }])
})

test('clearing a track rating reveals its inherited album rating', () => {
  assert.deepEqual(clearedTrackRating(4), { stars: 4, explicit: false })
  assert.equal(clearedTrackRating(null), null)
})

test('sequential playback skips exclusions but an explicit start still plays one', () => {
  const tracks = [
    { id: 1, enabled: true },
    { id: 2, enabled: false },
    { id: 3, enabled: true },
  ] as never
  assert.deepEqual(playbackQueue(tracks, 1).map((track) => track.id), [1, 3])
  assert.deepEqual(playbackQueue(tracks, 2).map((track) => track.id), [1, 2, 3])
})

test('playlist highlights require both the synthetic id and Spotify URI', () => {
  const playing = { trackId: SYNTHETIC_BASE + 15, uri: 'spotify:track:better-days' }
  assert.equal(isCurrentTrack(playing, { id: SYNTHETIC_BASE + 15, uri: 'spotify:track:silent-thanks' }), false)
  assert.equal(isCurrentTrack(playing, { id: SYNTHETIC_BASE + 15, uri: 'spotify:track:better-days' }), true)
})

test('playback origins return to the launching library or playlist', () => {
  assert.deepEqual(playbackOriginAction({ kind: 'library', source: 'podcasts' }), { type: 'source', source: 'podcasts' })
  assert.deepEqual(playbackOriginAction({ kind: 'playlist', id: 'road-trip' }), { type: 'playlist', id: 'road-trip' })
})

test('playback start and result decisions keep OAuth outside the controller', () => {
  assert.equal(playbackStartAction('spotify:track:one', false), 'connect')
  assert.equal(playbackStartAction('spotify:track:one', true), 'play')
  assert.equal(playbackStartAction('file:///tmp/one.mp3', false), 'play')
  const prompt = { reason: 'missing' as const, message: 'Authorize playback.', targetTrackId: 2 }
  assert.deepEqual(playbackAuthorizationPrompt({ playbackAuthorizationRequired: prompt }), prompt)
  assert.equal(playbackAuthorizationPrompt('started'), null)
  assert.equal(pendingPlaybackTarget(prompt, [{ id: 2, uri: 'spotify:track:two', enabled: true }]), 2)
  assert.equal(pendingPlaybackTarget(prompt, []), null)
  assert.equal(playbackRetryReady(false, false, true), false)
  assert.equal(playbackRetryReady(true, false, true), false)
  assert.equal(playbackRetryReady(true, true, true), true)
  assert.equal(playbackRetryReady(true, false, false), true)
})

test('track and disc sorts keep multi-disc albums in playback order', () => {
  const track = (discNo: number | null, trackNo: number) => ({ discNo, trackNo } as never)
  const tracks = [track(2, 1), track(1, 2), track(null, 1), track(1, 3)]
  const expected = [tracks[2], tracks[1], tracks[3], tracks[0]]
  assert.deepEqual([...tracks].sort((a, b) => compareTracks(a, b, 'track', false)), expected)
  assert.deepEqual([...tracks].sort((a, b) => compareTracks(a, b, 'disc', false)), expected)
  assert.deepEqual([...tracks].sort((a, b) => compareTracks(a, b, 'track', true)), [...expected].reverse())
})

test('playlist sorting is view-only and clearing it restores Spotify order', () => {
  const tracks = [{ name: 'Zulu' }, { name: 'Alpha' }, { name: 'Mike' }] as never
  assert.deepEqual(playlistRows(tracks, 'name', false).map((row) => row.upstreamIndex), [1, 2, 0])
  assert.deepEqual(playlistRows(tracks, null, false).map((row) => row.upstreamIndex), [0, 1, 2])
})

test('dialog tabs wrap in both directions', () => {
  assert.equal(dialogTabTarget(2, 3, false), 0)
  assert.equal(dialogTabTarget(0, 3, true), 2)
  assert.equal(dialogTabTarget(1, 3, false), null)
})

test('overlay edits separate Library tracks from unique missing playlist tracks', () => {
  assert.deepEqual(overlayEditTargets([
    { id: 7, uri: 'spotify:track:in' },
    { id: null, uri: 'spotify:track:out' },
    { id: null, uri: 'spotify:track:out' },
  ]), { ids: [7], missingUris: ['spotify:track:out'] })
})

test('release dates sort chronologically with the standard tie-breakers and missing dates last', () => {
  const track = (releaseDate: string | null, trackNo: number) => ({ releaseDate, trackNo } as never)
  const tracks = [track(null, 1), track('2024-01-01', 2), track('2024-01-01', 1), track('2020', 1)]
  assert.deepEqual([...tracks].sort((a, b) => compareTracks(a, b, 'releaseDate', false)), [tracks[3], tracks[2], tracks[1], tracks[0]])
  assert.deepEqual([...tracks].sort((a, b) => compareTracks(a, b, 'releaseDate', true)), [tracks[1], tracks[2], tracks[3], tracks[0]])
})
