// @vitest-environment jsdom

import { act, createRef, Profiler, StrictMode, useState } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AlbumPageView, ArtistPageView, BrowseView, DecisionTrack, LastFmImportState, LastFmState, LibrarySources, PlayerState, Settings, SpotifyNavEntry, SpotifySyncStatus, Track, TrackInfo, TrackMergePreview } from '../src/types.ts'
import type { MainEvent } from '../src/ipc.ts'

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
import { RemovedTracksDialog, RemoveTrackDialog, TrackMergeDialog } from '../src/trackDecisionDialogs.tsx'

;(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true

let root: Root | undefined
let container: HTMLDivElement | undefined

const deferred = <T,>() => {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}

const sources = (overrides: Partial<LibrarySources> = {}): LibrarySources => ({ savedTrack: false, savedAlbums: [], wholeAlbum: false, membershipKnown: true, retained: false, localFile: false, ...overrides })
const info = (track: Track, overrides: Partial<TrackInfo> = {}): TrackInfo => ({ ...track, source: 'music', localPath: null, origCat: null, inheritedRating: null, genres: [], sources: sources(), mergedSources: [], latestMergeAt: null, ...overrides })

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

async function typeInput(input: HTMLInputElement, value: string) {
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value)
  input.setSelectionRange(value.length, value.length)
  await act(async () => input.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: value.at(-1) })))
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

  it('opens album artwork in the shared lightbox without refetching and restores focus when dismissed', async () => {
    const page: AlbumPageView = {
      uri: 'spotify:album:one', name: 'One', artist: 'Artist', artistId: 'artist', albumType: 'Album', year: '2026',
      imageUrl: 'https://example.test/art-640.jpg', totalDurationSecs: 180, savedAlbum: false, contentComplete: false,
      addedAt: null, albumRating: null,
      tracks: [{ uri: 'spotify:track:one', name: 'One', trackNo: 1, durationSecs: 180, enabled: true, trackId: null, savedIndividually: false, rating: null }],
    }
    invokeMock.mockImplementation(async (command) => command === 'spotify_album_page' ? page : null)
    const view = await render(<SpotifySearch query="" searching={false} results={null} navigation={{ kind: 'album', uri: page.uri }} playingUri={null}
      onAdd={vi.fn(async () => {})} onRemove={vi.fn(async () => {})} onAddTrack={vi.fn(async () => {})} onRemoveTrack={vi.fn(async () => {})}
      onPlay={vi.fn()} onPlaylist={vi.fn()} onClose={vi.fn()} onError={vi.fn()} />)
    await waitFor(() => expect(view.querySelector('button[aria-label="Enlarge artwork for One"]')).not.toBeNull())
    const artwork = view.querySelector<HTMLButtonElement>('button[aria-label="Enlarge artwork for One"]')!
    expect(artwork.querySelector('img')?.src).toBe(page.imageUrl)
    artwork.focus()
    const before = invokeMock.mock.calls.length
    for (const dismissal of ['escape', 'button', 'backdrop']) {
      await act(async () => artwork.click())
      const dialog = view.querySelector<HTMLElement>('.artwork-lightbox[role="dialog"]')!
      expect(dialog.querySelector('img')?.src).toBe(page.imageUrl)
      expect(dialog.querySelector('img')?.alt).toBe('One album artwork')
      expect(document.activeElement).toBe(dialog.querySelector('button[aria-label="Close artwork"]'))
      await act(async () => {
        if (dismissal === 'escape') key(dialog, 'Escape')
        else if (dismissal === 'button') dialog.querySelector<HTMLButtonElement>('button[aria-label="Close artwork"]')!.click()
        else dialog.parentElement!.dispatchEvent(new MouseEvent('mousedown', { bubbles: true }))
      })
      expect(view.querySelector('[role="dialog"]')).toBeNull()
      expect(document.activeElement).toBe(artwork)
    }
    expect(invokeMock.mock.calls).toHaveLength(before)
  })

  it('shares track-list actions with now playing and keeps the clicked track despite selection or playback changes', async () => {
    const tracks = [track(1, 'Playing'), track(2, 'Selected'), track(3, 'Also Selected'), { ...track(4, 'Local'), uri: 'file:///local.mp3', isLocal: true }]
    const browse: BrowseView = {
      facets: { cats: ['Rock'], arts: ['Artist'], albs: ['Album'] }, tracks,
      albumRating: null, albumRatingArtist: null, albumRatingAmbiguous: false,
      counts: { tracks: tracks.length, totalSecs: 720, perSource: { music: tracks.length, podcasts: 0, audiobooks: 0 } },
    }
    let channel: { onmessage: (event: MainEvent) => void }
    const playerState = (index: number, overrides: Partial<PlayerState> = {}): PlayerState => ({
      trackId: tracks[index].id, uri: tracks[index].uri, elapsed: 10, isPlaying: true, external: false,
      name: null, art: null, alb: null, durationSecs: null, shuffle: false, ...overrides,
    })
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'browse') return browse
      if (command === 'get_settings') return defaultSettings
      if (command === 'connection_state') return { connected: true, needs_reauth: false, playback_authorized: true }
      if (command === 'spotify_sync_status') return spotifyStatus()
      if (command === 'lastfm_state') return { available: false, connected: false, username: null, pending: false, reconnectRequired: false, problem: null }
      if (command === 'lastfm_import_state') return idleLastFmImport()
      if (command === 'playlists_list') return []
      if (command === 'metadata_values') return { cats: [], arts: [], albs: [] }
      if (command === 'subscribe_main_events') { channel = args?.channel as typeof channel; return 1 }
      if (command === 'play_tracks') {
        channel.onmessage({ type: 'playerState', payload: playerState(Number(args?.startIndex)) })
        return 'started'
      }
      if (command === 'get_track') return info(tracks[Number(args?.id) - 1])
      if (command === 'get_track_merge') { const ids = args?.ids as number[]; const selected = mergeFixture().tracks.filter((track) => ids.includes(track.id!)); return { tracks: selected, target: selected[0], revision: 'selected' } }
      if (command === 'resolve_spotify_track_destination') return new Promise(() => {})
      return null
    })
    const view = await render(<App />)
    await waitFor(() => expect(view.querySelector('[data-track-id="1"]')).not.toBeNull())
    const contextMenu = async (selector: string) => act(async () => {
      view.querySelector<HTMLElement>(selector)!.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 250, clientY: 35 }))
    })
    const menuItems = () => [...view.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
    await contextMenu('.lcd')
    expect(menuItems()).toHaveLength(0)
    await act(async () => view.querySelector('[data-track-id="1"]')!.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })))
    await waitFor(() => expect(view.querySelector('.lcd .marquee')?.textContent).toBe('Playing'))
    await act(async () => {
      view.querySelector('[data-track-id="2"]')!.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    })
    await act(async () => {
      view.querySelector('[data-track-id="3"]')!.dispatchEvent(new MouseEvent('click', { bubbles: true, metaKey: true }))
    })
    expect(view.querySelectorAll('.track-row.selected')).toHaveLength(2)
    await contextMenu('[data-track-id="2"]')
    const listActions = menuItems().map((item) => item.textContent)
    await act(async () => menuItems()[4].click())
    expect(invokeMock).toHaveBeenLastCalledWith('get_track_merge', { ids: [2, 3], targetUri: null })
    expect(view.querySelector('#track-merge-title')?.textContent).toBe('Merge 2 tracks')
    await act(async () => key(view.querySelector('[role="dialog"]')!, 'Escape'))
    await contextMenu('[data-track-id="2"]')
    await act(async () => menuItems()[0].click())
    expect(invokeMock).toHaveBeenCalledWith('playlists_list', { uris: [tracks[1].uri, tracks[2].uri] })
    await act(async () => key(view.querySelector('[role="dialog"]')!, 'Escape'))
    await contextMenu('.lcd-artwork')
    expect(menuItems().map((item) => item.textContent).slice(0, 4)).toEqual(listActions.slice(0, 4))
    expect(listActions).toEqual(['Add to Playlist…', 'View album in Spotify', 'View artist albums in Spotify', 'Get Info', 'Merge tracks…', 'Remove from Retune…'])
    expect(menuItems().map((item) => item.textContent).slice(4)).toEqual(['Remove from Retune…', 'Remove from Spotify…'])
    await act(async () => channel.onmessage({ type: 'playerState', payload: playerState(1) }))
    await act(async () => menuItems()[3].click())
    expect(invokeMock).toHaveBeenCalledWith('get_track', { id: 1 })
    await waitFor(() => expect([...view.querySelectorAll<HTMLLabelElement>('.get-info label')].find((label) => label.textContent === 'Name')?.querySelector('input')?.value).toBe('Playing'))
    expect(view.querySelector('#multiple-item-information-title')).toBeNull()
    await act(async () => key(view.querySelector('[role="dialog"]')!, 'Escape'))

    await act(async () => channel.onmessage({ type: 'playerState', payload: playerState(0) }))
    await contextMenu('.lcd-copy')
    await act(async () => menuItems()[0].click())
    expect(invokeMock).toHaveBeenCalledWith('playlists_list', { uris: [tracks[0].uri] })
    await act(async () => key(view.querySelector('[role="dialog"]')!, 'Escape'))
    for (const [index, destination] of [[1, 'album'], [2, 'artist']] as const) {
      await contextMenu('.lcd')
      await act(async () => menuItems()[index].click())
      expect(invokeMock).toHaveBeenCalledWith('resolve_spotify_track_destination', { uri: tracks[0].uri, destination })
    }
    await act(async () => channel.onmessage({ type: 'playerState', payload: playerState(3) }))
    await contextMenu('.lcd')
    expect(menuItems().map((item) => item.disabled)).toEqual([false, true, true, false, false])
    await act(async () => key(menuItems()[0], 'Escape'))
    await act(async () => channel.onmessage({ type: 'playerState', payload: playerState(0, { external: true, uri: 'spotify:track:external', name: 'External' }) }))
    await contextMenu('.lcd')
    expect(menuItems()[3].disabled).toBe(true)
    await act(async () => menuItems()[0].click())
    expect(invokeMock).toHaveBeenCalledWith('playlists_list', { uris: ['spotify:track:external'] })
  })

  it('clears whole-album mode when a release loses its album selection, including stale saved options', async () => {
    const fixtures = importerFixtures()
    let page = { ...fixtures.pages.get(1)!, options: { ...fixtures.pages.get(1)!.options, wholeAlbum: true } }
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'genre_values') return []
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    const wholeAlbum = () => view.querySelector<HTMLInputElement>('input[aria-label="Import whole album"]')!
    await waitFor(() => expect(wholeAlbum()?.checked).toBe(true))
    page = { ...page, rows: page.rows.map((row) => ({ ...row, matchResult: { ...row.matchResult, selectedUri: 'spotify:track:one', trackMatches: {}, candidates: row.matchResult.candidates.map((candidate) => ({ ...candidate, uri: 'spotify:track:one', trackUris: ['spotify:track:one'] })) } })) }
    await emitNativeEvent('lastfm-import-changed', fixtures.state)
    await waitFor(() => expect(wholeAlbum().checked).toBe(false))
    expect(wholeAlbum().disabled).toBe(true)
    expect(view.querySelector('.import-exclusion-note')).toBeNull()
    const before = invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_options').length
    await act(async () => key(view.querySelector('[data-import-nav="source"][data-import-row="0"]')!, ' '))
    expect(wholeAlbum().checked).toBe(false)
    expect(invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_options')).toHaveLength(before)
    await act(async () => [...view.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Accept Changes')!.click())
    expect(invokeMock.mock.calls.find(([command]) => command === 'lastfm_import_apply')?.[1]).toMatchObject({ options: { wholeAlbum: false } })
  })

  it('keeps review interactive while whole-album options save and orders pending writes before acceptance', async () => {
    const fixtures = importerFixtures()
    const page = { ...fixtures.pages.get(1)!, options: { ...fixtures.pages.get(1)!.options, wholeAlbum: true } }
    const saves = [deferred<void>(), deferred<void>(), deferred<void>()]
    let saveIndex = 0
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: fixtures.queue, total: 2, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'lastfm_import_options') return saves[saveIndex++].promise
      if (command === 'genre_values') return []
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelector<HTMLInputElement>('input[aria-label="Import whole album"]')?.checked).toBe(true))
    const checkbox = view.querySelector<HTMLInputElement>('input[aria-label="Import whole album"]')!
    const pageReads = () => invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_page').length
    const before = pageReads()
    for (const expected of [false, true, false]) {
      await act(async () => checkbox.click())
      expect(checkbox.checked).toBe(expected)
      expect(checkbox.disabled).toBe(false)
      expect([...view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')].every((button) => !button.disabled)).toBe(true)
      expect(view.querySelector('.import-workspace')?.getAttribute('aria-busy')).toBe('false')
    }
    expect(saveIndex).toBe(1)
    await act(async () => [...view.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Accept Changes')!.click())
    expect(invokeMock.mock.calls.some(([command]) => command === 'lastfm_import_apply')).toBe(false)
    await act(async () => saves[0].resolve())
    await act(async () => saves[1].resolve())
    expect(pageReads()).toBeGreaterThan(before)
    expect(checkbox.checked).toBe(false)
    expect(invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_options').map(([, args]) => (args!.options as { wholeAlbum: boolean }).wholeAlbum)).toEqual([false, true, false])
    await act(async () => saves[2].resolve())
    expect(invokeMock.mock.calls.find(([command]) => command === 'lastfm_import_apply')?.[1]).toMatchObject({ options: { wholeAlbum: false } })
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
      const checkbox = rows[1].querySelector<HTMLInputElement>('input[type="checkbox"]')!
      expect(key(checkbox, ' ').defaultPrevented).toBe(false)
      expect(key(checkbox, 'r').defaultPrevented).toBe(false)
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
      if (command === 'lastfm_import_queue') return { cursor: 0, items: queue.map((item) => item.page === 2 ? { ...item, status: 'skipped' } : item), total: queue.length, nextCursor: null }
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
    expect(queueRows[1].getAttribute('aria-label')).toContain('skipped')
    expect(queueRows[1].querySelector('.import-queue-copy small')?.textContent).toBe('Artist · 1 tracks · skipped')
    expect(view.querySelectorAll('.import-queue-select input[type="checkbox"]')).toHaveLength(2)
    expect(view.querySelector('.import-status-dot')).toBeNull()
    expect(invokeMock.mock.calls.some(([command, args]) => command === 'lastfm_import_page' && args?.batchId === 2)).toBe(true)

    const filter = view.querySelector<HTMLInputElement>('input[aria-label="Filter import queue"]')!
    expect(filter.placeholder).toBe('Filter')
    const reviewRow = view.querySelector('[data-import-nav="source"][data-import-row="1"]')
    await typeInput(filter, 'Release Two')
    expect(view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')).toHaveLength(2)
    expect(view.querySelector('[data-import-nav="source"][data-import-row="1"]')).toBe(reviewRow)
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 110)) })
    await waitFor(() => expect(view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')).toHaveLength(1))
    expect(view.querySelector<HTMLButtonElement>('[data-import-nav="queue"]')?.textContent).toContain('Release Two')
    await typeInput(filter, '')
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 110)) })
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

    await typeInput(filter, 'Release Two')
    await act(async () => key(filter, 'Enter'))
    await waitFor(() => expect(view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')).toHaveLength(1))
    const releaseOneLoads = () => invokeMock.mock.calls.filter(([command, args]) => command === 'lastfm_import_page' && args?.batchId === 1).length
    const beforeApply = releaseOneLoads()
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Accept & Next Batch')!.click())
    await waitFor(() => expect(invokeMock.mock.calls.some(([command, args]) => command === 'lastfm_import_apply' && args?.batchId === 2)).toBe(true))
    expect(releaseOneLoads()).toBe(beforeApply)
  })

  it('shows preparation progress until the initial queue and batch finish loading', async () => {
    const fixtures = importerFixtures()
    const state = deferred<unknown>()
    const page = deferred<unknown>()
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return state.promise
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page.promise
      if (command === 'genre_values') return []
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    expect(view.textContent).toContain('Loading Last.fm tracks and preparing batches…')
    expect(view.querySelector('.import-intent-checks')).toBeNull()
    await act(async () => state.resolve(fixtures.state))
    expect(view.querySelector('.import-spinner')).not.toBeNull()
    expect(view.textContent).not.toContain('Import your complete Last.fm history')
    await act(async () => page.resolve(fixtures.pages.get(1)))
    await waitFor(() => expect(view.querySelector('#import-review-title')?.textContent).toBe('Release One'))
    expect(view.querySelector('.import-spinner')).toBeNull()
    expect(invokeMock.mock.calls.some(([command]) => command === 'start_lastfm_import')).toBe(false)
  })

  it.each(['downloading', 'aggregating', 'disconnected', 'error'])('keeps setup controls out of the %s state', async (phase) => {
    const fixtures = importerFixtures()
    let fail = phase === 'error'
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') {
        if (fail) throw new Error('Could not read import state')
        return { ...fixtures.state, phase: phase === 'disconnected' ? null : phase === 'error' ? 'done' : phase, username: phase === 'disconnected' ? null : fixtures.state.username }
      }
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [], total: 0, nextCursor: null }
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    const title = phase === 'downloading' ? 'Importing Last.fm tracks…' : phase === 'aggregating' ? 'Preparing Last.fm review batches…' : phase === 'disconnected' ? 'Last.fm isn’t connected' : 'Couldn’t load Last.fm batches'
    await waitFor(() => expect(view.textContent).toContain(title))
    expect(view.querySelector('.import-intent-checks')).toBeNull()
    expect(view.textContent).not.toContain('Start import')
    if (phase === 'error') {
      fail = false
      await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Try again')!.click())
      await waitFor(() => expect(view.querySelector('.import-empty')?.textContent).toContain('Import complete'))
    }
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
    invokeMock.mockImplementation(async (command, args) => {
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

  it('shows track-picker duration and assessments in provider order', async () => {
    const fixtures = importerFixtures()
    const base = fixtures.collectionPage.rows[0]
    const candidates = [
      { name: 'Miracle - Original Mix', uri: 'spotify:track:z', relation: 'same-songs', trackDurations: [368] },
      { name: 'Miracle', uri: 'spotify:track:y', relation: 'best-match', trackDurations: [218] },
      { name: 'Miracle (Other release)', uri: 'spotify:track:a', relation: null, trackDurations: [] },
    ].map((candidate) => ({ ...base.matchResult.candidates[0], artist: 'Cascada', trackAlbums: ['Album'], ...candidate }))
    const page = { ...fixtures.collectionPage, rows: [{ ...base, source: { ...base.source, artist: 'Cascada', track: 'Miracle' }, matchResult: { ...base.matchResult, selectedUri: null, trackMatches: {}, candidates } }] }
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'genre_values') return []
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.textContent).toContain('Change Track…'))
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Change Track…')!.click())
    const results = [...view.querySelectorAll('.import-picker-track')]
    expect(results.map((row) => row.querySelector('strong')?.textContent)).toEqual(candidates.map((candidate) => candidate.name))
    expect(results.map((row) => row.querySelector('time')?.textContent)).toEqual(['6:08', '3:38', '—'])
    expect(results.map((row) => row.querySelector('em')?.textContent)).toEqual(['Likely', 'EXACT TRACK MATCH', 'Low'])
    expect(results[2].querySelector('time')?.getAttribute('aria-label')).toBe('Duration unavailable')
    if (process.env.RETUNE_PICKER_PREVIEW) {
      const { writeFileSync, readFileSync } = await import('node:fs')
      writeFileSync(process.env.RETUNE_PICKER_PREVIEW, '<!doctype html><html data-theme="light"><meta charset="utf-8"><style>' + ['src/index.css', 'src/App.css', 'src/lastfmImporter.css'].map((path) => readFileSync(path, 'utf8')).join('\n') + '</style><body>' + document.body.innerHTML + '</body></html>')
    }
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

  it('keeps genre typing local and includes the draft in later option writes and Apply', async () => {
    const fixtures = importerFixtures()
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return fixtures.pages.get(1)
      if (command === 'genre_values') return []
      if (command === 'lastfm_import_options' || command === 'lastfm_import_apply') return null
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelector<HTMLInputElement>('input[aria-label="Import genre"]')).not.toBeNull())
    const genre = view.querySelector<HTMLInputElement>('input[aria-label="Import genre"]')!
    const optionCalls = () => invokeMock.mock.calls.filter(([command]) => command === 'lastfm_import_options')

    await typeInput(genre, 'Dream Pop')
    expect(optionCalls()).toHaveLength(0)
    const rating = view.querySelector<HTMLSelectElement>('select[aria-label="Import rating"]')!
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(rating, '4')
    await act(async () => rating.dispatchEvent(new Event('change', { bubbles: true })))
    await waitFor(() => expect(optionCalls()).toHaveLength(1))
    expect(optionCalls()[0][1]?.options).toEqual(expect.objectContaining({ genre: 'Dream Pop', rating: 4 }))

    await typeInput(genre, 'Post Rock')
    genre.focus()
    const beforeEnter = optionCalls().length
    await act(async () => key(genre, 'Enter', { isComposing: true }))
    expect(optionCalls()).toHaveLength(beforeEnter)
    await act(async () => key(genre, 'Enter'))
    await waitFor(() => expect(optionCalls().length).toBe(beforeEnter + 1))
    expect(optionCalls().at(-1)?.[1]?.options).toEqual(expect.objectContaining({ genre: 'Post Rock' }))

    await typeInput(genre, 'Shoegaze')
    await act(async () => [...view.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Accept Changes')!.click())
    await waitFor(() => expect(invokeMock.mock.calls.some(([command]) => command === 'lastfm_import_apply')).toBe(true))
    expect(invokeMock.mock.calls.findLast(([command]) => command === 'lastfm_import_apply')?.[1]?.options).toEqual(expect.objectContaining({ genre: 'Shoegaze' }))
  })

  it.each([true, false])('shows live library metadata and preserves genre edits (collection=%s)', async (collection) => {
    const fixtures = importerFixtures()
    const base = fixtures.collectionPage
    const album = base.collection.cachedAlbums[0]
    const target = album.trackUris[0]
    let page = { ...base, collection: collection ? base.collection : null,
      rows: base.rows.map((item) => ({ ...item, matchResult: { ...item.matchResult, selectedUri: collection ? null : album.uri, candidates: [album] } })),
      suggestedGenre: 'Christmas',
      libraryMatches: {
        [target]: { inLibrary: true, albumInLibrary: true, genres: ['Christmas'], playCount: 42, rating: 4 },
        [album.uri]: { inLibrary: true, albumInLibrary: false, genres: ['Christmas'], playCount: 137, rating: 5 },
      },
    }
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state' || command === 'lastfm_import_apply') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'genre_values') return ['Christmas', 'Rock']
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    const genre = () => view.querySelector<HTMLInputElement>('input[aria-label="Import genre"]')!
    const accept = () => [...view.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Accept Changes')!
    await waitFor(() => expect(genre()?.value).toBe('Christmas'))
    const metadata = view.querySelector('.import-match-cell [aria-label="Track library metadata"]')!
    expect(metadata.textContent).toContain('Track in library')
    expect(metadata.textContent).toContain('Album in library')
    expect(metadata.textContent).toContain('Genre: Christmas')
    expect(metadata.textContent).toContain('42 plays in library')
    expect(metadata.querySelector('[aria-label="4 out of 5 stars"]')).not.toBeNull()
    const albumMetadata = view.querySelector('[aria-label="Album library metadata"]')!
    expect(albumMetadata.textContent).toContain('137 total track plays in library')
    expect(albumMetadata.querySelector('[aria-label="5 out of 5 stars"]')).not.toBeNull()
    expect(view.querySelector('.import-source-cell')?.textContent).not.toContain('42 plays in library')
    if (process.env.RETUNE_LIBRARY_PREVIEW && collection) {
      const { writeFileSync, readFileSync } = await import('node:fs')
      writeFileSync(process.env.RETUNE_LIBRARY_PREVIEW, '<!doctype html><html data-theme="light"><meta charset="utf-8"><style>' + ['src/index.css', 'src/App.css', 'src/lastfmImporter.css'].map((path) => readFileSync(path, 'utf8')).join('\n') + '</style><body>' + view.innerHTML + '</body></html>')
    }
    await act(async () => accept().click())
    await waitFor(() => expect(invokeMock.mock.calls.some(([command]) => command === 'lastfm_import_apply')).toBe(true))
    expect(invokeMock.mock.calls.findLast(([command]) => command === 'lastfm_import_apply')?.[1]?.options).toEqual(expect.objectContaining({ genre: 'Christmas', rating: null }))
    page = { ...page, suggestedGenre: 'Rock', libraryMatches: { ...page.libraryMatches, [target]: { ...page.libraryMatches[target], albumInLibrary: false, rating: null, playCount: 43, genres: ['Rock'] } } }
    await emitNativeEvent('lastfm-import-changed', null)
    await waitFor(() => expect(genre().value).toBe('Rock'))
    expect(view.querySelector('.import-match-cell [aria-label="Track library metadata"]')?.textContent).not.toContain('Album in library')
    expect(view.querySelector('.import-match-cell [aria-label="4 out of 5 stars"]')).toBeNull()
    await typeInput(genre(), 'Holiday')
    page = { ...page, suggestedGenre: 'Jazz' }
    await emitNativeEvent('lastfm-import-changed', null)
    expect(genre().value).toBe('Holiday')
    await act(async () => accept().click())
    expect(invokeMock.mock.calls.findLast(([command]) => command === 'lastfm_import_apply')?.[1]?.options).toEqual(expect.objectContaining({ genre: 'Holiday' }))
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(genre(), '')
    await act(async () => genre().dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'deleteContentBackward' })))
    page = { ...page, suggestedGenre: 'Country' }
    await emitNativeEvent('lastfm-import-changed', null)
    expect(genre().value).toBe('')
    await act(async () => accept().click())
    expect(invokeMock.mock.calls.findLast(([command]) => command === 'lastfm_import_apply')?.[1]?.options).toEqual(expect.objectContaining({ genre: null }))
  })

  it('collapses count merges into one entry and applies row actions to every contributor', async () => {
    const fixtures = importerFixtures()
    const base = fixtures.collectionPage.rows[0]
    const target = base.matchResult.candidates[0].trackUris[0]
    const rows = [111, 3, 7].map((playCount, index) => {
      const source = { ...base.source, stableId: `merge-${index}`, track: ['Live To Love', 'Live to Love', 'Live To Love (Original)'][index], playCount }
      return { ...base, source: { ...source, variants: [{ artist: source.artist, album: source.album, track: source.track, playCount, earliest: 10, latest: 30 }] }, matchResult: { ...base.matchResult, trackMatches: { [source.stableId]: target } } }
    })
    const other = { ...base, source: { ...base.source, stableId: 'other', track: 'Another song' }, matchResult: { ...base.matchResult, trackMatches: { other: target } } }
    let page = { ...fixtures.collectionPage, rows: [...rows, other], options: { ...fixtures.collectionPage.options, selectedTrackIds: [...rows, other].map(({ source }) => source.stableId) }, fuzzyGroups: { [target]: rows.map(({ source }) => source) }, resolvedCounts: { [target]: 121 } }
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'lastfm_import_select_matches') return page
      if (command === 'lastfm_import_options') {
        const options = args?.options as typeof page.options
        page = { ...page, options, fuzzyGroups: options.selectedTrackIds.length === 4 ? { [target]: rows.map(({ source }) => source) } : {} }
        return null
      }
      if (command === 'lastfm_import_review') return fixtures.state
      if (command === 'genre_values') return []
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    const button = (text: string) => [...view.querySelectorAll<HTMLButtonElement>('button')].find((entry) => entry.textContent === text)!
    await waitFor(() => expect(view.querySelector('.import-fuzzy-panel')).not.toBeNull())
    expect(view.querySelectorAll('.import-track-row')).toHaveLength(2)
    expect(view.querySelectorAll('.import-track-check input')).toHaveLength(2)
    expect(view.querySelector('.import-fuzzy-panel .import-track-copy')?.textContent).toContain('121 plays')
    const details = view.querySelector<HTMLElement>('.import-fuzzy-merge')!
    expect(details.hidden).toBe(false)
    expect(details.querySelector('output')?.textContent).toContain('121 plays')
    expect([...details.querySelectorAll('li > span:first-child')].map((entry) => entry.textContent)).toEqual(rows.map((row) => row.source.track))
    await act(async () => button('Hide flow').click())
    expect(details.hidden).toBe(true)
    expect(button('Show flow').getAttribute('aria-expanded')).toBe('false')
    expect(view.querySelectorAll('.import-track-row')).toHaveLength(2)
    await act(async () => button('Show flow').click())
    if (process.env.RETUNE_MERGE_PREVIEW) {
      const { writeFileSync, readFileSync } = await import('node:fs')
      writeFileSync(process.env.RETUNE_MERGE_PREVIEW, '<!doctype html><html data-theme="light"><meta charset="utf-8"><style>' + ['src/index.css', 'src/App.css', 'src/lastfmImporter.css'].map((path) => readFileSync(path, 'utf8')).join('\n') + '</style><body>' + view.innerHTML + '</body></html>')
    }
    const beforeUnmerge = invokeMock.mock.calls.length
    await act(async () => view.querySelector<HTMLButtonElement>('[aria-label="Unmerge Live to Love"]')!.click())
    expect(view.querySelectorAll('.import-track-row')).toHaveLength(3)
    expect(view.querySelector('.import-fuzzy-heading')?.textContent).toContain('2 Last.fm names')
    await act(async () => button('Merge matching rows').click())
    await act(async () => button('Unmerge all').click())
    expect(view.querySelectorAll('.import-track-row')).toHaveLength(4)
    expect(view.querySelector('.import-fuzzy-panel')).toBeNull()
    expect(invokeMock.mock.calls).toHaveLength(beforeUnmerge)
    await act(async () => button('Merge matching rows').click())
    const sourceCell = () => view.querySelector<HTMLElement>('.import-fuzzy-panel .import-source-cell')!
    sourceCell().focus()
    await act(async () => key(sourceCell(), 'ArrowDown'))
    expect(document.activeElement?.getAttribute('aria-label')).toBe('Last.fm source Another song')
    await act(async () => sourceCell().click())
    expect(button('Map selected (3)…').disabled).toBe(false)
    await act(async () => key(sourceCell(), 'e'))
    expect(view.querySelector('[role="dialog"]')?.textContent).toContain('3')
    await act(async () => button('Use This Track').click())
    await waitFor(() => expect(invokeMock.mock.calls.some(([command]) => command === 'lastfm_import_select_matches')).toBe(true))
    expect(invokeMock.mock.calls.findLast(([command]) => command === 'lastfm_import_select_matches')?.[1]).toEqual({ batchId: 1, selections: rows.map((row) => ({ id: row.source.stableId, uri: target })) })
    await act(async () => key(sourceCell(), ' '))
    await waitFor(() => expect(invokeMock.mock.calls.some(([command]) => command === 'lastfm_import_options')).toBe(true))
    expect(invokeMock.mock.calls.findLast(([command]) => command === 'lastfm_import_options')?.[1]?.options).toEqual(expect.objectContaining({ selectedTrackIds: ['other'] }))
    expect(view.querySelectorAll('.import-track-row')).toHaveLength(4)
    for (const row of rows) {
      await act(async () => view.querySelector<HTMLInputElement>(`input[aria-label="Include ${row.source.track}"]`)!.click())
    }
    await waitFor(() => expect(view.querySelectorAll('.import-track-row')).toHaveLength(2))
    await act(async () => key(sourceCell(), 'x'))
    await waitFor(() => expect(invokeMock.mock.calls.some(([command]) => command === 'lastfm_import_review')).toBe(true))
    expect(invokeMock.mock.calls.findLast(([command]) => command === 'lastfm_import_review')?.[1]?.ids).toEqual(rows.map((row) => row.source.stableId))
  })

  it('keeps completed, excluded, and unchecked sources outside an editable merged row', async () => {
    const fixtures = importerFixtures()
    const base = fixtures.collectionPage.rows[0]
    const target = base.matchResult.candidates[0].trackUris[0]
    const rows = ['pending-a', 'pending-b', 'done', 'excluded', 'unchecked'].map((id) => ({
      ...base,
      source: { ...base.source, stableId: id, track: id, variants: [{ artist: 'Artist', album: 'Album', track: id, playCount: 3, earliest: 10, latest: 30 }] },
      decision: { status: id === 'done' ? 'done' as const : 'pending' as const, excluded: id === 'excluded' },
      matchResult: { ...base.matchResult, trackMatches: { [id]: target } },
    }))
    const page = { ...fixtures.collectionPage, rows, options: { ...fixtures.collectionPage.options, selectedTrackIds: ['pending-a', 'pending-b'] }, fuzzyGroups: { [target]: rows.slice(0, 3).map(({ source }) => source) }, resolvedCounts: { [target]: 9 } }
    invokeMock.mockImplementation(async (command) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'get_appearance') return { theme: 'light' }
      if (command === 'genre_values') return []
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelectorAll('.import-track-row')).toHaveLength(4))
    expect(view.querySelectorAll('.import-fuzzy-panel')).toHaveLength(1)
    expect(view.querySelector('.import-fuzzy-heading')?.textContent).toContain('2 Last.fm names')
    expect(view.querySelector('[data-review-status="done"]')).not.toBeNull()
    expect(view.querySelector<HTMLInputElement>('input[aria-label="Include done"]')?.disabled).toBe(true)
    expect(view.querySelector<HTMLInputElement>('input[aria-label="Include excluded"]')?.disabled).toBe(true)
    expect(view.querySelector<HTMLInputElement>('input[aria-label="Include unchecked"]')?.checked).toBe(false)
  })

  it.each([true, false])('offers a chosen track as a suggestion without selecting the other row (collection=%s)', async (collection) => {
    const fixtures = importerFixtures()
    const base = fixtures.collectionPage.rows[0]
    const target = 'spotify:track:chosen'
    const candidate = { ...base.matchResult.candidates[0], uri: target, name: 'Song', artist: 'Artist', inLibrary: false, trackUris: [target], trackNames: ['Song'], trackArtists: ['Artist'], trackAlbums: ['Album'], relation: 'same-songs' }
    const rows = ['chosen', 'near'].map((id) => ({
      ...base,
      source: { ...base.source, stableId: id, artist: 'Artist', track: id === 'near' ? 'Song Live' : 'Song', variants: [] },
      matchResult: { ...base.matchResult, sourceId: id, selectedUri: id === 'chosen' ? target : null, confidence: id === 'chosen' ? 'exact' : null, trackMatches: id === 'chosen' ? { [id]: target } : {}, candidates: [{ ...candidate, relation: id === 'chosen' ? 'best-match' : 'same-songs' }] },
    }))
    let page = { ...fixtures.collectionPage, rows, options: { ...fixtures.collectionPage.options, selectedTrackIds: ['chosen', 'near'] }, collection: collection ? { ...fixtures.collectionPage.collection, cachedAlbums: [], selectedAlbumUris: [] } : null }
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: [fixtures.queue[0]], total: 1, nextCursor: null }
      if (command === 'lastfm_import_page') return page
      if (command === 'lastfm_import_select_match') {
        expect(args).toEqual({ batchId: 1, id: 'near', uri: target })
        page = { ...page, rows: page.rows.map((item) => item.source.stableId === 'near' ? { ...item, matchResult: { ...item.matchResult, selectedUri: target, trackMatches: { near: target } } } : item) }
        return page
      }
      if (command === 'genre_values') return []
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.textContent).toContain('SUGGESTED'))
    expect(view.querySelectorAll('.import-match-cell.needs-action')).toHaveLength(1)
    expect(invokeMock.mock.calls.some(([command]) => command === 'lastfm_import_select_match')).toBe(false)
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Use This Track')!.click())
    await waitFor(() => expect(view.textContent).not.toContain('SUGGESTED'))
    expect(view.querySelectorAll('.import-match-cell.needs-action')).toHaveLength(0)
  })

  it.each([false, true])('accepts from the middle and continues in the visible queue (filtered=%s)', async (filtered) => {
    const fixtures = importerFixtures()
    const batches = ['Release A', 'Release B', 'Unrelated', 'Release C'].map((album, index) => ({
      ...fixtures.queue[0], page: index + 1, album, remainingPlayCount: 4 - index,
    }))
    let currentQueue = batches
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: currentQueue, total: currentQueue.length, nextCursor: null }
      if (command === 'lastfm_import_page') {
        const batch = batches.find((item) => item.page === args?.batchId)!
        return { ...fixtures.pages.get(1)!, batchId: batch.page, album: batch.album }
      }
      if (command === 'lastfm_import_apply') {
        currentQueue = currentQueue.filter((item) => item.page !== args?.batchId)
        return fixtures.state
      }
      if (command === 'genre_values') return []
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelector('#import-review-title')?.textContent).toBe('Release A'))
    if (filtered) {
      const filter = view.querySelector<HTMLInputElement>('input[aria-label="Filter import queue"]')!
      await typeInput(filter, 'Release')
      await act(async () => key(filter, 'Enter'))
    }
    const openBatch = (title: string) => [...view.querySelectorAll<HTMLButtonElement>('[data-import-nav="queue"]')].find((button) => button.textContent?.includes(title))!
    await act(async () => openBatch('Release B').click())
    await waitFor(() => expect(view.querySelector('#import-review-title')?.textContent).toBe('Release B'))
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Accept & Next Batch')!.click())
    const expected = filtered ? 'Release C' : 'Unrelated'
    await waitFor(() => expect(view.querySelector('#import-review-title')?.textContent).toBe(expected))
    expect(openBatch(expected).getAttribute('aria-current')).toBe('true')
    expect(document.activeElement).toBe(openBatch(expected))
    expect(view.querySelector('.import-queue-list')?.textContent).not.toContain('Release B')
  })

  it.each([false, true])('keeps acceptance safe and unlocks navigation before the next page loads (failure=%s)', async (fail) => {
    const fixtures = importerFixtures()
    const acknowledgement = deferred<unknown>()
    const nextPage = deferred<unknown>()
    let applying = false
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'lastfm_import_state') return fixtures.state
      if (command === 'lastfm_import_queue') return { cursor: 0, items: fixtures.queue, total: 2, nextCursor: null }
      if (command === 'lastfm_import_page') return applying && args?.batchId === 2 ? nextPage.promise : fixtures.pages.get(Number(args?.batchId))
      if (command === 'lastfm_import_apply') { applying = true; await acknowledgement.promise; if (fail) throw new Error('Disk is full'); return fixtures.state }
      if (command === 'genre_values') return []
      if (command === 'get_appearance') return { theme: 'light' }
      return null
    })
    const view = await render(<LastFmImporter />)
    await waitFor(() => expect(view.querySelector('#import-review-title')?.textContent).toBe('Release One'))
    await act(async () => [...view.querySelectorAll('button')].find((button) => button.textContent === 'Accept & Next Batch')!.click())
    expect(view.textContent).toContain('Saving accepted batch…')
    expect(view.querySelector<HTMLButtonElement>('[data-import-nav="queue"]')!.disabled).toBe(true)
    if (fail) {
      await act(async () => acknowledgement.resolve(null))
      expect(view.querySelector('#import-review-title')?.textContent).toBe('Release One')
      expect(view.textContent).toContain('Disk is full')
    } else {
      await act(async () => acknowledgement.resolve(fixtures.state))
      expect(view.querySelector('#import-review-title')).toBeNull()
      expect(view.querySelector<HTMLButtonElement>('[data-import-nav="queue"]')!.disabled).toBe(false)
      await act(async () => nextPage.resolve(fixtures.pages.get(2)))
      await waitFor(() => expect(view.querySelector('#import-review-title')?.textContent).toBe('Release Two'))
    }
    expect(view.querySelector<HTMLButtonElement>('[data-import-nav="queue"]')!.disabled).toBe(false)
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
      await typeInput(input, 'z'.repeat(index))
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
      track={info({ ...track(1, 'Track'), art: '', alb: '', cat: '' })}
      onCancel={() => {}} onSaved={() => {}} onError={() => {}}
    /></Profiler>)
    const input = [...view.querySelectorAll('label')].find((label) => label.textContent === 'Artist')!.querySelector('input')!
    const elapsed: number[] = []
    const renders: number[] = []
    for (let index = 1; index <= 6; index++) {
      durations.length = 0
      const start = performance.now()
      await typeInput(input, 'z'.repeat(index))
      if (index > 1) { elapsed.push(performance.now() - start); renders.push(durations.reduce((a, b) => a + b, 0)) }
    }
    elapsed.sort((a, b) => a - b); renders.sort((a, b) => a - b)
    expect(invokeMock.mock.calls.filter(([command]) => command === 'metadata_values')).toHaveLength(1)
    console.log(`TYPEAHEAD GetInfo artists=20000 event-to-DOM-ms median=${elapsed[2].toFixed(2)} range=${elapsed[0].toFixed(2)}..${elapsed[4].toFixed(2)} render-median=${renders[2].toFixed(2)}`)
  })

})

