import { getCurrentWindow } from '@tauri-apps/api/window'
import { useEffect, useRef, useState } from 'react'
import { appGateway } from './appGateway.ts'
import { initialState } from './appState.ts'
import { Preferences } from './dialogViews.tsx'
import { lastfmGateway } from './lastfmGateway.ts'
import type { Settings, SettingsPatch } from './types.ts'
import { useTauriInvalidationSnapshot } from './useTauriInvalidationSnapshot.ts'
import { ErrorNotice } from './viewShared.tsx'

export default function PreferencesWindow() {
  const [settings, setSettings] = useState<Settings>()
  const [lastfm, setLastfm] = useState(initialState.lastfm)
  const [lastfmImport, setLastfmImport] = useState(initialState.lastfmImport)
  const [error, setError] = useState<string>()
  const [saving, setSaving] = useState(false)
  const savePending = useRef(false)
  const loadError = () => setError('Couldn’t load preferences. Close this window and try again.')
  useTauriInvalidationSnapshot('settings-changed', appGateway.settings, setSettings, loadError)
  useTauriInvalidationSnapshot('lastfm-changed', lastfmGateway.accountState, setLastfm, loadError)
  useTauriInvalidationSnapshot('lastfm-import-changed', lastfmGateway.state, setLastfmImport, loadError)
  const theme = settings?.theme ?? 'system'
  useEffect(() => {
    const media = window.matchMedia('(prefers-color-scheme: dark)')
    const apply = () => { document.documentElement.dataset.theme = theme === 'system' ? media.matches ? 'dark' : 'light' : theme }
    apply()
    media.addEventListener('change', apply)
    return () => media.removeEventListener('change', apply)
  }, [theme])
  const close = () => {
    if (savePending.current) return
    void getCurrentWindow().close().catch(() => setError('Couldn’t close Preferences. Try again.'))
  }
  const save = async (patch: SettingsPatch) => {
    if (savePending.current) return
    savePending.current = true
    setSaving(true)
    setError(undefined)
    try {
      await appGateway.updateSettings({ ...patch, zoom: settings!.zoom })
      await getCurrentWindow().close()
    } catch {
      setError('Couldn’t finish saving preferences. Check your settings and try again.')
    } finally {
      savePending.current = false
      setSaving(false)
    }
  }
  return <main className="preferences-window" style={{ zoom: (settings?.zoom ?? 1) * 1.15 }}>
    {error && <ErrorNotice onDismiss={() => setError(undefined)}>{error}</ErrorNotice>}
    {settings ? <Preferences settings={settings} lastfm={lastfm} lastfmImport={lastfmImport} saving={saving}
      onZoom={(zoom) => setSettings({ ...settings, zoom })} onCancel={close} onLastfm={setLastfm}
      onImport={() => { void appGateway.openLastFmImporter().catch(() => setError('Couldn’t open the Last.fm importer. Try again.')) }}
      onSyncLastfm={() => { void lastfmGateway.syncPlays().then(setLastfmImport).catch(() => setError('Couldn’t sync Last.fm. Try again.')) }}
      onSave={save} /> : <p role="status">Loading preferences…</p>}
  </main>
}
