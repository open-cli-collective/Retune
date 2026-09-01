import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { createAppGateway } from '../src/appGateway.ts'
import { createLibraryGateway, libraryEvents } from '../src/libraryGateway.ts'
import { createLastFmGateway, lastfmEvents, type ImportPageOptions } from '../src/lastfmGateway.ts'
import { createPlaybackGateway, playbackEvents } from '../src/playbackGateway.ts'
import { createSpotifyGateway, spotifyEvents } from '../src/spotifyGateway.ts'
import { createMainEventSubscription, dispatchMainEvent, subscribeInvalidationThenSnapshot, subscribeThenSnapshot, subscriptionsThenSnapshot, type Invoker, type MainEvent } from '../src/ipc.ts'
import type { SettingsPatch } from '../src/types.ts'

test('subscribe-then-snapshot keeps an event that arrives during hydration', async () => {
  let emit!: (value: string) => void
  let resolveSnapshot!: (value: string) => void
  const installed: string[] = []
  const subscription = subscribeThenSnapshot(
    async (handler) => { emit = handler; return () => {} },
    () => new Promise<string>((resolve) => { resolveSnapshot = resolve }),
    (value) => installed.push(value),
    () => true,
  )
  await Promise.resolve()
  emit('new')
  resolveSnapshot('stale')
  await subscription
  assert.deepEqual(installed, ['new'])
})

test('subscribe-then-snapshot unregisters after an early unmount', async () => {
  let resolveSubscription!: (unlisten: () => void) => void
  let unlistened = false
  let active = true
  const subscription = subscribeThenSnapshot(
    () => new Promise((resolve) => { resolveSubscription = resolve }),
    async () => 'snapshot',
    () => assert.fail('an inactive subscription must not install a snapshot'),
    () => active,
  )
  active = false
  resolveSubscription(() => { unlistened = true })
  const unlisten = await subscription
  unlisten()
  assert.equal(unlistened, true)
})

test('invalidation snapshot ignores stale hydration after an event', async () => {
  let invalidate!: () => void
  const resolvers: Array<(value: string) => void> = []
  const installed: string[] = []
  const subscription = subscribeInvalidationThenSnapshot(
    async (handler) => { invalidate = handler; return () => {} },
    () => new Promise<string>((resolve) => resolvers.push(resolve)),
    (value) => installed.push(value),
    () => true,
  )
  await Promise.resolve()
  invalidate()
  resolvers[0]('stale')
  resolvers[1]('current')
  await subscription
  await Promise.resolve()
  assert.deepEqual(installed, ['current'])
})

test('main event dispatch is exhaustive over the tagged contract', () => {
  const received: string[] = []
  const handlers = {
    playerState: () => received.push('playerState'),
    playbackAuthorizationRequired: () => received.push('playbackAuthorizationRequired'),
    operationError: (payload: string) => received.push(payload),
    operationRecovered: () => received.push('operationRecovered'),
    localImportComplete: () => received.push('localImportComplete'),
    startupNotice: (payload: string) => received.push(payload),
  }
  dispatchMainEvent({ type: 'operationError', payload: 'error' }, handlers)
  dispatchMainEvent({ type: 'operationRecovered' }, handlers)
  dispatchMainEvent({ type: 'startupNotice', payload: 'notice' }, handlers)
  assert.deepEqual(received, ['error', 'operationRecovered', 'notice'])
})

