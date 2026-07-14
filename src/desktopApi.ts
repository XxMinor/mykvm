import { invoke, isTauri } from '@tauri-apps/api/core'
import { APP_VERSION, RELEASES_URL, REPOSITORY_URL } from './constants'
import { defaultLayout } from './defaultLayout'
import type {
  AppStateSnapshot,
  DiagnosticInfo,
  DiscoveryStatus,
  InputServiceStatus,
  PerformanceSample,
  RuntimeStatus,
} from './runtime'
import type { LayoutState } from './types'

export interface AppUpdateInfo {
  version: string
  currentVersion?: string
  date?: string
  body?: string
}

export interface AppUpdateCheckResult {
  available: boolean
  update?: AppUpdateInfo
}

export interface FileTransferSummary {
  targetName: string
  fileCount: number
  byteCount: number
}

// ponytail: static stopped-state stub so `npm run dev` in a plain browser still
// renders the layout editor; the real runtime lives in the Tauri backend.
const STUB_DETAIL = 'Available only in the Tauri desktop runtime.'

const BROWSER_RUNTIME: RuntimeStatus = {
  started: false,
  transport: { state: 'stubbed', detail: STUB_DETAIL },
  capture: { state: 'stubbed', detail: STUB_DETAIL },
  inject: { state: 'stubbed', detail: STUB_DETAIL },
  clipboard: { state: 'stubbed', detail: STUB_DETAIL },
  privilege: { isElevated: false, canElevate: false, detail: STUB_DETAIL },
  inputService: {
    installed: false,
    running: false,
    workerSessionId: null,
    pipeAvailable: false,
    sasAvailable: false,
    detail: STUB_DETAIL,
  },
  discovery: {
    state: 'idle',
    detail: STUB_DETAIL,
    port: defaultLayout.transportPort,
    localPeer: {
      id: 'browser-preview',
      name: 'Browser preview',
      platform: navigator.platform,
      machineRole: defaultLayout.machineRole,
      clusterId: defaultLayout.clusterId,
      pairingRequired: false,
      host: 'localhost',
      ip: '127.0.0.1',
      transportPort: defaultLayout.transportPort,
      quicPort: defaultLayout.quicPort,
      transportPublicKey: '',
      protocolVersion: 1,
      screenCount: 0,
      inputReady: false,
      screens: [],
      appVersion: APP_VERSION,
      lastSeenMs: 0,
    },
    peers: [],
  },
  pairing: {
    state: 'idle',
    code: '',
    requesterName: '',
    requesterIp: '',
    expiresAtMs: 0,
    detail: '',
  },
}

export async function loadAppState(): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    return {
      layout: defaultLayout,
      runtime: BROWSER_RUNTIME,
    }
  }

  return invoke<AppStateSnapshot>('load_app_state')
}

export async function saveLayout(layout: LayoutState): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    return {
      layout,
      runtime: BROWSER_RUNTIME,
    }
  }

  return invoke<AppStateSnapshot>('save_layout', { layout })
}

export async function resetPairing(): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    return {
      layout: { ...defaultLayout, pairedControllers: [] },
      runtime: BROWSER_RUNTIME,
    }
  }

  return invoke<AppStateSnapshot>('reset_pairing')
}

export async function isAutostartEnabled(): Promise<boolean> {
  if (!isTauri()) {
    return false
  }

  return invoke<boolean>('is_autostart_enabled')
}

export async function setAutostart(enabled: boolean): Promise<boolean> {
  if (!isTauri()) {
    return enabled
  }

  return invoke<boolean>('set_autostart', { enabled })
}

export async function startRuntime(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME
  }

  return invoke<RuntimeStatus>('start_runtime')
}

export async function readRuntimeStatus(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME
  }

  return invoke<RuntimeStatus>('read_runtime_status')
}

export async function readDiagnosticInfo(): Promise<DiagnosticInfo> {
  if (!isTauri()) {
    return {
      report: STUB_DETAIL,
      appVersion: APP_VERSION,
      platform: navigator.platform,
      role: defaultLayout.machineRole,
      runtimeStarted: false,
      localName: BROWSER_RUNTIME.discovery.localPeer.name,
      localIp: BROWSER_RUNTIME.discovery.localPeer.ip,
      discoveryPort: BROWSER_RUNTIME.discovery.port,
      quicPort: BROWSER_RUNTIME.discovery.localPeer.quicPort,
      peerCount: 0,
      logDir: '',
      configDir: '',
      networkHint: STUB_DETAIL,
      firewallHint: STUB_DETAIL,
    }
  }

  return invoke<DiagnosticInfo>('read_diagnostic_info')
}

export async function openLogDirectory(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('open_log_directory')
}

export async function stopRuntime(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME
  }

  return invoke<RuntimeStatus>('stop_runtime')
}

