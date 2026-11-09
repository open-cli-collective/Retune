import { useEffect, useRef, useState } from 'react'
import { appGateway } from './appGateway.ts'
import { initialState } from './appState.ts'
import { lastfmGateway } from './lastfmGateway.ts'
import type { LastFmImportState } from './types.ts'
import { useTauriInvalidationSnapshot } from './useTauriInvalidationSnapshot.ts'

function message(reason: unknown): string {
  return typeof reason === 'string' ? reason : reason instanceof Error ? reason.message : 'Couldn’t complete Last.fm sync. Retry or check the Bug tab in Preferences.'
}

export function LastFmSync({ state }: { state: LastFmImportState }) {
  const [account, setAccount] = useState(initialState.lastfm)
  const [progress, setProgress] = useState<Awaited<ReturnType<typeof lastfmGateway.syncStatus>> | null>(null)
  const [pending, setPending] = useState<'sync' | 'connect' | 'finish' | 'preferences' | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [pollError, setPollError] = useState<string | null>(null)
  const [accountError, setAccountError] = useState<string | null>(null)
  const generation = useRef(0)
  const running = useRef(false)
  useTauriInvalidationSnapshot('lastfm-changed', lastfmGateway.accountState, (next) => { if (next) { setAccount(next); setAccountError(null) } }, (reason) => setAccountError(message(reason)))
  const syncing = pending === 'sync' || state.syncing
  const view = syncing ? progress ?? state : state
  useEffect(() => {
    if (!state.syncing && pending !== 'sync') return
    let active = true
    const operation = generation.current
    const current = () => active && operation === generation.current
    let timer: ReturnType<typeof setTimeout>
    const poll = async () => {
      try {
        const next = await lastfmGateway.syncStatus()
        if (current()) { setProgress(next); setPollError(null) }
      } catch (reason) {
        if (current()) setPollError(message(reason))
      } finally {
        if (current()) timer = setTimeout(() => void poll(), 750)
      }
    }
    void poll()
    return () => { active = false; clearTimeout(timer) }
  }, [state.syncing, pending])
  // Invalidation snapshots remain authoritative once the operation finishes.
  const displayed = syncing ? view : state
  const action = async (kind: 'sync' | 'connect' | 'finish' | 'preferences') => {
    if (running.current) return
    running.current = true
    generation.current += 1
    setPending(kind)
    setProgress(null)
    setError(null)
    setPollError(null)
    try {
      if (kind === 'sync') setProgress(await lastfmGateway.syncPlays())
      else if (kind === 'connect') setAccount(await lastfmGateway.connectAccount())
      else if (kind === 'finish') setAccount(await lastfmGateway.finishAccount())
      else await appGateway.openPreferences()
    } catch (reason) {
      setError(message(reason))
    } finally {
      running.current = false
      setPending(null)
    }
  }
  const needsConnection = !account.connected || account.reconnectRequired
  const problem = error ?? accountError ?? (syncing ? pollError : null) ?? account.problem ?? displayed.syncProblem
  const pages = view.syncProgress
  const status = problem ?? (!account.available ? 'Last.fm is unavailable in this build.' : syncing
    ? pages?.totalPages != null
      ? pages.downloadedPages >= pages.totalPages ? 'Reconciling downloaded plays…' : `Downloaded ${pages.downloadedPages.toLocaleString()} of ${pages.totalPages.toLocaleString()} pages`
      : 'Preparing Last.fm sync…'
    : displayed.lastSyncedAt ? `Last synced ${new Date(displayed.lastSyncedAt * 1000).toLocaleString()}${displayed.pendingReview ? ` · ${displayed.pendingReview.toLocaleString()} plays need review` : ''}`
      : 'Sync new Last.fm plays into your library.')
  return <section className="import-sync" aria-label="Last.fm sync">
    <div className="import-sync-copy"><strong>Last.fm{account.username ? ` · ${account.username}` : ''}</strong>
      <span role={problem ? 'alert' : 'status'}>{status}</span>
      {syncing && <progress aria-label="Last.fm sync progress" max={pages?.totalPages || 1} value={pages?.totalPages ? pages.downloadedPages : undefined} />}
    </div>
    <div className="import-sync-actions">
      {account.available && (account.pending
        ? <button type="button" disabled={!!pending} onClick={() => void action('finish')}>Finish connecting</button>
        : needsConnection ? <button type="button" disabled={!!pending} onClick={() => void action('connect')}>{account.reconnectRequired ? 'Reconnect Last.fm' : 'Connect Last.fm'}</button>
          : <button type="button" className="primary" disabled={syncing} onClick={() => void action('sync')}>{syncing ? 'Syncing…' : problem ? 'Retry sync' : 'Sync now'}</button>)}
      {problem && <button type="button" disabled={!!pending} onClick={() => void action('preferences')}>Connection settings / logs</button>}
    </div>
  </section>
}
