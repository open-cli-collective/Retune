import { Channel, invoke } from '@tauri-apps/api/core'
import type { ImportSummary, PlaybackAuthorizationPrompt, PlayerState } from './types.ts'

export type Invoker = <T>(command: string, args?: Record<string, unknown>) => Promise<T>

export const tauriInvoker: Invoker = (command, args) => invoke(command, args)

export type SpotifyPlayRequest = { uri: string; name: string; artist: string; album: string; durationSecs: number }

export type MainEvent =
  | { type: 'spotifyPlayRequested'; payload: SpotifyPlayRequest }
  | { type: 'playerState'; payload: PlayerState }
  | { type: 'playbackAuthorizationRequired'; payload: PlaybackAuthorizationPrompt }
  | { type: 'operationError'; payload: string }
  | { type: 'operationRecovered' }
  | { type: 'localImportComplete'; payload: ImportSummary }
  | { type: 'startupNotice'; payload: string }

export type MainEventHandlers = {
  [Kind in MainEvent['type']]: Extract<MainEvent, { type: Kind }> extends { payload: infer Payload }
    ? (payload: Payload) => void
    : () => void
}

export function dispatchMainEvent(event: MainEvent, handlers: MainEventHandlers) {
  switch (event.type) {
    case 'spotifyPlayRequested': handlers.spotifyPlayRequested(event.payload); break
    case 'playerState': handlers.playerState(event.payload); break
    case 'playbackAuthorizationRequired': handlers.playbackAuthorizationRequired(event.payload); break
    case 'operationError': handlers.operationError(event.payload); break
    case 'operationRecovered': handlers.operationRecovered(); break
    case 'localImportComplete': handlers.localImportComplete(event.payload); break
    case 'startupNotice': handlers.startupNotice(event.payload); break
  }
}

type MainEventChannel = { onmessage: (event: MainEvent) => void }

export function createMainEventSubscription(
  invoker: Invoker,
  createChannel: (handler: (event: MainEvent) => void) => MainEventChannel,
) {
  let operations = Promise.resolve<unknown>(undefined)
  const enqueue = (operation: () => Promise<unknown>) => {
    operations = operations.catch(() => undefined).then(operation)
    return operations
  }
  return (handler: (event: MainEvent) => void, onError: (error: unknown) => void) => {
    const channel = createChannel(handler)
    let generation: number | undefined
    let stopped = false
    void enqueue(async () => {
      generation = await invoker<number>('subscribe_main_events', { channel })
    }).catch(onError)
    return () => {
      if (stopped) return
      stopped = true
      void enqueue(async () => {
        if (generation !== undefined) {
          await invoker<void>('unsubscribe_main_events', { generation })
        }
      }).catch(onError)
    }
  }
}

export const subscribeMainEvents = createMainEventSubscription(
  tauriInvoker,
  (handler) => new Channel<MainEvent>(handler),
)

export async function subscribeThenSnapshot<T>(
  subscribe: (install: (value: T) => void) => Promise<() => void>,
  snapshot: () => Promise<T>,
  install: (value: T) => void,
  active: () => boolean,
) {
  let eventSeen = false
  const unlisten = await subscribe((value) => {
    eventSeen = true
    if (active()) install(value)
  })
  if (!active()) return unlisten
  try {
    const value = await snapshot()
    if (active() && !eventSeen) install(value)
  } catch (error) {
    unlisten()
    throw error
  }
  return unlisten
}

export async function subscribeInvalidationThenSnapshot<T>(
  subscribe: (invalidate: () => void) => Promise<() => void>,
  snapshot: () => Promise<T>,
  install: (value: T) => void,
  active: () => boolean,
  onError: (error: unknown) => void = () => {},
) {
  let generation = 0
  let stopped = false
  let unlisten: (() => void) | undefined
  const requestSnapshot = () => {
    if (stopped || !active()) return
    const request = ++generation
    void snapshot().then((value) => {
      if (!stopped && active() && request === generation) install(value)
    }).catch((error) => {
      if (!stopped && active() && request === generation) onError(error)
    })
  }
  unlisten = await subscribe(requestSnapshot)
  const stop = () => {
    if (stopped) return
    stopped = true
    generation++
    unlisten?.()
  }
  if (!active()) {
    stop()
    return stop
  }
  const request = ++generation
  try {
    const value = await snapshot()
    if (!stopped && active() && request === generation) install(value)
  } catch (error) {
    stop()
    throw error
  }
  return stop
}

export async function subscriptionsThenSnapshot(
  subscriptions: Array<Promise<() => void>>,
  snapshot: () => Promise<unknown>,
  active: () => boolean,
) {
  const results = await Promise.allSettled(subscriptions)
  const unlistens = results.flatMap((result) => result.status === 'fulfilled' ? [result.value] : [])
  const stop = () => { for (const unlisten of unlistens) unlisten() }
  const failure = results.find((result): result is PromiseRejectedResult => result.status === 'rejected')
  if (failure) {
    stop()
    throw failure.reason
  }
  if (!active()) {
    stop()
    return stop
  }
  try {
    await snapshot()
  } catch (error) {
    stop()
    throw error
  }
  return stop
}

export type ExternalDestination = { kind: 'lastFm' } | { kind: 'spotifyAlbum'; id: string }

export const openExternalDestination = (destination: ExternalDestination) =>
  tauriInvoker<void>('open_external_destination', { destination })
