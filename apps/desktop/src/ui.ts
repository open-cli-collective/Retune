export type NativeDragEvent = { type: 'enter'; paths: string[] } | { type: 'over' } | { type: 'drop' } | { type: 'leave' }

export const nextNativeDragActive = (active: boolean, event: NativeDragEvent) => {
  if (event.type === 'enter') return event.paths.length > 0
  return event.type === 'over' ? active : false
}

export const normalizeZoom = (zoom: number, min: number, max: number) =>
  Math.min(max, Math.max(min, Math.round(zoom * 100) / 100))

export const clearedTrackRating = (inherited: number | null) =>
  inherited === null ? null : { stars: inherited, explicit: false }

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
