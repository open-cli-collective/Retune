import type { PlayerState, Playing } from './types.ts'

// Native position is presentation data; semantic playback changes still use the reducer.
export function createPlaybackProgress() {
  let elapsed = 0
  const listeners = new Set<() => void>()
  return {
    getSnapshot: () => elapsed,
    subscribe: (listener: () => void) => {
      listeners.add(listener)
      return () => { listeners.delete(listener) }
    },
    update: (next: number) => {
      if (next === elapsed) return
      elapsed = next
      listeners.forEach(listener => listener())
    },
  }
}

export function onlyPlaybackProgressChanged(current: Playing | null, next: PlayerState) {
  return current !== null && !current.simulated
    && (Object.keys(next) as (keyof PlayerState)[]).every(key => key === 'elapsed' || current[key] === next[key])
}