describe('type-ahead behavior', () => {
  it('coalesces local queries behind one active browse, retains compatible rows, and skips Spotify typing', async () => {
    const initial: BrowseView = { facets: { cats: ['Classic Metal', 'Classic Rock'], arts: ['Artist'], albs: ['Album'] }, tracks: [track(1, 'Alpha One'), track(2, 'Alpha Two')], albumRating: null, albumRatingArtist: null, albumRatingAmbiguous: false, counts: { tracks: 2, totalSecs: 360, perSource: { music: 2, podcasts: 0, audiobooks: 0 } } }
    const held: ReturnType<typeof deferred<BrowseView>>[] = []
    let holdBrowse = false
    invokeMock.mockImplementation(async (command) => {
      if (command === 'browse') {
        if (!holdBrowse) return initial
        const request = deferred<BrowseView>()
        held.push(request)
        return request.promise
      }
      if (command === 'get_settings') return defaultSettings
      if (command === 'connection_state') return { connected: false, needs_reauth: false, playback_authorized: false }
      if (command === 'spotify_sync_status') return spotifyStatus()
      if (command === 'lastfm_state') return { available: false, connected: false, username: null, pending: false, reconnectRequired: false, problem: null }
      if (command === 'lastfm_import_state') return idleLastFmImport()
      if (command === 'playlists_list') return []
      if (command === 'subscribe_main_events') return 1
      return null
    })
    const view = await render(<StrictMode><App /></StrictMode>)
    await waitFor(() => expect(view.querySelectorAll('[data-track-id]')).toHaveLength(2))

    let row = view.querySelector<HTMLElement>('[data-track-id="1"]')!
    row.focus()
    for (const character of 'alpha tw') {
      await act(async () => { key(document.activeElement!, character); await new Promise(requestAnimationFrame) })
    }
    expect(view.querySelector('[data-track-id="2"]')?.classList.contains('selected')).toBe(true)

    holdBrowse = true
    const facet = view.querySelector<HTMLButtonElement>('[data-facet="cat"] [data-row-index="0"]')!
    facet.focus()
    expect(key(facet, ' ').defaultPrevented).toBe(false)
    for (const character of 'classic r') {
      await act(async () => { key(document.activeElement!, character); await new Promise(requestAnimationFrame) })
    }
    expect(view.querySelector('[data-facet="cat"] [data-row-index="2"]')?.classList.contains('active')).toBe(true)
    expect(held).toHaveLength(1)
    await act(async () => held[0].resolve(initial))
    await waitFor(() => expect(held).toHaveLength(2))
    await act(async () => held[1].resolve(initial))
    await waitFor(() => expect(view.textContent).not.toContain('Filtering library…'))

    const input = view.querySelector<HTMLInputElement>('input.search')!
    input.focus()
    input.setSelectionRange(0, input.value.length)
    expect(key(input, 'a', { metaKey: true }).defaultPrevented).toBe(false)
    expect(view.querySelectorAll('.track-row.selected')).toHaveLength(0)
    const browseCount = () => invokeMock.mock.calls.filter(([command]) => command === 'browse').length
    const before = browseCount()
    await typeInput(input, 'r')
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 130)) })
    expect(browseCount()).toBe(before + 1)
    expect(view.querySelector('[data-track-id="1"]')).not.toBeNull()
    expect(view.textContent).toContain('Filtering library…')

    await typeInput(input, 'ro')
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 130)) })
    await typeInput(input, 'roc')
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 130)) })
    expect(browseCount()).toBe(before + 1)

    const stale = { ...initial, tracks: [track(9, 'Stale')] }
    await act(async () => held[2].resolve(stale))
    await waitFor(() => expect(browseCount()).toBe(before + 2))
    expect(invokeMock.mock.calls.filter(([command]) => command === 'browse').at(-1)?.[1]?.query).toBe('roc')
    expect(view.querySelector('[data-track-id="9"]')).toBeNull()
    const latest = { ...initial, tracks: [track(3, 'Rock')] }
    await act(async () => held[3].resolve(latest))
    await waitFor(() => expect(view.querySelector('[data-track-id="3"]')).not.toBeNull())
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 1_010)) })
    const rockRow = view.querySelector<HTMLElement>('[data-track-id="3"]')!
    rockRow.focus()
    await act(async () => { key(rockRow, 'r'); await new Promise(requestAnimationFrame) })
    expect(rockRow.classList.contains('selected')).toBe(true)

    holdBrowse = false
    await act(async () => [...view.querySelectorAll<HTMLButtonElement>('.scope-pills button')].find((button) => button.textContent === 'Spotify')!.click())
    await waitFor(() => expect(view.textContent).not.toContain('Filtering library…'))
    const spotifyBefore = browseCount()
    for (const value of ['p', 'po', 'pop']) await typeInput(input, value)
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 140)) })
    expect(browseCount()).toBe(spotifyBefore)

    await typeInput(input, 'orphaned query')
    const podcasts = [...view.querySelectorAll<HTMLButtonElement>('.source-row')].find((button) => button.textContent?.includes('Podcasts'))!
    await act(async () => podcasts.click())
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 100)) })
    expect(input.value).toBe('')
    expect(invokeMock.mock.calls.filter(([command, args]) => command === 'browse' && args?.query === 'orphaned query')).toHaveLength(0)
  })

  it('ignores a late browse response after StrictMode unmount', async () => {
    const pending = deferred<BrowseView>()
    invokeMock.mockImplementation(async (command) => {
      if (command === 'browse') return pending.promise
      if (command === 'get_settings') return defaultSettings
      if (command === 'connection_state') return { connected: false, needs_reauth: false, playback_authorized: false }
      if (command === 'spotify_sync_status') return spotifyStatus()
      if (command === 'lastfm_state') return { available: false, connected: false, username: null, pending: false, reconnectRequired: false, problem: null }
      if (command === 'lastfm_import_state') return idleLastFmImport()
      if (command === 'playlists_list') return []
      if (command === 'subscribe_main_events') return 1
      return null
    })
    await render(<StrictMode><App /></StrictMode>)
    await waitFor(() => expect(invokeMock.mock.calls.some(([command]) => command === 'browse')).toBe(true))
    await act(async () => root?.unmount())
    await act(async () => pending.resolve({ facets: { cats: [], arts: [], albs: [] }, tracks: [], albumRating: null, albumRatingArtist: null, albumRatingAmbiguous: false, counts: { tracks: 0, totalSecs: 0, perSource: { music: 0, podcasts: 0, audiobooks: 0 } } }))
    expect(container?.textContent).toBe('')
  })
})

