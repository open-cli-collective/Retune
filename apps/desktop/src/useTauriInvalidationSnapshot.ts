import { useEffect, useEffectEvent } from 'react'
import { listen } from '@tauri-apps/api/event'
import { subscribeInvalidationThenSnapshot } from './ipc.ts'

export function useTauriInvalidationSnapshot<T>(
  event: string,
  snapshot: () => Promise<T>,
  handler: (payload: T) => void,
  onError: (error: unknown) => void,
) {
  const read = useEffectEvent(snapshot)
  const handle = useEffectEvent(handler)
  const reportError = useEffectEvent(onError)
  useEffect(() => {
    let active = true
    const subscription = subscribeInvalidationThenSnapshot(
      (invalidate) => listen(event, invalidate),
      () => read(),
      (value) => handle(value),
      () => active,
      (error) => { if (active) reportError(error) },
    )
    void subscription.catch((error) => { if (active) reportError(error) })
    return () => {
      active = false
      void subscription.then((stop) => stop()).catch(() => {})
    }
  }, [event])
}
