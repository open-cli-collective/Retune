import { tauriInvoker, type Invoker } from './ipc.ts'
import type { AlbumPageView, ArtistAlbumsPage, ArtistPageView, ConnectionState, PlaylistListView, PlaylistTrack, SpotifyNavEntry, SpotifyResults, SpotifySyncStatus } from './types.ts'

export const spotifyEvents = {
  connectionChanged: 'connection-changed',
  syncProgress: 'sync-progress',
  syncProgressCount: 'sync-progress-count',
  syncStatusChanged: 'spotify-sync-status-changed',
  playlistsChanged: 'playlists-changed',
} as const

export function createSpotifyGateway(invoke: Invoker) {
  return {
    connectionState: () => invoke<ConnectionState>('connection_state'),
    connect: () => invoke<void>('connect_spotify'),
    authorizePlayback: () => invoke<void>('authorize_spotify_playback'),
    sync: () => invoke<void>('sync_from_spotify'),
    syncStatus: () => invoke<SpotifySyncStatus>('spotify_sync_status'),
    search: (query: string, offset: number) => invoke<SpotifyResults>('spotify_search', { query, offset }),
    albumPage: (uri: string) => invoke<AlbumPageView>('spotify_album_page', { uri }),
    artistPage: (artistId: string) => invoke<ArtistPageView>('spotify_artist_page', { artistId }),
    artistAlbums: (artistId: string, offset: number) => invoke<ArtistAlbumsPage>('spotify_artist_albums', { artistId, offset }),
    followArtist: (artistId: string, follow: boolean) => invoke<void>('spotify_follow_artist', { artistId, follow }),
    addAlbum: (album: { uri: string; name: string; artist: string }) => invoke<void>('add_spotify_album', album),
    removeAlbum: (uri: string) => invoke<void>('remove_spotify_album', { uri }),
    addTrack: (uri: string) => invoke<void>('add_spotify_track', { uri }),
    addTracks: (uris: string[]) => invoke<number[]>('add_spotify_tracks', { uris }),
    removeTrack: (uri: string) => invoke<void>('remove_spotify_track', { uri }),
    artwork: (uri: string, minWidth: number) => invoke<string | null>('track_artwork', { uri, minWidth }),
    resolveTrackDestination: (uri: string, destination: 'album' | 'artist') => invoke<SpotifyNavEntry>('resolve_spotify_track_destination', { uri, destination }),
    playlists: (uris?: string[]) => uris === undefined
      ? invoke<PlaylistListView[]>('playlists_list')
      : invoke<PlaylistListView[]>('playlists_list', { uris }),
    createPlaylist: (name: string) => invoke<PlaylistListView>('playlist_create', { name }),
    unfollowPlaylist: (id: string) => invoke<void>('playlist_unfollow', { id }),
    reorderPlaylists: (ids: string[]) => invoke<void>('reorder_playlists', { ids }),
    playlistTracks: (id: string) => invoke<PlaylistTrack[]>('playlist_tracks', { id }),
    addToPlaylist: (id: string, uris: string[]) => invoke<void>('playlist_add', { id, uris }),
    addAlbumToPlaylist: (id: string, albumUri: string, albumLabel: string) => invoke<void>('playlist_add_album', { id, albumUri, albumLabel }),
    reorderPlaylist: (id: string, rangeStart: number, insertBefore: number, rangeLength: number) => invoke<void>('playlist_reorder', { id, rangeStart, insertBefore, rangeLength }),
    removeFromPlaylist: (id: string, indices: number[]) => invoke<void>('playlist_remove', { id, indices }),
    openPlaylist: (id: string, target: 'app' | 'web') => invoke<void>('open_spotify_playlist', { id, target }),
  }
}

export const spotifyGateway = createSpotifyGateway(tauriInvoker)
