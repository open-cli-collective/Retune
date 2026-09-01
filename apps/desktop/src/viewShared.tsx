import { Children, cloneElement, isValidElement, useEffect, useLayoutEffect, useRef } from 'react'
import type { ReactElement, ReactNode, RefObject } from 'react'
import { dialogTabTarget, menuPosition } from './ui.ts'

const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex]:not([tabindex="-1"])'

export function ModalDialog({ className, labelledBy, dialogRef, onCancel, onSubmit, closeOnBackdrop = false, children }: {
  className: string
  labelledBy: string
  dialogRef?: RefObject<HTMLFormElement | null>
  onCancel?: () => void
  onSubmit?: () => void | Promise<void>
  closeOnBackdrop?: boolean
  children: ReactNode
}) {
  const internalDialog = useRef<HTMLFormElement>(null)
  const dialog = dialogRef ?? internalDialog
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null
    const element = dialog.current
    if (element && !element.contains(document.activeElement)) (element.querySelector<HTMLElement>('[autofocus]') ?? element.querySelector<HTMLElement>(FOCUSABLE) ?? element).focus()
    return () => previous?.focus()
  }, [dialog])
  return <div className="modal-backdrop" role="presentation" onMouseDown={(event) => {
    if (closeOnBackdrop && event.target === event.currentTarget) onCancel?.()
  }}><form ref={dialog} className={className} role="dialog" aria-modal="true" aria-labelledby={labelledBy} tabIndex={-1} onSubmit={(event) => {
    event.preventDefault()
    void onSubmit?.()
  }} onKeyDown={(event) => {
    if (event.key === 'Escape' && onCancel) {
      event.preventDefault()
      event.stopPropagation()
      onCancel()
      return
    }
    if (event.key !== 'Tab') return
    const focusable = [...event.currentTarget.querySelectorAll<HTMLElement>(FOCUSABLE)]
    const target = dialogTabTarget(focusable.indexOf(document.activeElement as HTMLElement), focusable.length, event.shiftKey)
    if (target === null) return
    event.preventDefault()
    focusable[target].focus()
  }}>{children}</form></div>
}

export function ContextMenu({ x, y, onClose, children }: { x: number; y: number; onClose: () => void; children: ReactNode }) {
  const menu = useRef<HTMLDivElement>(null)
  const previousFocus = useRef<HTMLElement | null>(null)
  const closeMenu = useRef(onClose)
  closeMenu.current = onClose
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
    const close = (event: PointerEvent) => { if (!(event.target as HTMLElement).closest('.context-menu')) closeMenu.current() }
    previousFocus.current = document.activeElement as HTMLElement | null
    const element = menu.current
    const firstItem = element?.querySelector<HTMLElement>('[role^="menuitem"]:not([aria-disabled="true"]), button:not(:disabled)')
    ;(firstItem ?? element)?.focus()
    document.addEventListener('pointerdown', close)
    return () => {
      document.removeEventListener('pointerdown', close)
      previousFocus.current?.focus()
    }
  }, [])
  return <div ref={menu} className="column-menu context-menu popup-context-menu" role="menu" tabIndex={-1} style={{ left: x, top: y }} onKeyDown={(event) => {
    if (event.key === 'Escape') {
      event.preventDefault()
      event.stopPropagation()
      closeMenu.current()
      return
    }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return
    const items = [...event.currentTarget.querySelectorAll<HTMLElement>('[role^="menuitem"]:not([aria-disabled="true"]), button:not(:disabled)')]
    if (!items.length) return
    event.preventDefault()
    const current = items.indexOf(document.activeElement as HTMLElement)
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1 : (current + (event.key === 'ArrowUp' ? -1 : 1) + items.length) % items.length
    items[next].focus()
  }}>{Children.map(children, (child) => {
      if (!isValidElement(child) || child.type !== 'button') return child
      const button = child as ReactElement<React.ButtonHTMLAttributes<HTMLButtonElement>>
      return cloneElement(button, { role: 'menuitem', tabIndex: -1, 'aria-disabled': button.props.disabled || undefined })
    })}</div>
}

export function CheckboxMenu({ x, y, onClose, items }: {
  x: number
  y: number
  onClose: () => void
  items: { key: string; label: string; checked: boolean; disabled?: boolean; onChange: (checked: boolean) => void }[]
}) {
  return <ContextMenu x={x} y={y} onClose={onClose}>{items.map((item) => <label key={item.key} role="menuitemcheckbox" aria-checked={item.checked} aria-disabled={item.disabled || undefined} tabIndex={-1}><input type="checkbox" checked={item.checked} disabled={item.disabled} tabIndex={-1} onChange={(event) => item.onChange(event.target.checked)} />{item.label}</label>)}</ContextMenu>
}
export function RatingStars({ rating, explicit = false, onRate }: { rating: number | null; explicit?: boolean; onRate?: (stars: number) => void }) {
  return <span className={`rating-stars ${rating ? explicit ? 'explicit' : 'inherited' : 'empty'} ${onRate ? '' : 'inert'}`} aria-label={rating ? `${rating} out of 5 stars` : 'Unrated'}>
    {[1, 2, 3, 4, 5].map((star) => <button type="button" key={star} disabled={!onRate} aria-label={`${star} stars`} onClick={(event) => { event.stopPropagation(); onRate?.(star) }}>{star <= (rating ?? 0) ? '★' : '☆'}</button>)}
  </span>
}
