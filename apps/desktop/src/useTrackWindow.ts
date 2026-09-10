import { useCallback, useLayoutEffect, useRef, useState } from 'react'

// Both lists have fixed CSS row heights; scroll offsets are unscaled by CSS zoom.
export function useTrackWindow(count: number, rowHeight: number, focusedIndex: number) {
  const scroll = useRef<HTMLDivElement>(null)
  const [viewport, setViewport] = useState({ top: 0, height: 600 })
  const overscan = 12
  const visibleCount = Math.ceil(viewport.height / rowHeight) + overscan * 2
  const first = Math.max(0, Math.min(Math.floor(viewport.top / rowHeight) - overscan, count - visibleCount))
  const last = Math.min(count, first + visibleCount)
  const readViewport = useCallback(() => {
    const element = scroll.current
    if (element) setViewport(current => {
      const next = { top: element.scrollTop, height: element.clientHeight || 600 }
      return current.top === next.top && current.height === next.height ? current : next
    })
  }, [])
  useLayoutEffect(() => {
    readViewport()
    const observer = new ResizeObserver(readViewport)
    if (scroll.current) observer.observe(scroll.current)
    return () => observer.disconnect()
  }, [count, readViewport])
  const reveal = (index: number) => {
    const element = scroll.current
    if (!element || index < 0) return
    const top = index * rowHeight
    const bottom = top + rowHeight + (element.firstElementChild as HTMLElement).offsetHeight
    if (top < element.scrollTop) element.scrollTop = top
    else if (bottom > element.scrollTop + element.clientHeight) element.scrollTop = Math.max(0, bottom - (element.clientHeight || 600))
    readViewport()
  }
  const indices = Array.from({ length: last - first }, (_, index) => first + index)
  // Keep the keyboard/pointer's current row mounted when it leaves the viewport.
  if (focusedIndex >= 0 && (focusedIndex < first || focusedIndex >= last)) indices.push(focusedIndex)
  indices.sort((left, right) => left - right)
  return { scroll, readViewport, reveal, indices }
}
