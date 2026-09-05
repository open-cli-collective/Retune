// @vitest-environment jsdom

import { act, createRef, Profiler, useState } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ArtistPageView, BrowseView, LastFmImportState, LastFmState, Settings, SpotifyNavEntry, SpotifySyncStatus, Track } from '../src/types.ts'

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

import App, { StatusBar, TransportBar } from '../src/App.tsx'
import { defaultSettings } from '../src/appState.ts'
import LastFmImporter from '../src/LastFmImporter.tsx'
import { GetInfo } from '../src/dialogViews.tsx'
import { filterImportQueue } from '../src/lastfmImportState.ts'
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

const idleLastFmImport = (): LastFmImportState => ({
  phase: null,
  username: null,
  spotifyAccountId: null,
  historyTo: null,
  downloadedThrough: null,
  nextPage: 1,
  totalPages: null,
  downloadedPages: 0,
  totalScrobbles: 0,
  includedScrobbles: 0,
  processedScrobbles: 0,
  defaults: { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false },
  remaining: 0,
  retryableError: null,
  searchTerms: false,
  syncing: false,
  lastSyncedAt: null,
  pendingReview: 0,
  syncProblem: null,
  applyingAll: false,
  spotifyLimit: null,
})

const spotifyStatus = (overrides: Partial<SpotifySyncStatus> = {}): SpotifySyncStatus => ({
  connected: true,
  running: false,
  lastFullSync: 1_700_000_000,
  nextSync: 1_700_003_600,
  cooldown: null,
  ...overrides,
})

