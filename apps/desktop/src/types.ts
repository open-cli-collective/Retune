export type Source = 'music' | 'podcasts' | 'audiobooks'
export type Theme = 'light' | 'dark' | 'system'
export type PlaybackBackend = 'connect' | 'local'
export type PlayThresholdPercent = 50 | 75 | 90 | 100
export type RepeatMode = 'off' | 'all' | 'one'
export type BrowserPanes = { cat: boolean; art: boolean; alb: boolean }
export type ColumnKey = 'name' | 'artist' | 'album' | 'disc' | 'track' | 'time' | 'rating' | 'genre' | 'plays' | 'kind' | 'bitrate' | 'lastPlayed' | 'added' | 'releaseDate'
export type Selection = { cat?: string[]; art?: string[]; alb?: string[] }
export type ActivePane = 'track' | keyof Selection

export type Settings = {
  theme: Theme
  zoom: number
  zebra: boolean
  plCollapsed: boolean
  browserVisible: boolean
  browserPanes: BrowserPanes
  columnOrder: ColumnKey[]
  columnWidths: Partial<Record<ColumnKey, number>>
  hiddenColumns: ColumnKey[]
  sortColumn: ColumnKey | null
  sortDesc: boolean
  autoAddSpotifyLibrary: boolean
  autoConnect: boolean
  spotifyClientId: string
  spotifySyncCompleted: boolean
  playbackBackend: PlaybackBackend
  repeat: RepeatMode
  shuffle: boolean
  volume: number
  streamingBitrate: number
  normalizeVolume: boolean
  gapless: boolean
  playThresholdPercent: PlayThresholdPercent
}

export type ConnectionState = { connected: boolean; needs_reauth: boolean; missing_scopes: string[] }
export type ImportSummary = { imported: number; duplicates: number; failed: { path: string; reason: string }[] }
export type PlaylistListView = {
  id: string
  name: string
  owned: boolean
  owner: string | null
  contains: boolean
  trackCount: number
  itemsAvailable: boolean
}
type RatingView = { stars: number; explicit: boolean }
export type SearchAlbum = {
  uri: string
  name: string
  artist: string
  year: string | null
  imageUrl: string | null
  albumType: string | null
  trackCount: number
  inLibrary: boolean
}
export type SpotifyResults = {
  artists: { id: string; name: string; descriptor: string; imageUrl: string | null }[]
  albums: SearchAlbum[]
  tracks: { uri: string; name: string; artist: string; alb: string; durationSecs: number; imageUrl: string | null; albumUri: string | null }[]
}
export type SpotifyNavEntry = { kind: 'artist'; id: string } | { kind: 'album'; uri: string; highlight?: string }

export type AlbumPageView = {
  uri: string
  name: string
  artist: string
  artistId: string
  albumType: string
  year: string | null
  imageUrl: string | null
  totalDurationSecs: number
  inLibrary: boolean
  albumRating: number | null
  tracks: {
    uri: string
    name: string
    trackNo: number | null
    durationSecs: number
    enabled: boolean
    trackId: number | null
    rating: RatingView | null
  }[]
}

export type ArtistPageView = {
  id: string
  name: string
  descriptor: string
  imageUrl: string | null
  following: boolean
}

export type ArtistAlbumsPage = { albums: SearchAlbum[]; nextOffset: number | null; total: number }

export type Track = {
  id: number
  uri: string
  name: string
  art: string
  alb: string
  cat: string
  discNo: number | null
  trackNo: number | null
  durationSecs: number
  enabled: boolean
  playCount: number
  lastPlayedAt: number | null
  addedAt: number | null
  releaseDate: string | null
  kind: string | null
  bitrateKbps: number | null
  overridden: boolean
  isLocal: boolean
  rating: RatingView | null
}

export type PlaybackTrack = Pick<Track, 'id' | 'uri' | 'name' | 'art' | 'alb' | 'durationSecs' | 'enabled'>
export type PlaylistTrack = Omit<PlaybackTrack, 'id'> & { id: number | null; rating: RatingView | null }
export type PlaylistSubject =
  | { kind: 'tracks'; label: string; uris: string[] }
  | { kind: 'album'; label: string; albumUri: string }

export type PlayerState = {
  trackId: number | null
  uri: string | null
  elapsed: number
  isPlaying: boolean
  external: boolean
  name: string | null
  art: string | null
  alb: string | null
  durationSecs: number | null
  volumeSupported: boolean
  shuffle: boolean
}

// `simulated` marks fixture tracks whose URIs must never reach a real backend.
export type Playing = PlayerState & { queue: readonly PlaybackTrack[]; simulated?: boolean }

export type BrowseView = {
  facets: { cats: string[]; arts: string[]; albs: string[] }
  tracks: Track[]
  albumRating: number | null
  albumRatingArtist: string | null
  albumRatingAmbiguous: boolean
  counts: {
    tracks: number
    totalSecs: number
    perSource: Record<Source, number>
  }
}

export type TrackInfo = {
  id: number
  uri: string
  localPath: string | null
  source: Source
  name: string
  art: string
  alb: string
  cat: string
  origCat: string | null
  rating: RatingView | null
  inheritedRating: number | null
  genres: string[]
}

export type MetadataValues = { arts: string[]; albs: string[]; cats: string[] }

export type InfoDialog =
  | { kind: 'single'; track: TrackInfo }
  | { kind: 'multiple'; tracks: Track[] }
