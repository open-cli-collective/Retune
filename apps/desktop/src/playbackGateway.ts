import { tauriInvoker, type Invoker } from './ipc.ts'
import type { PlaybackTrack, PlayOutcome, RepeatMode } from './types.ts'

export const playbackEvents = { action: 'player-action' } as const

export function createPlaybackGateway(invoke: Invoker) {
  return {
    play: (snapshot: readonly PlaybackTrack[], startIndex: number) => invoke<PlayOutcome>('play_tracks', {
      resources: snapshot.map(({ id, uri }) => ({ id, uri })),
      startIndex,
    }),
    toggle: () => invoke<void>('player_toggle'),
    previous: () => invoke<void>('player_prev'),
    next: () => invoke<void>('player_next'),
    setVolume: (volume: number) => invoke<void>('player_set_volume', { volume }),
    seek: (seconds: number) => invoke<void>('player_seek', { seconds }),
    setRepeat: (mode: RepeatMode) => invoke<void>('set_repeat', { mode }),
    setShuffle: (shuffle: boolean) => invoke<void>('set_shuffle', { shuffle }),
  }
}

export const playbackGateway = createPlaybackGateway(tauriInvoker)
