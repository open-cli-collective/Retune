import { defaultSettings, initialState } from '../src/appState.ts'
import type { MainEvent } from '../src/ipc.ts'
import type { PlayerState, Track } from '../src/types.ts'

export const tracks: Track[] = Array.from({ length: 4000 }, (_, index) => ({
  id: index + 1, uri: `file:///fixture/${index}`, name: `Track ${index}`, art: `Artist ${index % 200}`,
  alb: `Album ${index % 400}`, cat: 'Soundtrack', durationSecs: 3600, enabled: true, rating: null,
  isLocal: true, discNo: 1, trackNo: index % 12 + 1, playCount: 0, kind: 'MP3', bitrateKbps: 320,
  lastPlayedAt: null, addedAt: null, releaseDate: null,
}))
// Duplicate URIs deliberately represent separate playlist entries.
export const playlistTracks = [...tracks, ...tracks.slice(0, 100)]
export const calls: { command: string; args: unknown }[] = []
export class Channel { constructor(public onmessage: (event: MainEvent) => void) {} }
let channel: Channel | undefined
const listeners = new Map<string, Set<(event: { payload: unknown }) => void>>()
export const listen = async (name: string, handler: (event: { payload: unknown }) => void) => {
  const handlers = listeners.get(name) ?? new Set(); handlers.add(handler); listeners.set(name, handlers)
  return () => { handlers.delete(handler) }
}
export const getCurrentWindow = () => ({ label: 'main', setTitle: async () => {}, onDragDropEvent: async () => () => {} })
export function emit(event: MainEvent) { channel?.onmessage(event) }
export function invalidate(name: string, payload: unknown = null) { listeners.get(name)?.forEach(handler => handler({ payload })) }
export const player = (elapsed: number, overrides: Partial<PlayerState> = {}): PlayerState => ({
  trackId: 1, uri: tracks[0].uri, elapsed, isPlaying: true, external: false,
  name: tracks[0].name, art: tracks[0].art, alb: tracks[0].alb, durationSecs: 3600, shuffle: false, ...overrides,
})
export async function invoke(command: string, args: Record<string, any> = {}) {
  calls.push({ command, args })
  if (command === 'subscribe_main_events') { channel = args.channel; return 1 }
  if (command === 'unsubscribe_main_events') { channel = undefined; return }
  if (command === 'get_settings') return defaultSettings
  if (command === 'connection_state') return initialState.connection
  if (command === 'spotify_sync_status') return initialState.spotifySyncStatus
  if (command === 'lastfm_state') return initialState.lastfm
  if (command === 'lastfm_import_state') return initialState.lastfmImport
  if (command === 'track_artwork') return null
  if (command === 'browse') {
    const query = String(args.query ?? '').toLowerCase()
    const selected = tracks.filter(track => [track.name, track.art, track.alb, track.cat].some(value => value.toLowerCase().includes(query)))
    return { facets: { cats: ['Soundtrack'], arts: [...new Set(tracks.map(track => track.art))], albs: [...new Set(tracks.map(track => track.alb))] }, tracks: selected,
      albumRating: null, albumRatingArtist: null, albumRatingAmbiguous: false,
      counts: { tracks: selected.length, totalSecs: selected.length * 3600, perSource: { music: tracks.length, podcasts: 0, audiobooks: 0 } } }
  }
  if (command === 'playlists_list') return [{ id: 'fixture', name: 'Performance playlist', owned: true, itemsAvailable: true, trackCount: playlistTracks.length }]
  if (command === 'playlist_tracks') return playlistTracks
  if (command === 'play_tracks') { queueMicrotask(() => emit({ type: 'playerState', payload: player(0) })); return 'started' }
  if (command === 'player_toggle') { emit({ type: 'playerState', payload: player(0, { isPlaying: false }) }); return }
  if (command === 'player_seek') { emit({ type: 'playerState', payload: player(args.seconds) }); return }
  return null
}