describe('Spotify and Last.fm status bar', () => {
  it('composes clickable normal Spotify and Last.fm work segments', async () => {
    const onSpotifySync = vi.fn()
    const onLastfmImport = vi.fn()
    const lastfm = { ...idleLastFmImport(), syncing: true }
    const view = await render(<StatusBar
      view={null}
      unit="track"
      spotifySyncStatus={spotifyStatus()}
      lastfmImport={lastfm}
      lastfmRemaining={0}
      onSpotifySync={onSpotifySync}
      onLastfmImport={onLastfmImport}
      empty={false}
    />)

    expect(view.textContent).toContain('Spotify · last full sync')
    expect(view.textContent).toContain('· next')
    expect(view.textContent).toContain('Syncing Last.fm plays')
    expect(view.querySelector('.status-separator')?.textContent).toBe('·')

    await act(async () => view.querySelector<HTMLButtonElement>('.status-sync-link')?.click())
    await act(async () => view.querySelector<HTMLButtonElement>('.status-import-link')?.click())
    expect(onSpotifySync).toHaveBeenCalledTimes(1)
    expect(onLastfmImport).toHaveBeenCalledTimes(1)
  })

  it('renders cooldown as authoritative non-clickable Spotify status while preserving running progress', async () => {
    const onSpotifySync = vi.fn()
    const cooldownStatus = spotifyStatus({
      running: false,
      nextSync: 1_700_003_600,
      cooldown: { kind: 'quota', deadline: 1_700_000_120 },
    })
    const cooldownView = await render(<StatusBar
      view={null}
      unit="track"
      spotifySyncStatus={cooldownStatus}
      lastfmImport={idleLastFmImport()}
      lastfmRemaining={0}
      onSpotifySync={onSpotifySync}
      onLastfmImport={() => {}}
      empty={false}
    />)
    expect(cooldownView.textContent).toContain('Spotify paused until')
    expect(cooldownView.querySelector('.status-cooldown')?.tagName).toBe('SPAN')
    expect(cooldownView.querySelector('.status-sync-link')).toBeNull()

    await act(async () => root?.render(<StatusBar
      view={null}
      unit="track"
      syncPhase="Syncing saved albums…"
      syncProgress={{ tracks: 42, fraction: 0.5 }}
      spotifySyncStatus={spotifyStatus({ running: true, cooldown: cooldownStatus.cooldown })}
      lastfmImport={idleLastFmImport()}
      lastfmRemaining={0}
      onSpotifySync={onSpotifySync}
      onLastfmImport={() => {}}
      empty={false}
    />))
    expect(cooldownView.textContent).toContain('Syncing saved albums')
    expect(cooldownView.textContent).toContain('42 tracks synced')
    expect(cooldownView.querySelector('.status-sync-link')).toBeNull()
    expect(onSpotifySync).not.toHaveBeenCalled()
  })
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
      if (command === 'spotify_sync_status') return spotifyStatus()
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

    let queueRows = [...view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')]
    expect(queueRows[0].getAttribute('aria-label')).toContain('Batch 1 of 2')
    expect(queueRows[0].getAttribute('aria-current')).toBe('true')
    expect(invokeMock.mock.calls.some(([command, args]) => command === 'lastfm_import_page' && args?.batchId === 2)).toBe(true)

    const filter = view.querySelector<HTMLInputElement>('input[aria-label="Filter import queue"]')!
    expect(filter.placeholder).toBe('Filter')
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(filter, 'Release Two')
    await act(async () => filter.dispatchEvent(new Event('input', { bubbles: true })))
    await waitFor(() => expect(view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')).toHaveLength(1))
    expect(view.querySelector<HTMLButtonElement>('[data-import-nav="queue"]')?.textContent).toContain('Release Two')
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(filter, '')
    await act(async () => filter.dispatchEvent(new Event('input', { bubbles: true })))
    await waitFor(() => expect(view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')).toHaveLength(2))
    queueRows = [...view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')]

    queueRows[0].focus()
    const tab = key(queueRows[0], 'Tab')
    expect(tab.defaultPrevented).toBe(true)
    expect(document.activeElement).toBe(view.querySelector('[data-import-nav="source"][data-import-row="0"]'))

    await act(async () => queueRows[1].click())
    await waitFor(() => expect(view.textContent).toContain('Release Two'))
    expect(queueRows[1].getAttribute('aria-current')).toBe('true')
    expect(document.activeElement).toBe(queueRows[1])

    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(filter, 'Release Two')
    await act(async () => filter.dispatchEvent(new Event('input', { bubbles: true })))
    const releaseOneLoads = () => invokeMock.mock.calls.filter(([command, args]) => command === 'lastfm_import_page' && args?.batchId === 1).length
    const beforeApply = releaseOneLoads()
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Accept & Next Batch')!.click())
    await waitFor(() => expect(invokeMock.mock.calls.some(([command, args]) => command === 'lastfm_import_apply' && args?.batchId === 2)).toBe(true))
    expect(releaseOneLoads()).toBe(beforeApply)
  })

  it('retries queue pagination once when an apply changes the queue mid-load', async () => {
    const { state, queue, pages } = importerFixtures()
    const largeQueue = Array.from({ length: 1001 }, (_, index) => ({ ...queue[0], page: index + 1, album: `Release ${index + 1}` }))
    let queueCalls = 0
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return state
      if (command === 'lastfm_import_queue') {
        queueCalls += 1
        const cursor = Number(args?.cursor)
        if (queueCalls === 2) return { cursor, items: [], total: 1000, nextCursor: null }
        return { cursor, items: largeQueue.slice(cursor, cursor + 1000), total: largeQueue.length, nextCursor: cursor + 1000 < largeQueue.length ? cursor + 1000 : null }
      }
      if (command === 'lastfm_import_page') return pages.get(1)
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(queueCalls).toBe(4))
    expect(view.querySelector('[role="alert"]')).toBeNull()
  })

  it('combines the selected filtered queue results into one collection batch', async () => {
    const fixtures = importerFixtures()
    let currentQueue = fixtures.queue
    const combinedPage = {
      ...fixtures.pages.get(1)!,
      album: '',
      customBatch: true,
      albumLabelCount: 2,
      rows: [...fixtures.pages.get(1)!.rows, ...fixtures.pages.get(2)!.rows],
      collection: { cachedAlbums: [], selectedAlbumUris: [], fullAlbumUris: [], wholeAlbumReady: false, coverage: { matched: 0, ambiguous: 0, unresolved: 2, selectedAlbums: [], previews: [] } },
    }
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: currentQueue, total: currentQueue.length, nextCursor: null }
      if (command === 'lastfm_import_page') return fixtures.pages.get(Number(args?.batchId))
      if (command === 'lastfm_import_combine_batches') {
        currentQueue = [{ ...fixtures.queue[0], album: '', customBatch: true, sourceCount: 2 }]
        return combinedPage
      }
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelectorAll('[data-import-nav="queue"]')).toHaveLength(2))

    const selectAll = view.querySelector<HTMLInputElement>('input[aria-label="Select all filtered batches"]')!
    const firstBatch = view.querySelector<HTMLInputElement>('input[aria-label="Select Release One by Artist"]')!
    await act(async () => firstBatch.click())
    expect(selectAll.indeterminate).toBe(true)
    expect(selectAll.parentElement?.textContent).toContain('Select all 2 results')
    await act(async () => selectAll.click())
    const combine = [...view.querySelectorAll('button')].find((button) => button.textContent === 'Combine selected (2)')!
    expect(combine.disabled).toBe(false)
    await act(async () => combine.click())

    await waitFor(() => expect(view.querySelector('input[aria-label="Select Custom batch by Artist"]')).not.toBeNull())
    expect(invokeMock).toHaveBeenCalledWith('lastfm_import_combine_batches', { batchIds: [1, 2] })
    expect(view.textContent).toContain('You combined these Last.fm batches.')
    expect(view.textContent).toContain('Add albums…')
    expect([...view.querySelectorAll('button')].some((button) => button.textContent === 'Skip Batch')).toBe(true)
    expect([...view.querySelectorAll('button')].some((button) => button.textContent?.startsWith('Ignore '))).toBe(false)
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
      if (command === 'lastfm_import_collection_set_album_import') return { ...fixtures.collectionPage, collection: { ...fixtures.collectionPage.collection, fullAlbumUris: [fixtures.collectionPage.collection.selectedAlbumUris[0]] } }
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
    expect(view.querySelector<HTMLInputElement>('input[aria-label="Import whole album"]')).toBeNull()
    const importFullAlbum = view.querySelector<HTMLInputElement>('input[aria-label="Import full album: Selected Release"]')!
    const albumCard = importFullAlbum.closest('article')!
    expect(importFullAlbum.checked).toBe(false)
    expect(albumCard.textContent).toContain('MATCH SET')
    expect(albumCard.textContent).toContain('Matched tracks only')
    await act(async () => importFullAlbum.click())
    await waitFor(() => expect(importFullAlbum.checked).toBe(true))
    expect(albumCard.textContent).toContain('Full album')
    expect(invokeMock).toHaveBeenCalledWith('lastfm_import_collection_set_album_import', expect.objectContaining({ uri: 'spotify:album:selected', enabled: true }))

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

  it('maps selected Last.fm rows to one Spotify track', async () => {
    const fixtures = importerFixtures()
    const base = fixtures.collectionPage.rows[0]
    const trackUri = 'spotify:track:one-recording'
    const trackCandidate = { ...base.matchResult.candidates[0], uri: trackUri, name: 'One recording', relation: null, trackUris: [trackUri] }
    const rows = ['source-a', 'source-b', 'source-c'].map((stableId, index) => ({
      ...base,
      source: { ...base.source, stableId, track: `Source spelling ${index + 1}` },
      matchResult: index < 2 ? null : { ...base.matchResult, sourceId: stableId, selectedUri: trackUri, candidates: [trackCandidate], trackMatches: { [stableId]: trackUri } },
    }))
    const page = {
      ...fixtures.collectionPage,
      rows,
      collection: { ...fixtures.collectionPage.collection, cachedAlbums: [], selectedAlbumUris: [], coverage: { ...fixtures.collectionPage.collection.coverage, selectedAlbums: [] } },
      options: { ...fixtures.collectionPage.options, selectedTrackIds: rows.map((item) => item.source.stableId) },
    }
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'lastfm_import_select_matches') return page
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.textContent).toContain('Source spelling 3'))

    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Select all rows')!.click())
    const map = [...view.querySelectorAll('button')].find((button) => button.textContent === 'Map selected (3)…')!
    expect(map.disabled).toBe(false)
    await act(async () => map.click())
    expect(view.textContent).toContain('Choose one Spotify track for 3 Last.fm rows')

    const choice = view.querySelector<HTMLInputElement>('input[type="radio"]')!
    expect(choice.closest('label')?.textContent).toContain('One recording')
    await act(async () => choice.click())
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Use This Track')!.click())
    expect(invokeMock).toHaveBeenCalledWith('lastfm_import_select_matches', {
      batchId: 1,
      selections: rows.map((item) => ({ id: item.source.stableId, uri: trackUri })),
    })
  })
})

