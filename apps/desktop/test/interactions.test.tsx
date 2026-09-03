// @vitest-environment jsdom

import { act, createRef, useState } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ArtistPageView, BrowseView, LastFmImportState, LastFmState, Settings, SpotifyNavEntry, Track } from '../src/types.ts'

const invokeMock = vi.hoisted(() => vi.fn<(command: string, args?: Record<string, unknown>) => Promise<unknown>>(async () => null))
const nativeEventHandlers = vi.hoisted(() => new Map<string, (payload: unknown) => void>())
vi.mock('@tauri-apps/api/core', () => ({
  invoke: invokeMock,
  Channel: class { onmessage: (event: unknown) => void; constructor(onmessage: (event: unknown) => void) { this.onmessage = onmessage } },
}))
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async (name: string, handler: (event: { payload: unknown }) => void) => {
    nativeEventHandlers.set(name, (payload) => handler({ payload }))
    return () => nativeEventHandlers.delete(name)
  }),
}))
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ onDragDropEvent: vi.fn(async () => () => {}), setTitle: vi.fn(async () => {}) }) }))

import App, { TransportBar } from '../src/App.tsx'
import { defaultSettings } from '../src/appState.ts'
import LastFmImporter from '../src/LastFmImporter.tsx'
import { TrackList } from '../src/libraryViews.tsx'
import { SpotifySearch } from '../src/spotifyViews.tsx'
import { labels, routeGlobalShortcut } from '../src/ui.ts'
import { ContextMenu } from '../src/viewShared.tsx'

;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true

let root: Root | undefined
let container: HTMLDivElement | undefined

const deferred = <T,>() => {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}

beforeEach(() => {
  nativeEventHandlers.clear()
  Object.defineProperty(window, 'matchMedia', { configurable: true, value: () => ({ matches: false, addEventListener: () => {}, removeEventListener: () => {} }) })
  Object.defineProperty(globalThis, 'ResizeObserver', { configurable: true, value: class { observe() {} unobserve() {} disconnect() {} } })
  HTMLElement.prototype.scrollIntoView = vi.fn()
})

async function emitNativeEvent(name: string, payload: unknown) {
  await act(async () => nativeEventHandlers.get(name)?.(payload))
}

async function render(element: React.ReactNode) {
  container = document.createElement('div')
  document.body.append(container)
  root = createRoot(container)
  await act(async () => { root?.render(element) })
  return container
}

function key(target: Element, value: string, options: KeyboardEventInit = {}) {
  const event = new KeyboardEvent('keydown', { key: value, bubbles: true, cancelable: true, ...options })
  target.dispatchEvent(event)
  return event
}

async function waitFor(assertion: () => void) {
  let error: unknown
  for (let attempt = 0; attempt < 20; attempt += 1) {
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)) })
    try { assertion(); return } catch (reason) { error = reason }
  }
  throw error
}

afterEach(async () => {
  await act(async () => root?.unmount())
  container?.remove()
  root = undefined
  container = undefined
  invokeMock.mockClear()
})