export async function scanLanPeers(): Promise<DiscoveryStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME.discovery
  }

  return invoke<DiscoveryStatus>('scan_lan_peers')
}

export async function probeLanPeer(host: string) {
  if (!isTauri()) {
    throw new Error('Direct peer probing is available only in the Tauri desktop runtime.')
  }

  return invoke<DiscoveryStatus['localPeer']>('probe_lan_peer', { host })
}

export async function requestLanPairing(host: string) {
  if (!isTauri()) {
    throw new Error('LAN pairing is available only in the Tauri desktop runtime.')
  }

  return invoke<DiscoveryStatus['localPeer']>('request_lan_pairing', { host })
}

export async function confirmLanPairing(host: string, code: string) {
  if (!isTauri()) {
    throw new Error('LAN pairing is available only in the Tauri desktop runtime.')
  }

  return invoke<DiscoveryStatus['localPeer']>('confirm_lan_pairing', {
    host,
    code,
  })
}

export async function dismissPairingRequest(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME
  }

  return invoke<RuntimeStatus>('dismiss_pairing_request')
}

export async function writeClipboardText(text: string): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('write_clipboard_text', { text })
}

export async function readPerformanceSample(): Promise<PerformanceSample> {
  if (!isTauri()) {
    return {
      timestampMs: Date.now(),
      appCpuPercent: 0,
      appMemoryMb: 0,
      transportPackets: 0,
      inputEvents: 0,
      clipboardPackets: 0,
    }
  }

  return invoke<PerformanceSample>('read_performance_sample')
}

export async function restartAsAdmin(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('restart_as_admin')
}

export async function readInputServiceStatus(): Promise<InputServiceStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME.inputService
  }

  return invoke<InputServiceStatus>('read_input_service_status')
}

export async function installInputService(): Promise<InputServiceStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME.inputService
  }

  return invoke<InputServiceStatus>('install_input_service')
}

export async function uninstallInputService(): Promise<InputServiceStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME.inputService
  }

  return invoke<InputServiceStatus>('uninstall_input_service')
}

export async function sendFilesToDevice(deviceId: string, paths: string[]): Promise<FileTransferSummary> {
  if (!isTauri()) {
    return {
      targetName: 'Desktop fallback',
      fileCount: paths.length,
      byteCount: 0,
    }
  }

  return invoke<FileTransferSummary>('send_files_to_device', { deviceId, paths })
}

export async function relaunchApp(): Promise<void> {
  if (!isTauri()) {
    window.location.reload()
    return
  }

  const { relaunch } = await import('@tauri-apps/plugin-process')
  await relaunch()
}

export async function syncWindowChrome(theme: 'dark' | 'light'): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('sync_window_chrome', { theme })
}

export async function minimizeMainWindow(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('minimize_main_window')
}

export async function hideMainWindow(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('hide_main_window')
}

export async function startWindowDrag(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('start_window_drag')
}

export async function toggleMaximizeMainWindow(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('toggle_maximize_main_window')
}

export async function openRepositoryUrl(): Promise<void> {
  if (!isTauri()) {
    window.open(REPOSITORY_URL, '_blank', 'noopener,noreferrer')
    return
  }

  await invoke('open_repository_url')
}

export async function openUpdateReleasePage(): Promise<void> {
  if (!isTauri()) {
    window.open(RELEASES_URL, '_blank', 'noopener,noreferrer')
    return
  }

  await invoke('open_releases_url')
}

export async function isPortableMode(): Promise<boolean> {
  if (!isTauri()) {
    return false
  }

  return invoke<boolean>('is_portable_mode')
}

export async function checkForAppUpdate(): Promise<AppUpdateCheckResult> {
  if (!isTauri()) {
    return { available: false }
  }

  const { check } = await import('@tauri-apps/plugin-updater')
  const update = await check()

  if (!update) {
    return { available: false }
  }

  return {
    available: true,
    update: {
      version: update.version,
      currentVersion: update.currentVersion,
      date: update.date,
      body: update.body,
    },
  }
}

export async function setAppUpgrading(enabled: boolean): Promise<void> {
  if (!isTauri()) return
  await invoke('set_app_upgrading', { enabled })
}

export async function installAppUpdate(): Promise<void> {
  if (!isTauri()) {
    return
  }

  const [{ check }, { relaunch }] = await Promise.all([
    import('@tauri-apps/plugin-updater'),
    import('@tauri-apps/plugin-process'),
  ])
  const update = await check()

  if (!update) {
    return
  }

  await setAppUpgrading(true).catch(() => {})
  try {
    await update.downloadAndInstall()
  } catch (error) {
    await setAppUpgrading(false).catch(() => {})
    throw error
  }
  await relaunch()
}
