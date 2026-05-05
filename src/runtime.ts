import type { LayoutState } from './types'

export interface NativeStageStatus {
  state: 'stubbed' | 'idle' | 'ready' | 'error'
  detail: string
}

export interface LanPeer {
  id: string
  name: string
  platform: string
  host: string
  ip: string
  transportPort: number
  quicPort: number
  transportPublicKey: string
  protocolVersion: number
  screenCount: number
  inputReady: boolean
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

export interface RuntimeStatus {
  started: boolean
  transport: NativeStageStatus
  capture: NativeStageStatus
  inject: NativeStageStatus
  clipboard: NativeStageStatus
  discovery: DiscoveryStatus
  privilege: PrivilegeStatus
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

export interface PerformanceSample {
  timestampMs: number
  appCpuPercent: number
  appMemoryMb: number
  transportPackets: number
  inputEvents: number
  clipboardPackets: number
}