describe('mounted native interaction boundaries', () => {
  it('starts a facet with its first enabled visible track', async () => {
    const tracks = [
      { ...track(1, 'Excluded'), uri: 'fixture:track:excluded', enabled: false },
      { ...track(2, 'Included'), uri: 'fixture:track:included', enabled: true },
    ]
    let browse: BrowseView = {
      facets: { cats: ['Rock'], arts: ['Artist'], albs: ['Album'] },
      tracks,
      albumRating: null,
      albumRatingArtist: null,
      albumRatingAmbiguous: false,
      counts: { tracks: tracks.length, totalSecs: 360, perSource: { music: tracks.length, podcasts: 0, audiobooks: 0 } },
    }
    const settings: Settings = { ...defaultSettings, theme: 'light' }
    const lastfm: LastFmState = { available: false, connected: false, username: null, pending: false, reconnectRequired: false, problem: null }
    const lastfmImport: LastFmImportState = {
      phase: null, username: null, spotifyAccountId: null, historyTo: null, downloadedThrough: null, nextPage: 1,
      totalPages: null, downloadedPages: 0, totalScrobbles: 0, includedScrobbles: 0, processedScrobbles: 0,
      defaults: { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false }, remaining: 0,
      retryableError: null, searchTerms: true, syncing: false, lastSyncedAt: null, pendingReview: 0,
      syncProblem: null, applyingAll: false, spotifyLimit: null,
    }
    invokeMock.mockImplementation(async (command) => {
      if (command === 'browse') return browse
      if (command === 'get_settings') return settings
      if (command === 'connection_state') return { connected: false, needs_reauth: false, playback_authorized: false }
      if (command === 'lastfm_state') return lastfm
      if (command === 'lastfm_import_state') return lastfmImport
      if (command === 'playlists_list') return []
      if (command === 'subscribe_main_events') return 1
      return null
    })
    const view = await render(<App />)
    await waitFor(() => expect(view.querySelector('[data-facet="cat"] [data-row-index="1"]')).not.toBeNull())

    await act(async () => view.querySelector<HTMLButtonElement>('[data-facet="cat"] [data-row-index="1"]')!.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })))
    await waitFor(() => expect(view.querySelector('.lcd-copy .marquee')?.textContent).toBe('Included'))

    browse = {
      ...browse,
      tracks: browse.tracks.map((track, index) => ({ ...track, id: index + 3, name: `No Playback ${index + 1}`, enabled: false })),
    }
    await act(async () => view.querySelector<HTMLButtonElement>('[data-facet="art"] [data-row-index="1"]')!.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })))
    await waitFor(() => expect(view.querySelector('[data-track-id="3"]')?.textContent).toContain('No Playback 1'))
    expect(view.querySelector('.lcd-copy .marquee')?.textContent).toBe('Included')
  })

  it('keeps unavailable artist follow state retryable and rejects a late stale retry', async () => {
    const lateArtistA = deferred<ArtistPageView>()
    let artistACalls = 0
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'spotify_artist_albums') return { albums: [], nextOffset: null, total: 0 }
      if (command !== 'spotify_artist_page') return null
      if (args?.artistId === 'artist-a') {
        artistACalls += 1
        if (artistACalls === 1) throw new Error('follow state unavailable')
        return lateArtistA.promise
      }
      return { id: 'artist-b', name: 'Artist B', descriptor: 'Rock', imageUrl: null, following: true }
    })
    const navigationA: SpotifyNavEntry = { kind: 'artist', id: 'artist-a' }
    const navigationB: SpotifyNavEntry = { kind: 'artist', id: 'artist-b' }
    const props = {
      query: '', searching: false, results: null, playingUri: null,
      onAdd: vi.fn(async () => {}), onAddTrack: vi.fn(async () => {}), onRemoveTrack: vi.fn(async () => {}),
      onPlay: vi.fn(), onPlaylist: vi.fn(), onClose: vi.fn(), onError: vi.fn(),
    }
    const view = await render(<SpotifySearch {...props} navigation={navigationA} />)
    await waitFor(() => expect(view.textContent).toContain('Artist details are unavailable.'))
    expect([...view.querySelectorAll('button')].some((button) => button.textContent?.includes('Follow'))).toBe(false)

    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Retry')!.click())
    await act(async () => root?.render(<SpotifySearch {...props} navigation={navigationB} />))
    await waitFor(() => expect(view.textContent).toContain('Artist B'))
    expect(view.textContent).toContain('✓ Following')

    await act(async () => lateArtistA.resolve({ id: 'artist-a', name: 'Artist A', descriptor: 'Pop', imageUrl: null, following: false }))
    expect(view.textContent).toContain('Artist B')
    expect(view.textContent).not.toContain('+ Follow')
  })

  it('leaves unmodified button, link, and contenteditable keys to their native targets', async () => {
    const handled = vi.fn()
    const view = await render(<><button>Play</button><a href="#album">Album</a><div contentEditable>Rename</div></>)
    const listener = (event: KeyboardEvent) => { if (routeGlobalShortcut(event)) { event.preventDefault(); handled(event.key) } }
    document.addEventListener('keydown', listener)
    try {
      const buttonSpace = key(view.querySelector('button')!, ' ')
      const linkEnter = key(view.querySelector('a')!, 'Enter')
      const editableKey = key(view.querySelector('[contenteditable]')!, 'r')
      expect(handled).not.toHaveBeenCalled()
      expect([buttonSpace, linkEnter, editableKey].every((event) => !event.defaultPrevented)).toBe(true)

      key(view.querySelector('button')!, 'a', { ctrlKey: true })
      expect(handled).toHaveBeenCalledWith('a')
    } finally {
      document.removeEventListener('keydown', listener)
    }
  })

  it('seeks with native range semantics', async () => {
    const onSeek = vi.fn()
    const view = await render(<TransportBar
      playing={{ trackId: 1, uri: 'spotify:track:one', elapsed: 10, isPlaying: true, external: false, name: null, art: null, alb: null, durationSecs: null, shuffle: false, queue: [{ id: 1, uri: 'spotify:track:one', name: 'One', art: 'Artist', alb: 'Album', durationSecs: 120, enabled: true }] }}
      track={{ id: 1, uri: 'spotify:track:one', name: 'One', art: 'Artist', alb: 'Album', durationSecs: 120, enabled: true }}
      query=""
      scope="library"
      volume={50}
      searchRef={createRef<HTMLInputElement>()}
      onQuery={() => {}}
      onScope={() => {}}
      onPlay={() => {}}
      onPrev={() => {}}
      onNext={() => {}}
      onVolume={() => {}}
      onSeek={onSeek}
      onOrigin={() => {}}
      onArtwork={() => {}}
    />)
    const range = view.querySelector<HTMLInputElement>('input[aria-label="Playback position"]')!
    expect(range.type).toBe('range')
    expect(range.disabled).toBe(false)
    range.value = '42'
    await act(async () => range.dispatchEvent(new Event('input', { bubbles: true })))
    expect(onSeek).toHaveBeenCalledWith(42)
  })

  it('moves and activates library rows from the keyboard and recovers roving focus after refresh', async () => {
    const tracks = [track(1, 'One'), track(2, 'Two')]
    const onSelect = vi.fn()
    const onPlay = vi.fn()
    const props = {
      label: labels.music,
      selectedIds: new Set<number>(),
      playing: null,
      columnOrder: ['name'] as const,
      columnWidths: {},
      hiddenColumns: [],
      sortColumn: null,
      sortDesc: false,
      empty: false,
      onActivate: () => {}, onSetup: () => {}, onClearSelection: () => {}, onSelect, onPlay,
      onEnabled: () => {}, onRate: () => {}, onInfo: () => {}, onPlaylist: () => {},
      onGoToAlbum: () => {}, onGoToArtist: () => {}, onReorder: () => {}, onColumnWidths: () => {},
      onHiddenColumns: () => {}, onSort: () => {},
    }
    const view = await render(<TrackList {...props} columnOrder={[...props.columnOrder]} tracks={tracks} />)
    const rows = [...view.querySelectorAll<HTMLElement>('[data-track-id]')]
    const globalHandler = vi.fn()
    const listener = (event: KeyboardEvent) => { if (routeGlobalShortcut(event)) globalHandler(event.key) }
    document.addEventListener('keydown', listener)
    try {
      rows[0].focus()
      await act(async () => { key(rows[0], 'ArrowDown'); await new Promise(requestAnimationFrame) })
      expect(onSelect).toHaveBeenLastCalledWith(2, expect.anything())
      expect(document.activeElement).toBe(rows[1])
      key(rows[1], 'Enter')
      expect(onPlay).toHaveBeenCalledWith(2)
      key(rows[1], ' ')
      expect(onSelect).toHaveBeenLastCalledWith(2, expect.anything())
      expect(globalHandler).not.toHaveBeenCalled()
    } finally {
      document.removeEventListener('keydown', listener)
    }

    await act(async () => root?.render(<TrackList {...props} columnOrder={[...props.columnOrder]} tracks={[tracks[0]]} />))
    expect(view.querySelector<HTMLElement>('[data-track-id="1"]')?.tabIndex).toBe(0)
  })

  it('focuses menu items and restores the trigger on Escape', async () => {
    function Harness() {
      const [open, setOpen] = useState(false)
      return <><button onClick={() => setOpen(true)}>Actions</button>{open && <ContextMenu x={0} y={0} onClose={() => setOpen(false)}><button>Open album</button><button>Remove</button></ContextMenu>}</>
    }
    const view = await render(<Harness />)
    const trigger = view.querySelector('button')!
    trigger.focus()
    await act(async () => trigger.click())
    const menu = view.querySelector<HTMLElement>('[role="menu"]')!
    const item = menu.querySelector<HTMLElement>('[role="menuitem"]')!
    expect(document.activeElement).toBe(item)
    await act(async () => { key(item, 'Escape') })
    expect(view.querySelector('[role="menu"]')).toBeNull()
    expect(document.activeElement).toBe(trigger)
  })

  it('renders the virtual import queue as named buttons and moves Tab into the loaded mapping', async () => {
    const { state, queue, pages } = importerFixtures()
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: queue, total: queue.length, nextCursor: null }
      if (command === 'lastfm_import_page') return pages.get(Number(args?.batchId))
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')).toHaveLength(2))
    await waitFor(() => expect(view.textContent).toContain('Release One'))

    const queueRows = [...view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')]
    expect(queueRows[0].getAttribute('aria-label')).toContain('Batch 1 of 2')
    expect(queueRows[0].getAttribute('aria-current')).toBe('true')
    expect(invokeMock.mock.calls.some(([command, args]) => command === 'lastfm_import_page' && args?.batchId === 2)).toBe(true)

    queueRows[0].focus()
    const tab = key(queueRows[0], 'Tab')
    expect(tab.defaultPrevented).toBe(true)
    expect(document.activeElement).toBe(view.querySelector('[data-import-nav="source"][data-import-row="0"]'))

    await act(async () => queueRows[1].click())
    await waitFor(() => expect(view.textContent).toContain('Release Two'))
    expect(queueRows[1].getAttribute('aria-current')).toBe('true')
    expect(document.activeElement).toBe(queueRows[1])
  })

  it('refreshes only the selected batch for a valid apply success and cleans up native listeners', async () => {
    const { state, queue, pages } = importerFixtures()
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: queue, total: queue.length, nextCursor: null }
      if (command === 'lastfm_import_page') return pages.get(Number(args?.batchId))
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.textContent).toContain('Release One'))
    await waitFor(() => expect(nativeEventHandlers.has('lastfm-import-apply-finished')).toBe(true))
    const stateCalls = () => invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_state').length
    const before = stateCalls()

    await emitNativeEvent('lastfm-import-apply-finished', { status: 'succeeded', batchId: 2 })
    expect(stateCalls()).toBe(before)

    await emitNativeEvent('lastfm-import-apply-finished', { status: 'succeeded', batchId: 1 })
    await waitFor(() => expect(stateCalls()).toBeGreaterThan(before))
    expect(view.querySelector('[role="alert"]')).toBeNull()

    await act(async () => root?.unmount())
    root = undefined
    expect(nativeEventHandlers.has('lastfm-import-apply-finished')).toBe(false)
  })

  it('classifies every apply failure from code and never from display prose', async () => {
    const { state, queue, pages } = importerFixtures()
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: queue, total: queue.length, nextCursor: null }
      if (command === 'lastfm_import_page') return pages.get(Number(args?.batchId))
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.textContent).toContain('Release One'))
    const retryAt = Math.ceil(Date.now() / 1000) + 60

    await emitNativeEvent('lastfm-import-apply-finished', { status: 'failed', batchId: 1, code: 'spotify-rate-limited', message: 'Attendez avant de réessayer.', retryAt })
    await waitFor(() => expect(view.querySelector('[role="alert"]')?.textContent).toContain('Attendez avant de réessayer.'))
    expect(view.querySelector('.import-limit-reset')).not.toBeNull()

    await emitNativeEvent('lastfm-import-apply-finished', { status: 'failed', batchId: 1, code: 'spotify-quota-exhausted', message: 'La capacité est temporairement épuisée.', retryAt: null })
    await waitFor(() => expect(view.querySelector('[role="alert"]')?.textContent).toContain('La capacité est temporairement épuisée.'))
    expect(view.querySelector('.import-limit-reset')?.textContent).toContain('did not provide a reset time')

    await emitNativeEvent('lastfm-import-apply-finished', { status: 'failed', batchId: 1, code: 'apply-failed', message: 'Spotify rate limited until tomorrow.', retryAt })
    await waitFor(() => expect(view.querySelector('[role="alert"]')?.textContent).toContain('Spotify rate limited until tomorrow.'))
    expect(view.querySelector('.import-limit-reset')).toBeNull()
  })

  it('rejects malformed apply events, clears limit metadata, and refreshes the durable queue', async () => {
    const fixtures = importerFixtures()
    let queue = [{ ...fixtures.queue[0], status: 'failed' as const, error: 'Spotify rate limited legacy prose', errorCode: 'unknown-code', retryAt: Math.ceil(Date.now() / 1000) + 60 }]
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: queue, total: queue.length, nextCursor: null }
      if (command === 'lastfm_import_page') return fixtures.pages.get(Number(args?.batchId))
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.textContent).toContain('Spotify rate limited legacy prose'))
    expect(view.querySelector('.import-limit-reset')).toBeNull()
    const queueCalls = () => invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_queue').length
    const before = queueCalls()
    queue = [{ ...fixtures.queue[0], album: 'Durably Refreshed Release' }]

    await emitNativeEvent('lastfm-import-apply-finished', { status: 'failed', batchId: 1, code: 'spotify-rate-limited', message: 'missing deadline' })
    await waitFor(() => expect(queueCalls()).toBeGreaterThan(before))
    await waitFor(() => expect(view.textContent).toContain('Durably Refreshed Release'))
    expect(view.querySelector('[role="alert"]')?.textContent).toContain('Retune received an invalid Last.fm import result.')
    expect(view.querySelector('.import-limit-reset')).toBeNull()
  })

  it('executes release conversion, collection result actions, and track-picker native controls', async () => {
    const fixtures = importerFixtures()
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return fixtures.pages.get(Number(args?.batchId))
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      if (command === 'lastfm_import_activate_collection') return fixtures.collectionPage
      if (command === 'lastfm_import_collection_search_albums') return [fixtures.searchCandidate]
      if (command === 'lastfm_import_collection_preview_album') return fixtures.collectionPage
      if (command === 'lastfm_import_collection_add_album') return fixtures.collectionPage
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.textContent).toContain('Release One'))

    const changeTrack = [...view.querySelectorAll('button')].find((button) => button.textContent === 'Change Track…')!
    await act(async () => changeTrack.click())
    expect(view.querySelector<HTMLSelectElement>('label.import-picker-album-tracks select')?.options.length).toBeGreaterThan(1)
    expect(view.querySelector('label[for="import-picker-query"]')?.textContent).toBe('Search all Spotify')
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Cancel')!.click())

    const addAlbum = [...view.querySelectorAll('button')].find((button) => button.textContent === 'Add Album…')!
    await act(async () => addAlbum.click())
    await waitFor(() => expect(view.textContent).toContain('Manage Albums…'))
    expect(invokeMock).toHaveBeenCalledWith('lastfm_import_activate_collection', expect.objectContaining({ batchId: 1 }))
    expect(view.querySelector<HTMLInputElement>('input[aria-label="Import whole album"]')?.disabled).toBe(true)

    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Manage Albums…')!.click())
    const query = view.querySelector<HTMLInputElement>('#collection-album-query')!
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(query, 'alternate release')
    await act(async () => query.dispatchEvent(new Event('input', { bubbles: true })))
    await act(async () => view.querySelector<HTMLButtonElement>('.import-collection-dialog button[type="submit"]')!.click())
    await waitFor(() => expect(view.textContent).toContain('Alternate Release'))

    const result = [...view.querySelectorAll<HTMLElement>('article.import-picker-option')].find((row) => row.textContent?.includes('Alternate Release'))!
    const actions = [...result.querySelectorAll('button')]
    expect(actions.map((button) => button.textContent)).toEqual(['Preview', 'Add to album matches'])
    await act(async () => actions[1].click())
    expect(invokeMock).toHaveBeenCalledWith('lastfm_import_collection_add_album', expect.objectContaining({ uri: fixtures.searchCandidate.uri }))
  })
})

