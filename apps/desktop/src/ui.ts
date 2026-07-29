import type { ColumnKey, PlaybackTrack, PlaylistSubject, Track } from './types.ts'

export type NativeDragEvent = { type: 'enter'; paths: string[] } | { type: 'over' } | { type: 'drop' } | { type: 'leave' }

export const nextNativeDragActive = (active: boolean, event: NativeDragEvent) => {
  if (event.type === 'enter') return event.paths.length > 0
  return event.type === 'over' ? active : false
}

export const normalizeZoom = (zoom: number, min: number, max: number) =>
  Math.min(max, Math.max(min, Math.round(zoom * 100) / 100))

export const resizedColumnWidth = (startWidth: number, startX: number, clientX: number) =>
  Math.max(28, Math.round(startWidth + clientX - startX))

export const resizedPaneHeight = (startHeight: number, startY: number, clientY: number, maxHeight: number, zoom: number) =>
  Math.max(90, Math.min(maxHeight, Math.round(startHeight + (clientY - startY) / zoom)))

export const clearedTrackRating = (inherited: number | null) =>
  inherited === null ? null : { stars: inherited, explicit: false }

export const playbackQueue = (tracks: readonly PlaybackTrack[], requestedId: number) =>
  tracks.filter((track) => track.enabled || track.id === requestedId)

export const facetLabel = (title: string, value: string) =>
  title === 'Genre' && value === 'Uncategorized' ? 'No Genre' : value

const sortValue = (track: Track, column: ColumnKey): string | number | null => {
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

export const compareTracks = (left: Track, right: Track, column: ColumnKey, desc: boolean) => {
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

export type DragRange = { start: number; length: number }

export const parseDragRange = (value: string): DragRange | undefined => {
  try {
    const range = JSON.parse(value) as Partial<DragRange>
    if (Number.isInteger(range.start) && Number.isInteger(range.length) && range.start! >= 0 && range.length! > 0) return range as DragRange
  } catch {}
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
