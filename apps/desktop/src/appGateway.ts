import type { DiagnosticReport } from './diagnostics.ts'
import { tauriInvoker, type Invoker } from './ipc.ts'
import type { Appearance, Settings, SettingsPatch } from './types.ts'

export function createAppGateway(invoke: Invoker) {
  return {
    settings: () => invoke<Settings>('get_settings'),
    updateSettings: (patch: SettingsPatch) => invoke<void>('update_settings', { patch }),
    appearance: () => invoke<Appearance>('get_appearance'),
    openLastFmImporter: () => invoke<void>('open_lastfm_importer'),
    diagnostics: () => invoke<DiagnosticReport>('load_diagnostics'),
    emailDiagnostics: (body: string) => invoke<void>('email_diagnostics', { body }),
  }
}

export const appGateway = createAppGateway(tauriInvoker)
