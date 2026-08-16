import type { BrowseView, ColumnKey, PlaybackAuthorizationPrompt, PlaybackOrigin, PlaybackTrack, PlayOutcome, PlaylistSubject, Selection, Settings, Source, Track } from './types.ts'

export type NativeDragEvent = { type: 'enter'; paths: string[] } | { type: 'over' } | { type: 'drop' } | { type: 'leave' }

export const LIBRARY_DEFAULT_COLUMN_ORDER: ColumnKey[] = ['track', 'name', 'artist', 'album', 'time', 'plays', 'rating', 'genre', 'disc', 'kind', 'bitrate', 'lastPlayed', 'added', 'releaseDate']
export const LIBRARY_DEFAULT_HIDDEN_COLUMNS: ColumnKey[] = ['disc', 'kind', 'bitrate', 'lastPlayed', 'added', 'releaseDate']
export const PLAYLIST_DEFAULT_COLUMN_ORDER: ColumnKey[] = ['name', 'artist', 'album', 'time', 'rating', 'plays', 'genre', 'disc', 'kind', 'bitrate', 'lastPlayed', 'added', 'releaseDate', 'track']
export const PLAYLIST_DEFAULT_HIDDEN_COLUMNS: ColumnKey[] = ['disc', 'kind', 'bitrate', 'lastPlayed', 'added', 'releaseDate', 'track']
export const PLAYLIST_COLUMNS: readonly ColumnKey[] = PLAYLIST_DEFAULT_COLUMN_ORDER

export const nextNativeDragActive = (active: boolean, event: NativeDragEvent) => {
  if (event.type === 'enter') return event.paths.length > 0
  return event.type === 'over' ? active : false
}

export const normalizeZoom = (zoom: number, min: number, max: number) =>
  Math.min(max, Math.max(min, Math.round(zoom * 100) / 100))

export const appliedZoom = (zoom: number, base: number) => zoom * base

export const browseRequestKey = (source: Source, selection: Selection, query: string, scope: 'library' | 'spotify') =>
  JSON.stringify([source, selection.cat ?? [], selection.art ?? [], selection.alb ?? [], query, scope])

export const browseViewForRequest = <T>(view: T | null, resolvedKey: string | undefined, requestKey: string) =>
  resolvedKey === requestKey ? view : null

export const selectionAfterFacet = (selection: Selection, facet: keyof Selection, values: string[]): Selection =>
  facet === 'cat' ? { cat: values }
    : facet === 'art' ? { cat: selection.cat, art: values }
      : { ...selection, alb: values }

export const rememberSelection = (selections: Record<Source, Selection>, source: Source, selection: Selection) => ({ ...selections, [source]: selection })
export const restoreSelection = (selections: Record<Source, Selection>, source: Source) => selections[source] ?? {}

export const visibleColumnOrder = (order: ColumnKey[], hidden: ColumnKey[]) => order.filter((column) => !hidden.includes(column))
export const playlistOverride = <T>(overrides: Record<string, T>, id: string, value: T, defaultValue: T) => {
  const next = { ...overrides }
  if (JSON.stringify(value) === JSON.stringify(defaultValue)) delete next[id]
  else next[id] = value
  return next
}

export const playlistLayoutFor = (id: string | undefined, settings: Pick<Settings, 'playlistHiddenColumns' | 'playlistColumnOrders' | 'playlistColumnWidths'>) => ({
  hiddenColumns: id !== undefined ? settings.playlistHiddenColumns[id] ?? PLAYLIST_DEFAULT_HIDDEN_COLUMNS : PLAYLIST_DEFAULT_HIDDEN_COLUMNS,
  columnOrder: id !== undefined ? settings.playlistColumnOrders[id] ?? PLAYLIST_DEFAULT_COLUMN_ORDER : PLAYLIST_DEFAULT_COLUMN_ORDER,
  columnWidths: id !== undefined ? settings.playlistColumnWidths[id] ?? {} : {},
})

export const staleSelectionFacet = (selection: Selection, facets: BrowseView['facets']): 'cat' | 'art' | null => {
  const missing = (selected: string[] | undefined, available: string[]) => selected?.some((value) => !available.includes(value)) ?? false
  if (missing(selection.cat, facets.cats)) return 'cat'
  if (missing(selection.art, facets.arts) || missing(selection.alb, facets.albs)) return 'art'
  return null
}

export const resizedColumnWidth = (startWidth: number, startX: number, clientX: number) =>
  Math.max(28, Math.round(startWidth + clientX - startX))

