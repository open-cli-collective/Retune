import { invoke } from '@tauri-apps/api/core'
import { useEffect, useMemo, useRef, useState } from 'react'
import type { MetadataValues, PlayThresholdPercent, Settings, Theme, Track, TrackInfo } from './types.ts'
import { clearedTrackRating } from './ui.ts'
import { RatingStars } from './viewShared.tsx'

const streamingQualities = [
  ['Normal', 96],
  ['High', 160],
  ['Very High', 320],
] as const
const playThresholds: PlayThresholdPercent[] = [50, 75, 90, 100]

function AutocompleteInput({ suggestions, value, onValue, placeholder }: {
  suggestions: string[]
  value: string
  onValue: (value: string) => void
  placeholder?: string
}) {
  return <input value={value} placeholder={placeholder} onChange={(event) => {
    const input = event.target
    const typed = input.value
    const insertion = (event.nativeEvent as InputEvent).inputType === 'insertText' && input.selectionStart === typed.length
    const suggestion = insertion && suggestions.find((candidate) => candidate.length > typed.length && candidate.toLowerCase().startsWith(typed.toLowerCase()))
    if (suggestion) {
      input.value = suggestion
      input.setSelectionRange(typed.length, suggestion.length)
    }
    onValue(suggestion || typed)
  }} />
}

export function GetInfo({ track, onCancel, onSaved, onError }: { track: TrackInfo; onCancel: () => void; onSaved: () => void; onError: (error: string) => void }) {
  const [draft, setDraft] = useState({ name: track.name, art: track.art, alb: track.alb, cat: track.cat === 'Uncategorized' ? '' : track.cat })
  const [suggestions, setSuggestions] = useState<MetadataValues>({ arts: [], albs: [], cats: [] })
  const [rating, setRating] = useState(track.rating)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => {
    dialog.current?.focus()
    invoke<MetadataValues>('metadata_values').then(setSuggestions).catch((error) => onError(String(error)))
  }, [])
  const genres = useMemo(() => [...new Map([...suggestions.cats, ...track.genres].filter((genre) => genre && genre !== 'Uncategorized').map((genre) => [genre.toLowerCase(), genre] as const)).values()]
    .sort((left, right) => left.toLowerCase().localeCompare(right.toLowerCase())), [suggestions.cats, track.genres])
  const rate = (stars: number) => setRating((current) => current?.explicit && current.stars === stars
    ? track.inheritedRating === null ? null : { stars: track.inheritedRating, explicit: false }
    : { stars, explicit: true })
  const save = async () => {
    try {
      const ratingChange = { stars: rating?.explicit ? rating.stars : null }
      const edit = Object.fromEntries(Object.entries(draft).filter(([, value]) => value.trim() !== ''))
      await invoke('edit_track', { id: track.id, edit: { ...edit, ratingChange } })
      onSaved()
    } catch (error) {
      onError(String(error))
    }
  }
  const field = (key: keyof typeof draft) => ({
    value: draft[key],
    onChange: (event: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, [key]: event.target.value }),
  })
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info" role="dialog" aria-modal="true" aria-labelledby="get-info-title" tabIndex={-1} ref={dialog}>
      <h2 id="get-info-title">Get Info</h2>
      {track.localPath ? <label>File<input className="file-path" value={track.localPath} title={track.localPath} readOnly /></label> : <label>Spotify ID<input value={track.uri} readOnly /></label>}
      <label>Name<input {...field('name')} /></label>
      <label>Artist<AutocompleteInput suggestions={suggestions.arts} value={draft.art} onValue={(art) => setDraft({ ...draft, art })} /></label>
      <label>Album<AutocompleteInput suggestions={suggestions.albs} value={draft.alb} onValue={(alb) => setDraft({ ...draft, alb })} /></label>
      <label>Genre<AutocompleteInput suggestions={genres} value={draft.cat} onValue={(cat) => setDraft({ ...draft, cat })} placeholder={track.cat === 'Uncategorized' ? 'Uncategorized' : undefined} /></label>
      <div className="genre-hint">normalize freely, e.g. “Operatic Rock” → “Rock”</div>
      <div className="info-rating"><span>Track Rating</span><RatingStars rating={rating?.stars ?? null} explicit={rating?.explicit} onRate={rate} /><button disabled={!rating?.explicit} onClick={() => setRating(clearedTrackRating(track.inheritedRating))}>Clear rating</button></div>
      {track.origCat && draft.cat !== track.origCat && <div className="override-banner">Spotify reports this as “{track.origCat}”. Your overlay wins in Retune.</div>}
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => void save()}>Save Overlay</button></div>
    </div>
  </div>
}

