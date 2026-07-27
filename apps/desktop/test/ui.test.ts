import assert from 'node:assert/strict'
import test from 'node:test'
import { clearedTrackRating, menuPosition, mergeByUri, moveBefore, moveToIndex, nextNativeDragActive, normalizeZoom, parseDragRange, resizedColumnWidth, resizedPaneHeight } from '../src/ui.ts'

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

test('playlist drag ranges reject malformed payloads', () => {
  assert.deepEqual(parseDragRange('{"start":2,"length":3}'), { start: 2, length: 3 })
  assert.equal(parseDragRange('{"start":-1,"length":3}'), undefined)
  assert.equal(parseDragRange('nope'), undefined)
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

test('artist album pages append without duplicate releases', () => {
  assert.deepEqual(mergeByUri([{ uri: 'a' }], [{ uri: 'a' }, { uri: 'b' }]), [{ uri: 'a' }, { uri: 'b' }])
})

test('clearing a track rating reveals its inherited album rating', () => {
  assert.deepEqual(clearedTrackRating(4), { stars: 4, explicit: false })
  assert.equal(clearedTrackRating(null), null)
})
