import { useEffect, useLayoutEffect, useRef } from 'react'
import type { ReactNode } from 'react'
import { menuPosition } from './ui.ts'

export function ContextMenu({ x, y, onClose, children }: { x: number; y: number; onClose: () => void; children: ReactNode }) {
  const menu = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    const element = menu.current
    if (!element) return
    const place = () => {
      const bounds = element.getBoundingClientRect()
      const zoom = Number((element.closest('.app-shell') as HTMLElement | null)?.style.zoom) || 1
      const position = menuPosition(x, y, bounds.width, bounds.height, window.innerWidth, window.innerHeight, zoom)
      element.style.left = `${position.left}px`
      element.style.top = `${position.top}px`
    }
    place()
    window.addEventListener('resize', place)
    return () => window.removeEventListener('resize', place)
  }, [x, y])
  useEffect(() => {
    const close = (event: PointerEvent) => { if (!(event.target as HTMLElement).closest('.context-menu')) onClose() }
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') onClose() }
    document.addEventListener('pointerdown', close)
    document.addEventListener('keydown', escape)
    return () => {
      document.removeEventListener('pointerdown', close)
      document.removeEventListener('keydown', escape)
    }
  }, [onClose])
  return <div ref={menu} className="column-menu context-menu popup-context-menu" style={{ left: x, top: y }}>{children}</div>
}

export function CheckboxMenu({ x, y, onClose, items }: {
  x: number
  y: number
  onClose: () => void
  items: { key: string; label: string; checked: boolean; disabled?: boolean; onChange: (checked: boolean) => void }[]
}) {
  return <ContextMenu x={x} y={y} onClose={onClose}>{items.map((item) => <label key={item.key}><input type="checkbox" checked={item.checked} disabled={item.disabled} onChange={(event) => item.onChange(event.target.checked)} />{item.label}</label>)}</ContextMenu>
}
export function RatingStars({ rating, explicit = false, onRate }: { rating: number | null; explicit?: boolean; onRate?: (stars: number) => void }) {
  return <span className={`rating-stars ${rating ? explicit ? 'explicit' : 'inherited' : 'empty'} ${onRate ? '' : 'inert'}`} aria-label={rating ? `${rating} out of 5 stars` : 'Unrated'}>
    {[1, 2, 3, 4, 5].map((star) => <button key={star} disabled={!onRate} aria-label={`${star} stars`} onClick={(event) => { event.stopPropagation(); onRate?.(star) }}>{star <= (rating ?? 0) ? '★' : '☆'}</button>)}
  </span>
}
