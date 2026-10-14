import { useEffect, useRef, useState } from 'react'
import { spotifyGateway } from './spotifyGateway'
import type { PlaylistListView, ShadowCandidate } from './types'
import { ModalDialog } from './viewShared'

export function ShadowDiscovery({ playlists, onClose }: { playlists: PlaylistListView[]; onClose: () => void }) {
  const [candidates, setCandidates] = useState<ShadowCandidate[]>()
  const [error, setError] = useState('')
  const [busy, setBusy] = useState<string>()
  const pending = useRef(false)
  const [retry, setRetry] = useState(0)
  useEffect(() => {
    let active = true
    setError('')
    setCandidates(undefined)
    spotifyGateway.discoverShadows().then(rows => { if (active) setCandidates(rows) })
      .catch(error => { if (active) setError(String(error)) })
    return () => { active = false }
  }, [retry])
  const shadow = async (candidate: ShadowCandidate) => {
    if (pending.current) return
    pending.current = true
    setBusy(candidate.id)
    setError('')
    try { await spotifyGateway.shadowPlaylist(candidate.id, candidate.account) }
    catch (error) { setError(String(error)) }
    finally { pending.current = false; setBusy(undefined) }
  }
  return <ModalDialog className="get-info shadow-discovery" labelledBy="shadow-discovery-title" onCancel={busy ? undefined : onClose} closeOnBackdrop>
    <h2 id="shadow-discovery-title">Shadow playlists</h2>
    <p>Keep a read-only copy of a saved Spotify playlist in Retune. Shadows refresh automatically and keep their last successful tracks if refresh fails.</p>
    <p>Discover Weekly and Release Radar appear here when saved in your Spotify library.</p>
    {error && <p role="alert">{error}</p>}
    {!candidates && !error && <p role="status">Discovering your saved playlists…</p>}
    <div className="shadow-candidates">{candidates?.map(candidate => {
      const shadowed = playlists.some(p => p.id === candidate.id && p.shadow)
      return <div className="shadow-candidate" key={candidate.id}><div><strong>{candidate.name}</strong><small>by {candidate.owner}</small></div><button type="button" disabled={!!busy || shadowed} aria-label={`Shadow ${candidate.name}`} onClick={() => void shadow(candidate)}>{busy === candidate.id ? 'Shadowing…' : shadowed ? 'Shadowed' : 'Shadow'}</button></div>
    })}</div>
    {candidates?.length === 0 && <p>No saved playlists from other creators were found.</p>}
    <div className="modal-actions"><button type="button" disabled={!!busy} onClick={() => setRetry(r => r + 1)}>Discover again</button><button type="button" disabled={!!busy} onClick={onClose}>Done</button></div>
  </ModalDialog>
}