test('main event registration serializes StrictMode cleanup before replacement', async () => {
  const calls: string[] = []
  let generation = 0
  let finishFirst!: (generation: number) => void
  let markFirstStarted!: () => void
  const firstStarted = new Promise<void>((resolve) => { markFirstStarted = resolve })
  const invoke: Invoker = async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push(command === 'unsubscribe_main_events' ? `${command}:${args?.generation}` : command)
    if (command !== 'subscribe_main_events') return undefined as T
    generation++
    if (generation === 1) {
      markFirstStarted()
      return new Promise<number>((resolve) => { finishFirst = resolve }) as Promise<T>
    }
    return generation as T
  }
  const subscribe = createMainEventSubscription(invoke, (onmessage) => ({ onmessage }))
  const stopFirst = subscribe((_event: MainEvent) => {}, assert.fail)
  await firstStarted
  stopFirst()
  subscribe((_event: MainEvent) => {}, assert.fail)
  assert.deepEqual(calls, ['subscribe_main_events'])
  finishFirst(1)
  await new Promise((resolve) => setTimeout(resolve, 0))

  assert.deepEqual(calls, [
    'subscribe_main_events',
    'unsubscribe_main_events:1',
    'subscribe_main_events',
  ])
})

test('main event channel can dispatch through the latest render handler without resubscribing', async () => {
  let channel!: { onmessage: (event: MainEvent) => void }
  let current = (_event: MainEvent) => assert.fail('stale handler ran')
  const subscribe = createMainEventSubscription(
    async <T>() => 1 as T,
    (onmessage) => (channel = { onmessage }),
  )
  subscribe((event) => current(event), assert.fail)
  await Promise.resolve()
  let received = ''
  current = (event) => { received = event.type }

  channel.onmessage({ type: 'startupNotice', payload: 'ready' })

  assert.equal(received, 'startupNotice')
})

test('all importer listeners are installed before hydration starts', async () => {
  let resolveState!: (stop: () => void) => void
  let resolveCompletion!: (stop: () => void) => void
  let resolveHydration!: () => void
  let hydrationStarted = false
  let completionSeen = false
  const onCompletion = () => { completionSeen = true }
  const installed = subscriptionsThenSnapshot(
    [
      new Promise((resolve) => { resolveState = resolve }),
      new Promise((resolve) => { resolveCompletion = resolve }),
    ],
    () => new Promise<void>((resolve) => {
      hydrationStarted = true
      resolveHydration = resolve
    }),
    () => true,
  )
  resolveState(() => {})
  await Promise.resolve()
  assert.equal(hydrationStarted, false)
  resolveCompletion(() => {})
  await Promise.resolve()
  await Promise.resolve()
  assert.equal(hydrationStarted, true)
  onCompletion()
  assert.equal(completionSeen, true)
  resolveHydration()
  await installed
})