// Opt-in characterization for docs/lastfm-interaction-audit.md; no timing claims from jsdom.
describe.skipIf(!process.env.RETUNE_INTERACTION_AUDIT)('Last.fm interaction audit', () => {
  it('refreshes state, queue and page on a collection command event despite its page response', async () => {
    const fixtures = importerFixtures()
    const preview = deferred<unknown>()
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return fixtures.collectionPage
      if (command === 'lastfm_import_collection_preview_album') return preview.promise
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelector('.import-selected-album-card')).not.toBeNull())
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Manage Albums…')!.click())
    const before = invokeMock.mock.calls.length
    await act(async () => view.querySelector<HTMLButtonElement>('.import-collection-dialog .import-selected-album button')!.click())
    await emitNativeEvent('lastfm-import-changed', null)
    await waitFor(() => expect(invokeMock.mock.calls.slice(before).map(([command]) => command)).toContain('lastfm_import_page'))
    const calls = invokeMock.mock.calls.slice(before).map(([command]) => command)
    expect(calls).toContain('lastfm_import_state')
    expect(calls).toContain('lastfm_import_queue')
    await act(async () => preview.resolve(fixtures.collectionPage))
    console.log('AUDIT: collection preview + its invalidation event invoke', calls.join(', '))
  })

  it('holds unrelated controls through option save and queue refresh; keyboard bypasses busy', async () => {
    const fixtures = importerFixtures()
    const save = deferred<unknown>()
    const refreshedQueue = deferred<unknown>()
    let holdRefresh = false
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return holdRefresh ? refreshedQueue.promise : { cursor: 0, items: fixtures.queue, total: 2, nextCursor: null }
      if (command === 'lastfm_import_page') return fixtures.pages.get(Number(args?.batchId))
      if (command === 'lastfm_import_options') return save.promise
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelector('[data-import-nav="source"][data-import-row="1"]')).not.toBeNull())
    const include = view.querySelector<HTMLInputElement>('input[aria-label="Include Track 1"]')!
    expect(include).not.toBeNull()
    holdRefresh = true
    await act(async () => include.click())
    expect(include.checked).toBe(false)
    const queueButton = view.querySelector<HTMLButtonElement>('[data-import-nav="queue"]')!
    expect(queueButton.disabled).toBe(true)
    const before = invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_options').length
    await act(async () => key(view.querySelector('[data-import-nav="source"][data-import-row="1"]')!, ' '))
    expect(invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_options')).toHaveLength(before + 1)
    await act(async () => save.resolve(null))
    expect(queueButton.disabled).toBe(true)
    await act(async () => refreshedQueue.resolve({ cursor: 0, items: fixtures.queue, total: 2, nextCursor: null }))
    await waitFor(() => expect(queueButton.disabled).toBe(false))
    console.log('AUDIT: include updates locally; queue stays disabled through save AND refresh; Space submits another save while busy.')
  })

  it('updates exclusion before acknowledgement and keeps undo available', async () => {
    const fixtures = importerFixtures()
    const save = deferred<unknown>()
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: fixtures.queue, total: 2, nextCursor: null }
      if (command === 'lastfm_import_page') return fixtures.pages.get(Number(args?.batchId))
      if (command === 'lastfm_import_review') return save.promise
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelector('[data-import-nav="source"][data-import-row="1"]')).not.toBeNull())
    await act(async () => key(view.querySelector('[data-import-nav="source"][data-import-row="1"]')!, 'x'))
    const undo = view.querySelector<HTMLButtonElement>('button[aria-label="Undo exclusion"]')!
    expect(undo).not.toBeNull()
    expect(undo.disabled).toBe(false)
    expect(view.querySelector<HTMLButtonElement>('[data-import-nav="queue"]')!.disabled).toBe(true)
    await waitFor(() => expect(invokeMock.mock.calls.some(([command]) => command === 'lastfm_import_review')).toBe(true))
    await act(async () => save.resolve(fixtures.state))
    console.log('AUDIT: exclusion updates the DOM before IPC acknowledgement; undo remains enabled; queue navigation is still disabled.')
  })
})

