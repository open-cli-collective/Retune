import { Channel, invoke } from '@tauri-apps/api/core'
import type { ImportSummary, PlaybackAuthorizationPrompt, PlayerState } from './types.ts'

export type Invoker = <T>(command: string, args?: Record<string, unknown>) => Promise<T>

export const tauriInvoker: Invoker = (command, args) => invoke(command, args)

export type MainEvent =
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
  const value = await snapshot()
  if (active() && !eventSeen) install(value)
  return unlisten
}

export async function subscribeInvalidationThenSnapshot<T>(
  subscribe: (invalidate: () => void) => Promise<() => void>,
  snapshot: () => Promise<T>,
  install: (value: T) => void,
  active: () => boolean,
) {
  let generation = 0
  let invalidated = false
  const unlisten = await subscribe(() => {
    invalidated = true
    const request = ++generation
    void snapshot().then((value) => {
      if (active() && request === generation) install(value)
    })
  })
  if (!active()) return unlisten
  const request = ++generation
  const value = await snapshot()
  if (active() && !invalidated && request === generation) install(value)
  return unlisten
}

export async function subscriptionsThenSnapshot(
  subscriptions: Array<Promise<() => void>>,
  snapshot: () => Promise<unknown>,
  active: () => boolean,
) {
  const unlistens = await Promise.all(subscriptions)
  if (active()) await snapshot()
  return () => { for (const unlisten of unlistens) unlisten() }
}

export type ExternalDestination = { kind: 'lastFm' } | { kind: 'spotifyAlbum'; id: string }

export const openExternalDestination = (destination: ExternalDestination) =>
  tauriInvoker<void>('open_external_destination', { destination })