test('Last.fm gateway preserves every command name and camel-case argument', async () => {
  const calls: Array<[string, Record<string, unknown> | undefined]> = []
  const invoke: Invoker = async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push([command, args])
    return undefined as T
  }
  const lastfm = createLastFmGateway(invoke)
  const batch = { batchId: 7, artist: 'Artist', album: 'Album' }
  const options: ImportPageOptions = { importContent: true, includeHistoricalPlayCounts: false, wholeAlbum: true, genre: 'Rock', rating: 4, selectedTrackIds: ['source-a'] }

  await lastfm.state()
  await lastfm.queue(100, 25)
  await lastfm.page(batch)
  await lastfm.review({ ...batch, action: 'skip-album' })
  await lastfm.review({ ...batch, action: 'exclude', ids: ['source-a', 'source-b'] })
  await lastfm.saveOptions(batch, options)
  await lastfm.apply(batch, ['source-a'], true, options)
  await lastfm.retryApply(7)
  await lastfm.countMode('spotify:track:one', 'overwrite')
  await lastfm.activateCollection(batch)
  await lastfm.collectionSearchAlbums(7, 'Artist', 'needle')
  await lastfm.collectionPreviewAlbum(7, 'Artist', 'spotify:album:one')
  await lastfm.collectionAddAlbum(7, 'Artist', 'spotify:album:one')
  await lastfm.collectionRemoveAlbum(7, 'Artist', 'spotify:album:one')
  await lastfm.changeTrack(7, 'source-a', 'track query')
  await lastfm.changeAlbum(7, 'source-a', 'album query')
  await lastfm.selectMatch(7, 'source-a', 'spotify:track:one')
  await lastfm.selectMatches(7, [{ id: 'source-a', uri: 'spotify:track:one' }])
  const defaults = { importContent: true, includeHistoricalPlayCounts: true, wholeAlbum: false }
  await lastfm.start(defaults)
  await lastfm.acceptAll()
  await lastfm.prepareAcceptAll()
  await lastfm.setSearchTerms(false)
  await lastfm.accountState()
  await lastfm.connectAccount()
  await lastfm.finishAccount()
  await lastfm.disconnectAccount()
  await lastfm.syncPlays()

  assert.deepEqual(calls, [
    ['lastfm_import_state', undefined],
    ['lastfm_import_queue', { cursor: 100, limit: 25 }],
    ['lastfm_import_page', batch],
    ['lastfm_import_review', { ...batch, action: 'skip-album' }],
    ['lastfm_import_review', { ...batch, ids: ['source-a', 'source-b'], action: 'exclude' }],
    ['lastfm_import_options', { ...batch, options }],
    ['lastfm_import_apply', { ...batch, selectedIds: ['source-a'], archiveBatch: true, options }],
    ['lastfm_import_retry_apply', { batchId: 7 }],
    ['lastfm_import_count_mode', { targetUri: 'spotify:track:one', mode: 'overwrite' }],
    ['lastfm_import_activate_collection', batch],
    ['lastfm_import_collection_search_albums', { batchId: 7, artist: 'Artist', query: 'needle' }],
    ['lastfm_import_collection_preview_album', { batchId: 7, artist: 'Artist', uri: 'spotify:album:one' }],
    ['lastfm_import_collection_add_album', { batchId: 7, artist: 'Artist', uri: 'spotify:album:one' }],
    ['lastfm_import_collection_remove_album', { batchId: 7, artist: 'Artist', uri: 'spotify:album:one' }],
    ['lastfm_import_change_track', { batchId: 7, id: 'source-a', query: 'track query' }],
    ['lastfm_import_change_album', { batchId: 7, id: 'source-a', query: 'album query' }],
    ['lastfm_import_select_match', { batchId: 7, id: 'source-a', uri: 'spotify:track:one' }],
    ['lastfm_import_select_matches', { batchId: 7, selections: [{ id: 'source-a', uri: 'spotify:track:one' }] }],
    ['start_lastfm_import', { defaults }],
    ['lastfm_import_accept_all', undefined],
    ['lastfm_import_prepare_accept_all', undefined],
    ['lastfm_import_search_terms', { show: false }],
    ['lastfm_state', undefined],
    ['connect_lastfm', undefined],
    ['finish_lastfm', undefined],
    ['disconnect_lastfm', undefined],
    ['sync_lastfm_plays', undefined],
  ])
})

test('app gateway preserves shell command names and argument shapes', async () => {
  const calls: Array<[string, Record<string, unknown> | undefined]> = []
  const invoke: Invoker = async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push([command, args])
    return undefined as T
  }
  const app = createAppGateway(invoke)
  const fixture = JSON.parse(readFileSync(new URL('./fixtures/ipc-contracts.json', import.meta.url), 'utf8')) as {
    settingsPatch: SettingsPatch
  }

  await app.settings()
  await app.updateSettings(fixture.settingsPatch)
  await app.appearance()
  await app.openLastFmImporter()
  await app.diagnostics()
  await app.emailDiagnostics('redacted report')

  assert.deepEqual(calls, [
    ['get_settings', undefined],
    ['update_settings', { patch: fixture.settingsPatch }],
    ['get_appearance', undefined],
    ['open_lastfm_importer', undefined],
    ['load_diagnostics', undefined],
    ['email_diagnostics', { body: 'redacted report' }],
  ])
})