export function MultipleItemInformation({ tracks, onCancel, onSaved, onError }: { tracks: Track[]; onCancel: () => void; onSaved: () => void; onError: (error: string) => void }) {
  type Field = 'art' | 'alb' | 'cat'
  const [draft, setDraft] = useState<Partial<Record<Field, string>>>({})
  const [suggestions, setSuggestions] = useState<MetadataValues>({ arts: [], albs: [], cats: [] })
  const [rating, setRating] = useState<number | null | undefined>(undefined)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => {
    dialog.current?.focus()
    invoke<MetadataValues>('metadata_values').then(setSuggestions).catch((error) => onError(String(error)))
  }, [])
  const placeholder = (key: Field) => tracks.every((track) => track[key] === tracks[0][key]) ? tracks[0][key] : 'Mixed'
  const save = async () => {
    try {
      await invoke('set_track_infos', {
        ids: tracks.map((track) => track.id),
        edit: { ...draft, ...(rating === undefined ? {} : { ratingChange: { stars: rating } }) },
      })
      onSaved()
    } catch (error) {
      onError(String(error))
    }
  }
  const field = (key: Field, values: string[]) => ({
    suggestions: values,
    value: draft[key] ?? '',
    placeholder: placeholder(key),
    onValue: (value: string) => setDraft((current) => {
      const next = { ...current }
      if (value.trim()) next[key] = value
      else delete next[key]
      return next
    }),
  })
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info" role="dialog" aria-modal="true" aria-labelledby="multiple-item-information-title" tabIndex={-1} ref={dialog}>
      <h2 id="multiple-item-information-title">Editing {tracks.length} items</h2>
      <label>Artist<AutocompleteInput {...field('art', suggestions.arts)} /></label>
      <label>Album<AutocompleteInput {...field('alb', suggestions.albs)} /></label>
      <label>Genre<AutocompleteInput {...field('cat', suggestions.cats)} /></label>
      <div className="info-rating bulk-rating"><span>Rating</span><RatingStars rating={rating ?? null} explicit={rating !== undefined && rating !== null} onRate={setRating} /><button className={rating === undefined ? 'active' : ''} onClick={() => setRating(undefined)}>No Change</button><button className={rating === null ? 'active' : ''} onClick={() => setRating(null)}>Clear</button></div>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => void save()}>Save Overlay</button></div>
    </div>
  </div>
}

export function SetupLibrary({ settings, connected, onCancel, onConnect, onSync }: {
  settings: Settings
  connected: boolean
  onCancel: () => void
  onConnect: (clientId: string) => void
  onSync: (clientId: string) => void
}) {
  const [clientId, setClientId] = useState(settings.spotifyClientId)
  const [webApi, setWebApi] = useState(true)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => { dialog.current?.focus() }, [])
  const trimmedClientId = clientId.trim()
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info preferences setup-library" role="dialog" aria-modal="true" aria-labelledby="setup-library-title" tabIndex={-1} ref={dialog}>
      <h2 id="setup-library-title">Set Up Your Library</h2>
      <div className="setup-content">
        <p>Retune reads your Spotify library through the Web API and builds a local overlay. Confirm three things, then sync.</p>
        <div className="setup-step"><span className="step-number">1</span><label><strong>Spotify app Client ID</strong><input value={clientId} onChange={(event) => setClientId(event.target.value)} /><small>Create one at developer.spotify.com → Dashboard → your app.</small></label></div>
        <div className="setup-step"><span className="step-number">2</span><div><label className="setup-check"><input type="checkbox" checked={webApi} onChange={(event) => setWebApi(event.target.checked)} /><strong>Web API enabled</strong></label><small>The app must have the Web API scope turned on in its dashboard settings.</small></div></div>
        <div className="setup-step"><span className="step-number">3</span><div><strong>Spotify connection</strong><div className={`setup-status ${connected ? 'connected' : ''}`}><span className="connection-dot" /><span>{connected ? 'Connected' : 'Not connected'}</span>{connected ? <span className="detected">✓ auto-detected</span> : <button onClick={() => onConnect(trimmedClientId)}>Connect…</button>}</div></div></div>
      </div>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" disabled={!trimmedClientId || !webApi || !connected} onClick={() => onSync(trimmedClientId)}>Sync</button></div>
    </div>
  </div>
}

