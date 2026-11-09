import type { ShadowProgress as Progress } from './spotifyGateway'

export function ShadowProgress({ progress }: { progress?: Progress }) {
  const saving = progress && progress.completed === progress.total
  const label = !progress ? 'Connecting to Spotify and reading playlist…'
    : saving ? 'Saving shadow…'
    : `Loaded details for ${progress.completed.toLocaleString()} of ${progress.total.toLocaleString()} songs`
  return <div className="shadow-progress" role="status">
    <span>{label}</span>
    <progress aria-label={label} max={progress?.total || 1} value={progress && !saving ? progress.completed : undefined} />
    <small>Large playlists may take a little longer.</small>
  </div>
}