export const resizedPaneHeight = (startHeight: number, startY: number, clientY: number, maxHeight: number, zoom: number) =>
  Math.max(90, Math.min(maxHeight, Math.round(startHeight + (clientY - startY) / zoom)))

export const clearedTrackRating = (inherited: number | null) =>
  inherited === null ? null : { stars: inherited, explicit: false }

export const playbackQueue = (tracks: readonly PlaybackTrack[], requestedId: number) =>
  tracks.filter((track) => track.enabled || track.id === requestedId)

export const playbackStartAction = (uri: string | undefined, connected: boolean) =>
  uri?.startsWith('spotify:') && !connected ? 'connect' as const : 'play' as const

export const playbackAuthorizationPrompt = (outcome: PlayOutcome | undefined) =>
  typeof outcome === 'object' ? outcome.playbackAuthorizationRequired : null

export const pendingPlaybackTarget = (prompt: PlaybackAuthorizationPrompt, tracks: readonly PlaybackTrack[]) =>
  tracks.some((track) => track.id === prompt.targetTrackId) ? prompt.targetTrackId : null

export const playbackRetryReady = (connected: boolean, playbackAuthorized: boolean, awaitingAuthorization: boolean) =>
  connected && (!awaitingAuthorization || playbackAuthorized)

export const playbackOriginAction = (origin: PlaybackOrigin) => origin.kind === 'playlist'
  ? { type: 'playlist' as const, id: origin.id }
  : { type: 'source' as const, source: origin.source }

export const isCurrentTrack = (
  playing: { trackId: number | null; uri: string | null } | null,
  track: { id: number; uri: string },
) => playing?.trackId === track.id && playing.uri === track.uri

export const facetLabel = (title: string, value: string) =>
  title === 'Genre' && value === 'Uncategorized' ? 'No Genre' : value

type SortableTrack = Pick<Track, 'discNo' | 'trackNo' | 'name' | 'durationSecs' | 'art' | 'alb' | 'cat' | 'rating' | 'playCount' | 'kind' | 'bitrateKbps' | 'lastPlayedAt' | 'addedAt' | 'releaseDate'>

const sortValue = (track: SortableTrack, column: ColumnKey): string | number | null => {
  if (column === 'disc') return track.discNo ?? 1
  if (column === 'track') return track.trackNo
  if (column === 'name') return track.name
  if (column === 'time') return track.durationSecs
  if (column === 'artist') return track.art
  if (column === 'album') return track.alb
  if (column === 'genre') return track.cat
  if (column === 'rating') return track.rating?.stars ?? null
  if (column === 'plays') return track.playCount
  if (column === 'kind') return track.kind
  if (column === 'bitrate') return track.bitrateKbps
  if (column === 'lastPlayed') return track.lastPlayedAt
  if (column === 'added') return track.addedAt
  return track.releaseDate
}

export const compareTracks = (left: SortableTrack, right: SortableTrack, column: ColumnKey, desc: boolean) => {
  const primary: ColumnKey[] = column === 'track' ? ['disc', 'track'] : [column]
  const columns = [...primary, ...(['disc', 'track', 'artist', 'album', 'genre'] as ColumnKey[]).filter((key) => !primary.includes(key))]
  for (const key of columns) {
    const a = sortValue(left, key)
    const b = sortValue(right, key)
    if (a === null && b === null) continue
    if (a === null) return 1
    if (b === null) return -1
    const compared = typeof a === 'number' && typeof b === 'number'
      ? a - b
      : String(a).localeCompare(String(b), undefined, { sensitivity: 'base' })
    if (compared) return desc ? -compared : compared
  }
  return 0
}

export const playlistRows = <T extends SortableTrack>(tracks: readonly T[], column: ColumnKey | null, desc: boolean) => {
  const rows = tracks.map((track, upstreamIndex) => ({ track, upstreamIndex }))
  return column ? rows.sort((left, right) => compareTracks(left.track, right.track, column, desc)) : rows
}

export const dialogTabTarget = (current: number, count: number, backward: boolean) => {
  if (!count) return null
  if (current < 0) return backward ? count - 1 : 0
  if (backward && current === 0) return count - 1
  if (!backward && current === count - 1) return 0
  return null
}

export const overlayEditTargets = (tracks: readonly { id: number | null; uri: string }[]) => ({
  ids: [...new Set(tracks.flatMap((track) => track.id === null ? [] : [track.id]))],
  missingUris: [...new Set(tracks.filter((track) => track.id === null).map((track) => track.uri))],
})