test('Last.fm gateway owns the consumed event names', () => {
  assert.deepEqual(lastfmEvents, {
    changed: 'lastfm-import-changed',
    applyFinished: 'lastfm-import-apply-finished',
  })
})

test('library gateway preserves command names and argument shapes', async () => {
  const calls: Array<[string, Record<string, unknown> | undefined]> = []
  const invoke: Invoker = async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push([command, args])
    return undefined as T
  }
  const library = createLibraryGateway(invoke)

  await library.browse('music', { cat: ['Rock'] }, null)
  await library.browse('podcasts', { art: ['Host'], alb: ['Series'] }, 'needle')
  await library.metadataValues()
  await library.genreValues()
  await library.getTrack(41)
  await library.editTrack(41, { name: 'Song', ratingChange: { stars: null } })
  await library.editTracks([41, 42], { art: 'Artist', cat: 'Rock', ratingChange: { stars: 5 } })
  await library.clickTrackStar(41, 4)
  await library.setTrackEnabled(42, false)
  await library.setAlbumRating('audiobooks', 'Author', 'Book', null)

  assert.deepEqual(calls, [
    ['browse', { source: 'music', sel: { cat: ['Rock'], art: [], alb: [] } }],
    ['browse', { source: 'podcasts', sel: { cat: [], art: ['Host'], alb: ['Series'] }, query: 'needle' }],
    ['metadata_values', undefined],
    ['genre_values', undefined],
    ['get_track', { id: 41 }],
    ['edit_track', { id: 41, edit: { name: 'Song', ratingChange: { stars: null } } }],
    ['set_track_infos', { ids: [41, 42], edit: { art: 'Artist', cat: 'Rock', ratingChange: { stars: 5 } } }],
    ['click_track_star', { id: 41, stars: 4 }],
    ['set_track_enabled', { id: 42, enabled: false }],
    ['set_album_rating', { source: 'audiobooks', art: 'Author', alb: 'Book', stars: null }],
  ])
})

test('library gateway owns the consumed event names', () => {
  assert.deepEqual(libraryEvents, {
    changed: 'library-changed',
    localImportStarted: 'local-import-started',
    localImportFailed: 'local-import-failed',
    localDragChanged: 'local-drag-changed',
  })
})

test('playback gateway preserves every command name and argument shape', async () => {
  const calls: Array<[string, Record<string, unknown> | undefined]> = []
  const invoke: Invoker = async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push([command, args])
    return undefined as T
  }
  const playback = createPlaybackGateway(invoke)
  const snapshot = [{ id: 7, uri: 'file:///Music/one.flac', name: 'One', art: 'Artist', alb: 'Album', durationSecs: 180, enabled: true }]

  await playback.play(snapshot, 0)
  await playback.toggle()
  await playback.previous()
  await playback.next()
  await playback.setVolume(73)
  await playback.seek(42)
  await playback.setRepeat('one')
  await playback.setShuffle(true)

  assert.deepEqual(calls, [
    ['play_tracks', { resources: [{ id: 7, uri: 'file:///Music/one.flac' }], startIndex: 0 }],
    ['player_toggle', undefined],
    ['player_prev', undefined],
    ['player_next', undefined],
    ['player_set_volume', { volume: 73 }],
    ['player_seek', { seconds: 42 }],
    ['set_repeat', { mode: 'one' }],
    ['set_shuffle', { shuffle: true }],
  ])
})

test('playback gateway owns the consumed event names', () => {
  assert.deepEqual(playbackEvents, {
    action: 'player-action',
  })
})