describe.skipIf(!process.env.RETUNE_TYPEAHEAD_AUDIT)('type-ahead audit', () => {
  it('measures queue filtering without rendering or IPC', () => {
    const fixture = importerFixtures().queue[0]
    for (const size of [1000, 10000, 50000]) {
      const rows = Array.from({ length: size }, (_, index) => ({ ...fixture, page: index, artist: `Artist ${index}`, album: `Album ${index}` }))
      const samples: number[] = []
      for (let iteration = 0; iteration < 6; iteration++) {
        const start = performance.now()
        const result = filterImportQueue(rows, 'artist')
        const elapsed = performance.now() - start
        expect(result).toHaveLength(size)
        if (iteration > 0) samples.push(elapsed)
      }
      samples.sort((a, b) => a - b)
      console.log(`TYPEAHEAD filter rows=${size} median-ms=${samples[2].toFixed(2)} min=${samples[0].toFixed(2)} max=${samples[4].toFixed(2)}`)
    }
  })
  const type = async (input: HTMLInputElement, value: string) => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value)
    input.setSelectionRange(value.length, value.length)
    await act(async () => input.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: value.at(-1) })))
  }
  it.each([25, 250, 1000])('profiles importer genre typing with %i visible source rows', async (size) => {
    const fixtures = importerFixtures()
    const base = fixtures.pages.get(1)!
    const rows = Array.from({ length: size }, (_, index) => ({
      ...base.rows[0],
      source: { ...base.rows[0].source, stableId: `source-${index}`, track: `Track ${index}` },
      matchResult: null,
    }))
    const page = { ...base, rows, options: { ...base.options, selectedTrackIds: rows.map((row) => row.source.stableId) } }
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'genre_values') return Array.from({ length: 200 }, (_, index) => `Genre ${index}`)
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const durations: number[] = []
    const view = await render(<Profiler id="importer" onRender={(_, __, duration) => durations.push(duration)}><LastFmImporter /></Profiler>)
    await waitFor(() => expect(view.querySelectorAll('.import-track-row')).toHaveLength(size))
    const input = view.querySelector<HTMLInputElement>('input[aria-label="Import genre"]')!
    input.focus()
    const before = invokeMock.mock.calls.length
    const samples: number[] = []
    for (let index = 1; index <= 6; index++) {
      durations.length = 0
      await type(input, 'z'.repeat(index))
      if (index > 1) samples.push(durations.reduce((sum, duration) => sum + duration, 0))
    }
    expect(invokeMock.mock.calls.length).toBe(before)
    samples.sort((a, b) => a - b)
    console.log(`TYPEAHEAD importer rows=${size} React-render-ms median=${samples[2].toFixed(2)} min=${samples[0].toFixed(2)} max=${samples[4].toFixed(2)} IPC=0`)
  })
  it('profiles main Get Info autocomplete with 20000 artist suggestions', async () => {
    invokeMock.mockImplementation(async (command) => command === 'metadata_values'
      ? { arts: Array.from({ length: 20000 }, (_, index) => `Artist ${index}`), albs: [], cats: [] } : null)
    const durations: number[] = []
    const view = await render(<Profiler id="info" onRender={(_, __, duration) => durations.push(duration)}><GetInfo
      track={{ id: 1, uri: 'fixture:track:1', localPath: null, source: 'music', name: 'Track', art: '', alb: '', cat: '', origCat: null, rating: null, inheritedRating: null, genres: [] }}
      onCancel={() => {}} onSaved={() => {}} onError={() => {}}
    /></Profiler>)
    const input = [...view.querySelectorAll('label')].find((label) => label.textContent === 'Artist')!.querySelector('input')!
    const elapsed: number[] = []
    const renders: number[] = []
    for (let index = 1; index <= 6; index++) {
      durations.length = 0
      const start = performance.now()
      await type(input, 'z'.repeat(index))
      if (index > 1) { elapsed.push(performance.now() - start); renders.push(durations.reduce((a, b) => a + b, 0)) }
    }
    elapsed.sort((a, b) => a - b); renders.sort((a, b) => a - b)
    expect(invokeMock.mock.calls.filter(([command]) => command === 'metadata_values')).toHaveLength(1)
    console.log(`TYPEAHEAD GetInfo artists=20000 event-to-DOM-ms median=${elapsed[2].toFixed(2)} range=${elapsed[0].toFixed(2)}..${elapsed[4].toFixed(2)} render-median=${renders[2].toFixed(2)}`)
  })
  it('issues one browse per search keystroke even in Spotify scope and hides old rows', async () => {
    const browse: BrowseView = { facets: { cats: ['Rock'], arts: ['Artist'], albs: ['Album'] }, tracks: [track(1, 'Track')], albumRating: null, albumRatingArtist: null, albumRatingAmbiguous: false, counts: { tracks: 1, totalSecs: 180, perSource: { music: 1, podcasts: 0, audiobooks: 0 } } }
    let pending = deferred<BrowseView>()
    let held = false
    invokeMock.mockImplementation(async (command) => {
      if (command === 'browse') return held ? pending.promise : browse
      if (command === 'get_settings') return defaultSettings
      if (command === 'connection_state') return { connected: false, needs_reauth: false, playback_authorized: false }
      if (command === 'spotify_sync_status') return spotifyStatus()
      if (command === 'lastfm_state') return { available: false, connected: false, username: null, pending: false, reconnectRequired: false, problem: null }
      if (command === 'lastfm_import_state') return idleLastFmImport()
      if (command === 'playlists_list') return []
      if (command === 'subscribe_main_events') return 1
      return null
    })
    const view = await render(<App />)
    await waitFor(() => expect(view.querySelector('[data-track-id="1"]')).not.toBeNull())
    const row = view.querySelector<HTMLElement>('[data-track-id="1"]')!
    await act(async () => { row.focus(); key(row, 't') })
    expect(row.classList.contains('selected')).toBe(false)
    const facet = view.querySelector<HTMLElement>('[data-facet="art"]')!
    await act(async () => facet.dispatchEvent(new MouseEvent('mousedown', { bubbles: true })))
    held = true
    await act(async () => key(document.body, 'a'))
    expect(view.querySelector('[data-facet="art"] [data-row-index="1"]')).not.toBeNull()
    expect(view.querySelector('[data-track-id="1"]')).toBeNull()
    const duringFacet = invokeMock.mock.calls.length
    await act(async () => key(document.body, 'r'))
    expect(invokeMock.mock.calls.length).toBe(duringFacet)
    await act(async () => pending.resolve(browse))
    pending = deferred<BrowseView>()
    console.log('TYPEAHEAD focused track row ignores letters; old facets remain visible but next letter issues no refinement while browse is held.')
    const input = view.querySelector<HTMLInputElement>('input.search')!
    held = true
    const before = invokeMock.mock.calls.length
    for (const value of ['r', 'ro', 'roc']) await type(input, value)
    expect(view.querySelector('[data-track-id="1"]')).toBeNull()
    expect(invokeMock.mock.calls.slice(before).filter(([command]) => command === 'browse')).toHaveLength(3)
    await act(async () => pending.resolve(browse))
    held = false
    await act(async () => [...view.querySelectorAll<HTMLButtonElement>('.scope-pills button')].find((button) => button.textContent === 'Spotify')!.click())
    const spotifyBefore = invokeMock.mock.calls.length
    for (const value of ['p', 'po', 'pop']) await type(input, value)
    const calls = invokeMock.mock.calls.slice(spotifyBefore).filter(([command]) => command === 'browse')
    expect(calls).toHaveLength(3)
    expect(calls.every(([, args]) => args?.query === undefined)).toBe(true)
    console.log('TYPEAHEAD 3 library characters => 3 browse calls, prior rows hidden; 3 Spotify characters => 3 unfiltered library browse calls.')
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
    cachedAlbums: [selectedCandidate], selectedAlbumUris: [selectedCandidate.uri], fullAlbumUris: [], wholeAlbumReady: false,
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