export function Preferences({ settings, onZoom, onCancel, onSave }: {
  settings: Settings
  onZoom: (zoom: number) => void
  onCancel: () => void
  onSave: (settings: Pick<Settings, 'theme' | 'browserVisible' | 'browserPanes' | 'autoAddSpotifyLibrary' | 'autoConnect' | 'spotifyClientId' | 'playbackBackend' | 'streamingBitrate' | 'normalizeVolume' | 'gapless' | 'playThresholdPercent'>) => void
}) {
  type PreferenceTab = 'appearance' | 'library' | 'audio'
  const [tab, setTab] = useState<PreferenceTab>('appearance')
  const [theme, setTheme] = useState(settings.theme)
  const [browserVisible, setBrowserVisible] = useState(settings.browserVisible)
  const [browserPanes, setBrowserPanes] = useState(settings.browserPanes)
  const [autoAdd, setAutoAdd] = useState(settings.autoAddSpotifyLibrary)
  const [autoConnect, setAutoConnect] = useState(settings.autoConnect)
  const [clientId, setClientId] = useState(settings.spotifyClientId)
  const [playbackBackend, setPlaybackBackend] = useState(settings.playbackBackend)
  const [streamingBitrate, setStreamingBitrate] = useState(settings.streamingBitrate)
  const [normalizeVolume, setNormalizeVolume] = useState(settings.normalizeVolume)
  const [gapless, setGapless] = useState(settings.gapless)
  const [playThresholdPercent, setPlayThresholdPercent] = useState(settings.playThresholdPercent)
  const dialog = useRef<HTMLDivElement>(null)
  useEffect(() => { dialog.current?.focus() }, [])
  const tabs: [PreferenceTab, string, string][] = [
    ['appearance', '◑', 'Appearance'],
    ['library', '♫', 'Library'],
    ['audio', '◉', 'Audio'],
  ]
  const themeOptions: [Theme, string, string][] = [
    ['system', 'System', 'Follow the OS appearance, switching automatically.'],
    ['light', 'Light', 'Always use the light theme.'],
    ['dark', 'Dark', 'Always use the dark theme.'],
  ]
  return <div className="modal-backdrop" role="presentation">
    <div className="get-info preferences" role="dialog" aria-modal="true" aria-labelledby="preferences-title" tabIndex={-1} ref={dialog}>
      <h2 id="preferences-title">Preferences</h2>
      <div className="preference-toolbar" role="tablist">
        {tabs.map(([value, glyph, label]) => <button key={value} id={`preferences-${value}-tab`} className={tab === value ? 'active' : ''} role="tab" aria-selected={tab === value} aria-controls={`preferences-${value}`} onClick={() => setTab(value)}><span aria-hidden="true">{glyph}</span>{label}</button>)}
      </div>
      <div className="preference-content" id={`preferences-${tab}`} role="tabpanel" aria-labelledby={`preferences-${tab}-tab`}>
        {tab === 'appearance' && <>
          <section className="preference-group"><h3>Theme</h3><div className="preference-inset preference-options">
            {themeOptions.map(([value, label, help]) => <label className="preference-choice" key={value}><input type="radio" name="theme" value={value} checked={theme === value} onChange={() => setTheme(value)} /><span><strong>{label}</strong><small>{help}</small></span></label>)}
          </div></section>
          <section className="preference-group"><h3>Text size <small>⌘+ · ⌘− · ⌘0</small></h3><div className="preference-inset preference-row">
            {([['Small', .9], ['Medium', 1], ['Large', 1.15]] as const).map(([label, zoom]) => <label className="preference-choice" key={label}><input type="radio" name="text-size" checked={Math.abs(settings.zoom - zoom) <= .03} onChange={() => onZoom(zoom)} /><span>{label}</span></label>)}
          </div></section>
          <section className="preference-group"><h3>Column browser <small>⌘B</small></h3><div className="preference-inset">
            <div className="preference-row">{([['Show', true], ['Hide', false]] as const).map(([label, visible]) => <label className="preference-choice" key={label}><input type="radio" name="browser-visible" checked={browserVisible === visible} onChange={() => setBrowserVisible(visible)} /><span>{label}</span></label>)}</div>
            <div className="preference-divider" />
            <div className={`preference-row browser-checks ${browserVisible ? '' : 'dimmed'}`}>{([['cat', 'Genre'], ['art', 'Artist'], ['alb', 'Album']] as const).map(([pane, label]) => <label className="preference-choice" key={pane}><input type="checkbox" checked={browserPanes[pane]} onChange={() => setBrowserPanes({ ...browserPanes, [pane]: !browserPanes[pane] })} /><span>{label}</span></label>)}</div>
          </div>
          </section>
        </>}
        {tab === 'library' && <>
          <section className="preference-group"><h3>Spotify account</h3><div className="preference-inset client-id-field">
            <label><span>Client ID:</span><input value={clientId} onChange={(event) => setClientId(event.target.value)} placeholder="From developer.spotify.com" /></label>
            <small>From your Spotify Developer dashboard. Stored on this Mac only.</small>
          </div></section>
          <section className="preference-group"><h3>Syncing</h3><div className="preference-inset preference-options">
            <label className="preference-choice"><input type="checkbox" checked={autoAdd} onChange={(event) => setAutoAdd(event.target.checked)} /><span><strong>Automatically add my entire Spotify library</strong><small>Everything you save on Spotify appears here automatically.</small></span></label>
            <label className="preference-choice"><input type="checkbox" checked={autoConnect} onChange={(event) => setAutoConnect(event.target.checked)} /><span><strong>Connect to Spotify automatically at launch</strong><small>Keep pulling in music you add on Spotify each time Retune starts.</small></span></label>
          </div></section>
        </>}
        {tab === 'audio' && <>
          <section className="preference-group"><h3>Streaming quality</h3><div className="preference-inset preference-row quality-options">
            {streamingQualities.map(([label, bitrate]) => <label className="preference-choice" key={label}><input type="radio" name="streaming-quality" checked={streamingBitrate === bitrate} onChange={() => setStreamingBitrate(bitrate)} /><span><strong>{label}</strong><small>{bitrate} kbps</small></span></label>)}
          </div></section>
          <section className="preference-group"><h3>Playback</h3><div className="preference-inset preference-options">
            <label className="preference-choice"><input type="radio" name="playback-backend" checked={playbackBackend === 'local'} onChange={() => setPlaybackBackend('local')} /><span><strong>Built-in (librespot)</strong><small>Play directly inside Retune — no Spotify app window needed.</small></span></label>
            <label className="preference-choice"><input type="radio" name="playback-backend" checked={playbackBackend === 'connect'} onChange={() => setPlaybackBackend('connect')} /><span><strong>Spotify app (Connect)</strong><small>Route playback through the running Spotify desktop app.</small></span></label>
            <div className="preference-divider" />
            <label className="preference-choice"><input type="checkbox" checked={normalizeVolume} onChange={(event) => setNormalizeVolume(event.target.checked)} /><span>Normalize volume across tracks</span></label>
            <label className="preference-choice"><input type="checkbox" checked={gapless} onChange={(event) => setGapless(event.target.checked)} /><span>Gapless album playback</span></label>
          </div></section>
          <section className="preference-group"><h3>Count as played after</h3><div className="preference-inset preference-row threshold-options">
            {playThresholds.map((percent) => <label className="preference-choice" key={percent}><input type="radio" name="play-threshold" checked={playThresholdPercent === percent} onChange={() => setPlayThresholdPercent(percent)} /><span>{percent === 100 ? 'When finished' : `${percent}%`}</span></label>)}
          </div></section>
        </>}
      </div>
      <div className="modal-actions"><button onClick={onCancel}>Cancel</button><button className="primary" onClick={() => onSave({ theme, browserVisible, browserPanes, autoAddSpotifyLibrary: autoAdd, autoConnect, spotifyClientId: clientId.trim(), playbackBackend, streamingBitrate, normalizeVolume, gapless, playThresholdPercent })}>OK</button></div>
    </div>
  </div>
}
