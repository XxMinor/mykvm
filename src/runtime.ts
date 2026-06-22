import type { LayoutState } from './types'

export interface NativeStageStatus {
  state: 'stubbed' | 'idle' | 'ready' | 'error'
  detail: string
}

export interface LanPeer {
  id: string
  name: string
  platform: string
  machineRole: string
  clusterId: string
  pairingRequired: boolean
  host: string
  ip: string
  transportPort: number
  quicPort: number
  transportPublicKey: string
  protocolVersion: number
  screenCount: number
  inputReady: boolean
  upgrading?: boolean
  screens: LanPeerScreen[]
  appVersion: string
  lastSeenMs: number
}

export interface LanPeerScreen {
  id: string
  name: string
  x: number
  y: number
  width: number
  height: number
  scale: number
  isPrimary: boolean
}

export interface DiscoveryStatus {
  state: 'idle' | 'ready' | 'error'
  detail: string
  port: number
  localPeer: LanPeer
  peers: LanPeer[]
}

export interface PairingStatus {
  state: 'idle' | 'available' | 'requested' | 'paired'
  code: string
  requesterName: string
  requesterIp: string
  expiresAtMs: number
  detail: string
}

export interface RuntimeStatus {
  started: boolean
  transport: NativeStageStatus
  capture: NativeStageStatus
  inject: NativeStageStatus
  clipboard: NativeStageStatus
  discovery: DiscoveryStatus
  pairing: PairingStatus
  privilege: PrivilegeStatus
  inputService: InputServiceStatus
}

export interface AppStateSnapshot {
  layout: LayoutState
  runtime: RuntimeStatus
}

export interface PrivilegeStatus {
  isElevated: boolean
  canElevate: boolean
  detail: string
}

export interface InputServiceStatus {
  installed: boolean
  running: boolean
  workerSessionId?: number | null
  pipeAvailable: boolean
  sasAvailable: boolean
  detail: string
}

export interface PerformanceSample {
  timestampMs: number
  appCpuPercent: number
  appMemoryMb: number
  transportPackets: number
  inputEvents: number
  clipboardPackets: number
}