function track(id: number, name: string): Track {
  return {
    id, uri: `spotify:track:${id}`, name, art: 'Artist', alb: 'Album', cat: 'Rock', discNo: 1, trackNo: id,
    durationSecs: 180, enabled: true, playCount: 0, lastPlayedAt: null, addedAt: null, releaseDate: null,
    kind: null, bitrateKbps: null, overridden: false, isLocal: false, rating: null,
  }
}

function importerFixtures() {
  const state = {
    phase: 'review', username: 'listener', spotifyAccountId: 'spotify-user', historyTo: 100, downloadedThrough: 100,
    nextPage: 1, totalPages: 1, downloadedPages: 1, totalScrobbles: 5, includedScrobbles: 5, processedScrobbles: 5,
    defaults: { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false }, remaining: 5,
    retryableError: null, searchTerms: true, syncing: false, lastSyncedAt: null, pendingReview: 2, syncProblem: null,
    applyingAll: false, spotifyLimit: null,
  }
  const queue = [
    { page: 1, artist: 'Artist', album: 'Release One', playCount: 3, importedPlayCount: 0, remainingPlayCount: 3, latest: 30, sourceCount: 1, remaining: true, albumEntities: 1, trackEntities: 0 },
    { page: 2, artist: 'Artist', album: 'Release Two', playCount: 2, importedPlayCount: 0, remainingPlayCount: 2, latest: 20, sourceCount: 1, remaining: true, albumEntities: 1, trackEntities: 0 },
  ]
  const selectedCandidate = albumCandidate('spotify:album:selected', 'Selected Release')
  const searchCandidate = albumCandidate('spotify:album:alternate', 'Alternate Release')
  const page = (batchId: number, album: string) => ({
    state, batchId, artist: 'Artist', album, pageNumber: batchId, pageCount: 2,
    rows: [{
      source: { stableId: `source-${batchId}`, artist: 'Artist', album, track: `Track ${batchId}`, playCount: batchId === 1 ? 3 : 2, earliest: 10, latest: 30, variants: [] },
      decision: { status: 'pending', excluded: false },
      matchResult: { sourceId: `source-${batchId}`, searchTerm: 'query', confidence: 'exact', selectedUri: selectedCandidate.uri, candidates: [selectedCandidate], trackMatches: { [`source-${batchId}`]: selectedCandidate.trackUris[0] } },
    }],
    options: { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false, genre: null, rating: null, selectedTrackIds: [`source-${batchId}`] },
    fuzzyGroups: {}, countModes: {}, resolvedCounts: {}, lockedCountModes: [], collection: null,
  })
  const pages = new Map([[1, page(1, 'Release One')], [2, page(2, 'Release Two')]])
  const collection = {
    cachedAlbums: [selectedCandidate], selectedAlbumUris: [selectedCandidate.uri], wholeAlbumReady: false,
    coverage: { matched: 1, ambiguous: 0, unresolved: 0, selectedAlbums: [{ uri: selectedCandidate.uri, matched: 1, uniqueCoverage: 1 }], previews: [] },
  }
  const collectionPage = { ...pages.get(1)!, collection }
  return { state, queue, pages, collectionPage, searchCandidate }
}

function albumCandidate(uri: string, name: string) {
  return {
    uri, name, artist: 'Artist', inLibrary: false, relation: 'best-match', trackUris: [`${uri}:track:1`],
    trackNames: ['Track 1'], trackArtists: ['Artist'], trackAlbums: [name], imageUrl: null, releaseDate: '2020',
    albumType: 'album', totalTracks: 1, trackNumbers: [1], trackDurations: [180],
  }
}
