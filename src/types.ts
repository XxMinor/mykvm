export type Platform = 'windows' | 'macos' | 'unknown'

export type MachineRole = 'unset' | 'server' | 'client'

export type AppLanguage = 'cn' | 'en'

export type ThemeMode = 'system' | 'dark' | 'light'

export type TransportPortMode = 'auto' | 'fixed'

export type ModifierTarget = 'control' | 'alt' | 'meta' | 'same'

export interface ModifierMap {
  control: ModifierTarget
  alt: ModifierTarget
  meta: ModifierTarget
}

export interface ScreenSwitchHotkeys {
  left: string
  right: string
  up: string
  down: string
}

export interface PairedController {
  id: string
  name: string
  host: string
  ip: string
  transportPublicKey: string
  protocolVersion: number
  clusterId: string
  pairedAtMs: number
}

export interface Screen {
  id: string
  deviceId: string
  name: string
  x: number
  y: number
  width: number
  height: number
  scale: number
  isPrimary: boolean
}

export interface Device {
  id: string
  name: string
  platform: Platform
  host: string
  transportPort: number
  quicPort: number
  transportPublicKey: string
  protocolVersion: number
  color: string
  online: boolean
  inputReady: boolean
  upgrading?: boolean
  /** Multipliers this controller applies when driving the device. */
  pointerSpeed?: number
  scrollSpeed?: number
  role: 'local' | 'server' | 'client'
  source?: 'detected' | 'manual'
  screens: Screen[]
}

export interface LayoutState {
  devices: Device[]
  activeDeviceId: string
  selectedScreenId: string
  inputMode: 'control' | 'receive'
  machineRole: MachineRole
  clusterId: string
  pairSecret: string
  pairedControllers: PairedController[]
  clipboardSync: boolean
  clipboardOnDemand?: boolean
  clipboardPasteHotkey?: string
  fileTransferEnabled: boolean
  /** Controller: locking this machine locks its online clients too. */
  lockSync?: boolean
  inputProtection?: InputProtection
  language: AppLanguage
  themeMode: ThemeMode
  performanceMonitor: boolean
  transportPortMode: TransportPortMode
  transportPort: number
  quicPort: number
  modifierRemap: boolean
  modifierMap: ModifierMap
  edgeSwitchHotkey: string
  screenSwitchHotkeys: ScreenSwitchHotkeys
}

export interface InputProtection {
  localOnly: boolean
  protectFullscreen: boolean
  blockedApplications: string[]
  lockHotkey: string
}
