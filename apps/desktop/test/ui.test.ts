import assert from 'node:assert/strict'
import test from 'node:test'
import type { PlaybackTrack } from '../src/types.ts'
import { browseRequestKey, browseViewForRequest, clearedTrackRating, compareTracks, contiguousRange, dialogTabTarget, facetLabel, insertionIndexAtY, isCurrentTrack, menuPosition, mergeByUri, moveBefore, moveToIndex, nextNativeDragActive, normalizeZoom, overlayEditTargets, playbackOriginAction, playbackQueue, playlistRows, resizedColumnWidth, resizedPaneHeight, SYNTHETIC_BASE } from '../src/ui.ts'

test('pending narrower browse criteria cannot use the prior resolved tracks', () => {
  const broadQueue: PlaybackTrack[] = [
    { id: 1, uri: 'fixture:track:1', name: 'Welcome', art: 'Artist', alb: 'Broad Album', durationSecs: 180, enabled: true },
    { id: 2, uri: 'fixture:track:2', name: 'Americana', art: 'Artist', alb: 'Broad Album', durationSecs: 200, enabled: true },
  ]
  const baseSelection = {}
  const resolvedKey = browseRequestKey('music', baseSelection, '', 'library', 0)
  const pendingKey = browseRequestKey('music', { alb: ['America, The Dream Goes On'] }, '', 'library', 0)

  assert.deepEqual(playbackQueue(browseViewForRequest(broadQueue, resolvedKey, resolvedKey) ?? [], 1).map((track) => track.id), [1, 2])
  assert.deepEqual(browseViewForRequest(broadQueue, resolvedKey, pendingKey) ?? [], [])

  // Category, artist, and album selections are browser-pane selection changes.
  const changedKeys = [
    browseRequestKey('podcasts', baseSelection, '', 'library', 0),
    browseRequestKey('music', { cat: ['Rock'] }, '', 'library', 0),
    browseRequestKey('music', { art: ['Artist'] }, '', 'library', 0),
    pendingKey,
    browseRequestKey('music', baseSelection, 'America', 'library', 0),
    browseRequestKey('music', baseSelection, '', 'spotify', 0),
    browseRequestKey('music', baseSelection, '', 'library', 1),
  ]
  assert.equal(new Set([resolvedKey, ...changedKeys]).size, changedKeys.length + 1)
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

test('zoom preserves the Large preset and clamps limits', () => {
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
