import { invoke, isTauri } from '@tauri-apps/api/core'
import { RELEASES_URL, REPOSITORY_URL } from './constants'
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

const FALLBACK_RUNTIME: RuntimeStatus = {
  started: false,
  transport: {
    state: 'stubbed',
    detail: 'Desktop fallback mode does not start discovery, input capture, or injection.',
  },
  capture: {
    state: 'stubbed',
    detail: 'Global input capture will be implemented in the Rust layer.',
  },
  inject: {
    state: 'stubbed',
    detail: 'System input injection will be implemented in the Rust layer.',
  },
  clipboard: {
    state: 'stubbed',
    detail: '剪贴板同步需要在 Tauri 桌面端运行。',
  },
  privilege: {
    isElevated: false,
    canElevate: false,
    detail: 'Administrator restart is available only in the Windows desktop runtime.',
  },
  inputService: {
    installed: false,
    running: false,
    workerSessionId: null,
    pipeAvailable: false,
    sasAvailable: false,
    detail: 'Windows lock screen input service is available only in the Windows desktop runtime.',
  },
  discovery: {
    state: 'idle',
    detail: 'LAN discovery is available only in the Tauri desktop runtime.',
    port: 47833,
    localPeer: {
      id: 'browser-preview',
      name: 'Desktop fallback',
      platform: navigator.platform,
      machineRole: defaultLayout.machineRole,
      clusterId: defaultLayout.clusterId,
      pairingRequired: false,
      host: window.location.hostname || 'localhost',
      ip: '127.0.0.1',
      transportPort: defaultLayout.transportPort,
      quicPort: defaultLayout.quicPort,
      transportPublicKey: '',
      protocolVersion: 1,
      screenCount: 1,
      inputReady: false,
      screens: [
        {
          id: 'browser-display-1',
          name: 'Browser display',
          x: defaultLayout.devices[0].screens[0].x,
          y: defaultLayout.devices[0].screens[0].y,
          width: defaultLayout.devices[0].screens[0].width,
          height: defaultLayout.devices[0].screens[0].height,
          scale: defaultLayout.devices[0].screens[0].scale,
          isPrimary: true,
        },
      ],
      appVersion: '0.1.0',
      lastSeenMs: Date.now(),
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

let browserRuntime = FALLBACK_RUNTIME

export async function loadAppState(): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    return {
      layout: defaultLayout,
      runtime: browserRuntime,
    }
  }

  return invoke<AppStateSnapshot>('load_app_state')
}

export async function saveLayout(layout: LayoutState): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    return {
      layout,
      runtime: browserRuntime,
    }
  }

  return invoke<AppStateSnapshot>('save_layout', { layout })
}

export async function resetPairing(): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    return {
      layout: { ...defaultLayout, pairedControllers: [] },
      runtime: browserRuntime,
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
    browserRuntime = {
      started: true,
      transport: {
        state: 'ready',
        detail: 'Desktop fallback does not start native discovery, input capture, or injection.',
      },
      capture: FALLBACK_RUNTIME.capture,
      inject: FALLBACK_RUNTIME.inject,
      clipboard: FALLBACK_RUNTIME.clipboard,
      privilege: FALLBACK_RUNTIME.privilege,
      inputService: FALLBACK_RUNTIME.inputService,
      pairing: FALLBACK_RUNTIME.pairing,
      discovery: {
        ...FALLBACK_RUNTIME.discovery,
        detail: 'Desktop fallback cannot scan the LAN. Start the Tauri desktop app to use UDP discovery.',
        localPeer: {
          ...FALLBACK_RUNTIME.discovery.localPeer,
          lastSeenMs: Date.now(),
        },
      },
    }

    return browserRuntime
  }

  return invoke<RuntimeStatus>('start_runtime')
}

export async function readRuntimeStatus(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    browserRuntime = {
      ...browserRuntime,
      discovery: {
        ...browserRuntime.discovery,
        localPeer: {
          ...browserRuntime.discovery.localPeer,
          lastSeenMs: Date.now(),
        },
      },
    }

    return browserRuntime
  }

  return invoke<RuntimeStatus>('read_runtime_status')
}

export async function readDiagnosticInfo(): Promise<DiagnosticInfo> {
  if (!isTauri()) {
    return {
      report: 'Desktop diagnostics are available only in the Tauri desktop runtime.',
      appVersion: '0.1.0',
      platform: navigator.platform,
      role: defaultLayout.machineRole,
      runtimeStarted: browserRuntime.started,
      localName: browserRuntime.discovery.localPeer.name,
      localIp: browserRuntime.discovery.localPeer.ip,
      discoveryPort: browserRuntime.discovery.port,
      quicPort: browserRuntime.discovery.localPeer.quicPort,
      peerCount: browserRuntime.discovery.peers.length,
      knownDevices: [],
      logDir: '',
      configDir: '',
      networkHint: 'Desktop diagnostics are available only in the Tauri desktop runtime.',
      firewallHint: 'Desktop diagnostics are available only in the Tauri desktop runtime.',
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
    browserRuntime = FALLBACK_RUNTIME
    return browserRuntime
  }

  return invoke<RuntimeStatus>('stop_runtime')
}

export async function scanLanPeers(): Promise<DiscoveryStatus> {
  if (!isTauri()) {
    return browserRuntime.discovery
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
    return browserRuntime
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
    const memory = (performance as Performance & {
      memory?: {
        usedJSHeapSize: number
        jsHeapSizeLimit: number
      }
    }).memory
    const usedMb = memory ? memory.usedJSHeapSize / 1024 / 1024 : 96 + Math.sin(Date.now() / 2000) * 12

    return {
      timestampMs: Date.now(),
      appCpuPercent: Math.max(2, Math.min(100, 18 + Math.sin(Date.now() / 1500) * 10)),
      appMemoryMb: usedMb,
      transportPackets: Math.round(Date.now() / 1000) % 700,
      inputEvents: Math.round(Date.now() / 80) % 1200,
      clipboardPackets: Math.round(Date.now() / 5000) % 80,
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
    return browserRuntime.inputService
  }

  return invoke<InputServiceStatus>('read_input_service_status')
}

export async function installInputService(): Promise<InputServiceStatus> {
  if (!isTauri()) {
    return browserRuntime.inputService
  }

  return invoke<InputServiceStatus>('install_input_service')
}

export async function uninstallInputService(): Promise<InputServiceStatus> {
  if (!isTauri()) {
    return browserRuntime.inputService
  }

  return invoke<InputServiceStatus>('uninstall_input_service')
}

export async function sendSecureAttention(deviceId: string): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('send_secure_attention', { deviceId })
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
