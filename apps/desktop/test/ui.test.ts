import assert from 'node:assert/strict'
import test from 'node:test'
import { formatDiagnosticReport, reportWindow, type DiagnosticEntry } from '../src/diagnostics.ts'
import { initialState, reducer, type Action } from '../src/appState.ts'
import type { BrowseView, PlaybackTrack, Selection, Settings, SpotifyResults } from '../src/types.ts'
import { createSpotifySearchState, expandSpotifySearchGroup, failSpotifySearchGroup, moreSpotifySearchLabel, receiveSpotifySearchPage, replaceSpotifySearchResults, resetSpotifySearchQuery, retrySpotifySearchGroup, setSpotifySearchTab, spotifyMembership, spotifySearchGroupHeader, spotifySearchPendingPageKey } from '../src/spotifySearch.ts'
import { acceptImportAndNext, acceptImportChanges, applyCurrentImportPageResponse, defaultReviewState, excludedImportCount, excludeImportRow, ignoreImportAlbum, ignoreImportArtist, isCurrentImportPageResponse, nextRemainingImportQueue, remainingImportCount, resolveImportCount, restPendingImportCount, selectedImportCount, skipImportAlbum, sortImportQueue, toggleImportRow, validImportIntent, type ImportQueueItem, type ImportSourceRow } from '../src/lastfmImportState.ts'
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
  { artist: 'Beta', album: 'Album', playCount: 3, latest: 30, sourceIds: ['a'], remaining: true, albumEntities: 1, trackEntities: 0 },
  { artist: 'Alpha', album: 'Album', playCount: 2, latest: 40, sourceIds: ['b', 'c'], remaining: true, albumEntities: 0, trackEntities: 2 },
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

test('Last.fm deferred page selection keeps the newer response', async () => {
  let resolveA!: (page: string) => void
  let resolveB!: (page: string) => void
  const responseA = new Promise<string>((resolve) => { resolveA = resolve })
  const responseB = new Promise<string>((resolve) => { resolveB = resolve })
  let generation = 1
  const applied: string[] = []
  const requestA = applyCurrentImportPageResponse(1, () => generation, responseA, (page) => applied.push(page))
  generation = 2
  const requestB = applyCurrentImportPageResponse(2, () => generation, responseB, (page) => applied.push(page))
  resolveB('B')
  await requestB
  resolveA('A')
  await requestA
  assert.deepEqual(applied, ['B'])
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