function mergeFixture(): TrackMergePreview {
  const tracks: DecisionTrack[] = [114, 0, 18].map((playCount, index) => ({
    ...track(index + 1, ['Chasing Fire', 'Chasing Fire (Single)', 'Chasing Fire (Remix)'][index]),
    art: 'Lauv', alb: index === 0 ? 'I met you when I was 18' : 'Chasing Fire', cat: index === 1 ? 'Pop' : 'Rock',
    rating: index === 1 ? 2 : 4, playCount, sources: sources({ savedTrack: true, savedAlbums: index === 0 ? ['I met you when I was 18'] : [], wholeAlbum: index === 0 }),
    addedAt: 1_600_000_000 + index, lastPlayedAt: 1_700_000_000 + index,
  }))
  return { tracks, target: tracks[0], revision: 'current' }
}

async function clickText(view: Element, text: string) {
  const button = [...view.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === text)
  expect(button, `button ${text}`).toBeDefined()
  await act(async () => button!.click())
}

async function selectValue(select: HTMLSelectElement, value: string) {
  await act(async () => { select.value = value; select.dispatchEvent(new Event('change', { bubbles: true })) })
}

describe('library track decisions', () => {
  it('requires explicit conflict choices, validates custom totals, and retries and undoes a three-track merge', async () => {
    const fixture = mergeFixture()
    let attempts = 0
    invokeMock.mockImplementation(async (command) => {
      if (command === 'get_track_merge') return fixture
      if (command === 'merge_library_tracks') { if (++attempts === 1) throw new Error('Disk unavailable'); return 1 }
      return null
    })
    const changed = vi.fn()
    const closed = vi.fn()
    const view = await render(<StrictMode><TrackMergeDialog ids={[1, 2, 3]} onClose={closed} onChanged={changed} /></StrictMode>)
    await waitFor(() => expect(view.querySelectorAll('.merge-recording')).toHaveLength(3))
    expect(view.querySelector<HTMLInputElement>('.merge-recording input')?.checked).toBe(true)
    if (process.env.RETUNE_DECISIONS_PREVIEW) {
      const { writeFileSync, readFileSync } = await import('node:fs')
      writeFileSync(`${process.env.RETUNE_DECISIONS_PREVIEW}-recording.html`, '<!doctype html><html data-theme="light"><meta charset="utf-8"><style>' + ['src/index.css', 'src/App.css', 'src/trackDecisions.css'].map((path) => readFileSync(path, 'utf8')).join('\n') + '</style><body>' + view.innerHTML + '</body></html>')
    }
    await clickText(view, 'Continue')
    const proceed = () => [...view.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Continue')!
    expect(proceed().disabled).toBe(true)
    await typeInput(view.querySelector<HTMLInputElement>('input[list="merge-genres"]')!, 'Rock')
    expect(proceed().disabled).toBe(true)
    await selectValue(view.querySelector('select')!, '4')
    expect(proceed().disabled).toBe(false)
    expect(view.querySelector('.merge-total')?.textContent).toContain('114 plays')
    await act(async () => view.querySelectorAll<HTMLInputElement>('input[name="play-count"]')[1].click())
    expect(view.querySelector('.merge-total')?.textContent).toContain('132 plays')
    await act(async () => view.querySelectorAll<HTMLInputElement>('input[name="play-count"]')[2].click())
    const number = view.querySelector<HTMLInputElement>('input[type="number"]')!
    for (const value of ['-1', '1.5', '4294967296', '25']) {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(number, value)
      await act(async () => number.dispatchEvent(new InputEvent('input', { bubbles: true })))
      expect(proceed().disabled).toBe(value !== '25')
    }
    await clickText(view, 'Continue')
    expect(view.querySelector('.merge-result')?.textContent).toContain('25 plays')
    if (process.env.RETUNE_DECISIONS_PREVIEW) {
      const { writeFileSync, readFileSync } = await import('node:fs')
      writeFileSync(`${process.env.RETUNE_DECISIONS_PREVIEW}-review.html`, '<!doctype html><html data-theme="light"><meta charset="utf-8"><style>' + ['src/index.css', 'src/App.css', 'src/trackDecisions.css'].map((path) => readFileSync(path, 'utf8')).join('\n') + '</style><body>' + view.innerHTML + '</body></html>')
    }
    await clickText(view, 'Merge tracks')
    expect(view.querySelector('[role="alert"]')?.textContent).toContain('Disk unavailable')
    expect(changed).not.toHaveBeenCalled()
    await clickText(view, 'Merge tracks')
    expect(invokeMock).toHaveBeenLastCalledWith('merge_library_tracks', { ids: [1, 2, 3], targetUri: fixture.target!.uri, edit: { name: fixture.target!.name, art: 'Lauv', alb: fixture.target!.alb, cat: 'Rock', rating: 4, playCount: { mode: 'custom', value: 25 } }, expectedRevision: 'current' })
    expect(changed).toHaveBeenCalledWith(1)
    await clickText(view, 'Undo merge')
    expect(invokeMock).toHaveBeenLastCalledWith('undo_track_merge', { id: 1 })
    expect(closed).toHaveBeenCalledOnce()
    expect(invokeMock.mock.calls.some(([command]) => /^(add|remove)_spotify/.test(command))).toBe(false)
  })

  it('includes an existing different recording in the preview and ignores obsolete search responses', async () => {
    const fixture = mergeFixture()
    fixture.tracks.forEach((track) => { track.cat = 'Rock'; track.rating = 4 })
    const external = { ...fixture.tracks[0], id: 99, uri: 'spotify:track:acoustic', name: 'Chasing Fire (Acoustic)', playCount: 9 }
    const stale = deferred<unknown>()
    invokeMock.mockImplementation(async (command, args) => {
      if (command === 'get_track_merge') return args?.targetUri ? { ...fixture, target: external, revision: 'external' } : fixture
      if (command === 'spotify_search') return args?.query === 'old' ? stale.promise : { tracks: { items: [{ ...external, artist: 'Lauv' }], total: 1, nextOffset: null } }
      return null
    })
    const view = await render(<TrackMergeDialog ids={[1, 2, 3]} onClose={vi.fn()} onChanged={vi.fn()} />)
    const query = view.querySelector<HTMLInputElement>('input[aria-label="Search recordings"]')!
    await typeInput(query, 'old'); await clickText(view, 'Search')
    await typeInput(query, 'new'); await clickText(view, 'Search')
    await act(async () => stale.resolve({ tracks: { items: [], nextOffset: null } }))
    expect(view.querySelector('.merge-search-results')?.textContent).toContain('Acoustic')
    await act(async () => view.querySelector<HTMLButtonElement>('.merge-search-results button')!.click())
    expect(invokeMock).toHaveBeenLastCalledWith('get_track_merge', { ids: [1, 2, 3], targetUri: external.uri })
    await clickText(view, 'Continue')
    await act(async () => view.querySelectorAll<HTMLInputElement>('input[name="play-count"]')[1].click())
    expect(view.querySelector('.merge-total')?.textContent).toContain('141 plays')
    await clickText(view, 'Continue')
    expect(view.textContent).toContain('Its existing history is included above.')
  })

  it.each([
    ['track', sources({ savedTrack: true }), true, 'move its Retune entry'],
    ['both', sources({ savedTrack: true, savedAlbums: ['Album'] }), true, 'excluded from sequential playback'],
    ['album', sources({ savedAlbums: ['Album'] }), false, 'not individually saved'],
    ['unknown', sources({ savedTrack: null, membershipKnown: false }), false, 'Sync Spotify'],
  ] as const)('handles %s membership before offering a Spotify removal', async (_, membership, enabled, explanation) => {
    invokeMock.mockImplementation(async (command) => command === 'get_track' ? info(track(1, 'Song'), { sources: membership }) : null)
    const changed = vi.fn()
    const view = await render(<RemoveTrackDialog tracks={[track(1, 'Song')]} spotify onClose={vi.fn()} onChanged={changed} />)
    expect(view.textContent).toContain(explanation)
    const remove = [...view.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Remove from Spotify')!
    expect(remove.disabled).toBe(!enabled)
    expect(invokeMock.mock.calls.some(([command]) => command === 'remove_spotify_track')).toBe(false)
    await act(async () => remove.click())
    if (enabled) { expect(invokeMock).toHaveBeenLastCalledWith('remove_spotify_track', { uri: 'spotify:track:1' }); expect(changed).toHaveBeenCalledOnce() }
    else { expect(changed).not.toHaveBeenCalled(); await clickText(view, 'Exclude from playback'); expect(invokeMock).toHaveBeenLastCalledWith('set_track_enabled', { id: 1, enabled: false }) }
  })

  it('removes locally with retry and restores through the dedicated list without Spotify writes', async () => {
    let attempts = 0
    invokeMock.mockImplementation(async (command) => {
      if (command === 'remove_retune_tracks' && ++attempts === 1) throw new Error('Write failed')
      if (command === 'removed_retune_tracks') return [mergeFixture().tracks[0]]
      return null
    })
    const changed = vi.fn()
    const view = await render(<RemoveTrackDialog tracks={[track(1, 'Song'), track(2, 'Other')]} spotify={false} onClose={vi.fn()} onChanged={changed} />)
    await clickText(view, 'Remove from Retune')
    expect(view.querySelector('[role="alert"]')?.textContent).toContain('Write failed')
    await clickText(view, 'Remove from Retune')
    expect(invokeMock).toHaveBeenLastCalledWith('remove_retune_tracks', { ids: [1, 2] })
    await act(async () => root!.render(<RemovedTracksDialog onClose={vi.fn()} onChanged={changed} />))
    expect(view.textContent).toContain('114 plays')
    await clickText(view, 'Add to Retune')
    expect(invokeMock).toHaveBeenLastCalledWith('restore_retune_track', { uri: 'spotify:track:1' })
    expect(view.textContent).toContain('No removed tracks')
    expect(invokeMock.mock.calls.some(([command]) => command.includes('spotify'))).toBe(false)
  })

  it('shows sources and merge lineage in Get Info while protecting unsaved fields from undo', async () => {
    invokeMock.mockImplementation(async (command) => command === 'metadata_values' ? { arts: [], albs: [], cats: [] } : null)
    const value = info({ ...track(1, 'Chosen'), playCount: 114 }, { sources: sources({ savedAlbums: ['Album'] }), mergedSources: mergeFixture().tracks, latestMergeAt: 1_700_000_000 })
    const view = await render(<GetInfo track={value} onCancel={vi.fn()} onSaved={vi.fn()} onError={vi.fn()} />)
    expect(view.textContent).toContain('Saved Spotify album · Album')
    expect(view.textContent).toContain('Play history · 114 plays')
    expect(view.textContent).toContain('Merged recordings (3)')
    const name = [...view.querySelectorAll('label')].find((label) => label.textContent === 'Name')!.querySelector('input')!
    await typeInput(name, 'Edited')
    const undo = [...view.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Undo last merge')!
    expect(undo.disabled).toBe(true)
    await act(async () => view.querySelector<HTMLInputElement>('.info-enabled input')!.click())
    expect(name.value).toBe('Edited')
    expect(invokeMock).toHaveBeenLastCalledWith('set_track_enabled', { id: 1, enabled: false })
    await typeInput(name, 'Chosen')
    expect(undo.disabled).toBe(false)
    await act(async () => undo.click())
    expect(invokeMock).toHaveBeenLastCalledWith('undo_track_merge', { id: 1 })
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