test('Spotify gateway preserves every command name and camel-case argument', async () => {
  const calls: Array<[string, Record<string, unknown> | undefined]> = []
  const invoke: Invoker = async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push([command, args])
    return undefined as T
  }
  const spotify = createSpotifyGateway(invoke)
  const album = { uri: 'spotify:album:a', name: 'Album', artist: 'Artist' }

  await spotify.connectionState()
  await spotify.connect()
  await spotify.authorizePlayback()
  await spotify.sync()
  await spotify.syncStatus()
  await spotify.search('needle', 20)
  await spotify.albumPage(album.uri)
  await spotify.artistPage('artist-id')
  await spotify.artistAlbums('artist-id', 40)
  await spotify.followArtist('artist-id', true)
  await spotify.addAlbum(album)
  await spotify.removeAlbum(album.uri)
  await spotify.addTrack('spotify:track:a')
  await spotify.addTracks(['spotify:track:a', 'spotify:track:b'])
  await spotify.removeTrack('spotify:track:b')
  await spotify.artwork('spotify:track:a', 300)
  await spotify.resolveTrackDestination('spotify:track:a', 'artist')
  await spotify.playlists()
  await spotify.playlists(['spotify:track:a'])
  await spotify.createPlaylist('Road Trip')
  await spotify.unfollowPlaylist('playlist-id')
  await spotify.reorderPlaylists(['second', 'first'])
  await spotify.playlistTracks('playlist-id')
  await spotify.addToPlaylist('playlist-id', ['spotify:track:a', 'spotify:track:b'])
  await spotify.addAlbumToPlaylist('playlist-id', 'spotify:album:a', 'Album · Album')
  await spotify.reorderPlaylist('playlist-id', 2, 5, 3)
  await spotify.removeFromPlaylist('playlist-id', [1, 4])
  await spotify.openPlaylist('playlist-id', 'web')

  assert.deepEqual(calls, [
    ['connection_state', undefined],
    ['connect_spotify', undefined],
    ['authorize_spotify_playback', undefined],
    ['sync_from_spotify', undefined],
    ['spotify_sync_status', undefined],
    ['spotify_search', { query: 'needle', offset: 20 }],
    ['spotify_album_page', { uri: 'spotify:album:a' }],
    ['spotify_artist_page', { artistId: 'artist-id' }],
    ['spotify_artist_albums', { artistId: 'artist-id', offset: 40 }],
    ['spotify_follow_artist', { artistId: 'artist-id', follow: true }],
    ['add_spotify_album', album],
    ['remove_spotify_album', { uri: 'spotify:album:a' }],
    ['add_spotify_track', { uri: 'spotify:track:a' }],
    ['add_spotify_tracks', { uris: ['spotify:track:a', 'spotify:track:b'] }],
    ['remove_spotify_track', { uri: 'spotify:track:b' }],
    ['track_artwork', { uri: 'spotify:track:a', minWidth: 300 }],
    ['resolve_spotify_track_destination', { uri: 'spotify:track:a', destination: 'artist' }],
    ['playlists_list', undefined],
    ['playlists_list', { uris: ['spotify:track:a'] }],
    ['playlist_create', { name: 'Road Trip' }],
    ['playlist_unfollow', { id: 'playlist-id' }],
    ['reorder_playlists', { ids: ['second', 'first'] }],
    ['playlist_tracks', { id: 'playlist-id' }],
    ['playlist_add', { id: 'playlist-id', uris: ['spotify:track:a', 'spotify:track:b'] }],
    ['playlist_add_album', { id: 'playlist-id', albumUri: 'spotify:album:a', albumLabel: 'Album · Album' }],
    ['playlist_reorder', { id: 'playlist-id', rangeStart: 2, insertBefore: 5, rangeLength: 3 }],
    ['playlist_remove', { id: 'playlist-id', indices: [1, 4] }],
    ['open_spotify_playlist', { id: 'playlist-id', target: 'web' }],
  ])
})

test('Spotify gateway owns the consumed lifecycle event names', () => {
  assert.deepEqual(spotifyEvents, {
    connectionChanged: 'connection-changed',
    syncProgress: 'sync-progress',
    syncProgressCount: 'sync-progress-count',
    syncStatusChanged: 'spotify-sync-status-changed',
    playlistsChanged: 'playlists-changed',
  })
})