export const COLUMN_SPECS: Record<ColumnKey, { width: string; numeric?: boolean }> = {
  disc: { width: '34px', numeric: true }, track: { width: '34px', numeric: true }, name: { width: 'minmax(160px, 1.6fr)' }, time: { width: '52px', numeric: true }, artist: { width: '1.1fr' },
  album: { width: '1.1fr' }, genre: { width: '.9fr' }, rating: { width: '84px' }, plays: { width: '48px', numeric: true }, kind: { width: '140px' },
  bitrate: { width: '64px', numeric: true }, lastPlayed: { width: '88px', numeric: true }, added: { width: '88px', numeric: true }, releaseDate: { width: '88px', numeric: true },
}

export const trackColumnHeadings = (label: (typeof labels)[Source]): Record<ColumnKey, string> => ({
  disc: 'Disc', track: '#', name: label.item[0].toUpperCase() + label.item.slice(1), time: 'Time', artist: label.facets[1], album: label.facets[2], genre: label.facets[0], rating: 'Rating', plays: 'Plays', kind: 'Kind', bitrate: 'Bit Rate', lastPlayed: 'Last Played', added: 'Date Added', releaseDate: 'Release Date',
})

export const trackGridColumns = (columns: ColumnKey[], widths: Partial<Record<ColumnKey, number>>, leading = '16px') =>
  `${leading} ${columns.map((column) => widths[column] === undefined ? COLUMN_SPECS[column].width : `${widths[column]}px`).join(' ')}`

export const moveBefore = <T>(items: T[], item: T, target?: T) => {
  if (!items.includes(item) || item === target) return items
  const next = items.filter((candidate) => candidate !== item)
  const index = target === undefined ? next.length : next.indexOf(target)
  if (index < 0) return items
  next.splice(index, 0, item)
  return next
}

export const moveToIndex = <T>(items: T[], item: T, target: number) => {
  const source = items.indexOf(item)
  if (source < 0) return items
  const next = items.filter((candidate) => candidate !== item)
  next.splice(Math.max(0, Math.min(target - (source < target ? 1 : 0), next.length)), 0, item)
  return next
}

export const insertionIndexAtY = (midpoints: number[], clientY: number) => {
  const index = midpoints.findIndex((midpoint) => clientY < midpoint)
  return index < 0 ? midpoints.length : index
}

export const mergeByUri = <T extends { uri: string }>(current: T[], incoming: T[]) => {
  const seen = new Set(current.map((item) => item.uri))
  return [...current, ...incoming.filter((item) => {
    if (seen.has(item.uri)) return false
    seen.add(item.uri)
    return true
  })]
}

export const menuPosition = (x: number, y: number, width: number, height: number, viewportWidth: number, viewportHeight: number, zoom: number, margin = 6) => {
  const left = Math.max(margin, Math.min(x, viewportWidth - width - margin))
  const preferredTop = y + height + margin <= viewportHeight ? y : y - height
  const top = Math.max(margin, Math.min(preferredTop, viewportHeight - height - margin))
  return { left: left / zoom, top: top / zoom }
}

export const contiguousRange = (indices: number[]) => {
  const sorted = [...indices].sort((left, right) => left - right)
  return sorted.length && sorted.every((row, offset) => row === sorted[0] + offset)
    ? { start: sorted[0], length: sorted.length }
    : undefined
}

export const DRAG_TYPE = 'application/x-retune'
export const DRAG_LOCAL_TYPE = 'application/x-retune-local'
export const SYNTHETIC_BASE = 2 ** 40

export const hasLocalTracks = (subject: PlaylistSubject) => subject.kind === 'tracks'
  ? subject.uris.some((uri) => uri.startsWith('file:'))
  : subject.albumUri.startsWith('file:')

export const labels = {
  music: { facets: ['Genre', 'Artist', 'Album'], item: 'song', icons: '♪', name: 'Music' },
  podcasts: { facets: ['Category', 'Podcaster', 'Show'], item: 'episode', icons: '🎙', name: 'Podcasts' },
  audiobooks: { facets: ['Category', 'Author', 'Book'], item: 'chapter', icons: '📖', name: 'Audiobooks' },
} as const

export function formatTime(seconds: number) {
  const hours = Math.floor(seconds / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)
  const secs = Math.floor(seconds % 60)
  return hours
    ? `${hours}:${String(minutes).padStart(2, '0')}:${String(secs).padStart(2, '0')}`
    : `${minutes}:${String(secs).padStart(2, '0')}`
}
