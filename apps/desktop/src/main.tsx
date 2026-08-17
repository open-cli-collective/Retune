import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { getCurrentWindow } from '@tauri-apps/api/window'
import './index.css'
import App from './App.tsx'
import LastFmImporter from './LastFmImporter.tsx'

const isImporter = getCurrentWindow().label === 'lastfm-importer'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    {isImporter ? <LastFmImporter /> : <App />}
  </StrictMode>,
)
