import type { Device, LayoutState, Screen, ScreenAdjacency, ScreenEdge } from './types'

const EDGE_TOLERANCE = 80
const SNAP_TOLERANCE = 80

export interface FlattenedScreen extends Screen {
  deviceName: string
  deviceColor: string
  platform: Device['platform']
  online: boolean
  inputReady: boolean
  upgrading?: boolean
  role: Device['role']
}

export interface LayoutBounds {
  minX: number
  minY: number
  maxX: number
  maxY: number
  width: number
  height: number
}

export interface ScreenPosition {
  x: number
  y: number
}

export function screenPositionOverlaps(
  layout: LayoutState,
  screenId: string,
  nextPosition: ScreenPosition,
): boolean {
  const movingScreen = getScreenById(layout, screenId)
  if (!movingScreen) {
    return false
  }

  const nextScreen = { ...movingScreen, ...nextPosition }
  return flattenScreens(layout).some(
    (screen) => screen.id !== screenId && screensOverlap(nextScreen, screen),
  )
}

export function snapOverlappingScreenPosition(
  layout: LayoutState,
  screenId: string,
  nextPosition: ScreenPosition,
  startPosition: ScreenPosition,
): ScreenPosition | null {
  const movingScreen = getScreenById(layout, screenId)
  if (!movingScreen) {
    return null
  }

  const nextScreen = { ...movingScreen, ...nextPosition }
  let target: Screen | null = null
  let targetOverlapArea = 0
  for (const screen of flattenScreens(layout)) {
    if (screen.id === screenId) {
      continue
    }

    const area = screenOverlapArea(nextScreen, screen)
    if (area > targetOverlapArea) {
      target = screen
      targetOverlapArea = area
    }
  }

  if (!target) {
    return null
  }

  const dx = nextPosition.x - startPosition.x
  const dy = nextPosition.y - startPosition.y
  const horizontal = Math.abs(dx) >= Math.abs(dy)
  const movingCenterX = nextScreen.x + nextScreen.width / 2
  const movingCenterY = nextScreen.y + nextScreen.height / 2
  const targetCenterX = target.x + target.width / 2
  const targetCenterY = target.y + target.height / 2
  const snapped = horizontal
    ? {
        x:
          movingCenterX < targetCenterX || (movingCenterX === targetCenterX && dx < 0)
            ? target.x - movingScreen.width
            : target.x + target.width,
        y: nextPosition.y,
      }
    : {
        x: nextPosition.x,
        y:
          movingCenterY < targetCenterY || (movingCenterY === targetCenterY && dy < 0)
            ? target.y - movingScreen.height
            : target.y + target.height,
      }

  return screenPositionOverlaps(layout, screenId, snapped) ? null : snapped
}

export function flattenScreens(layout: LayoutState): FlattenedScreen[] {
  return layout.devices.flatMap((device) =>
    device.screens.map((screen) => ({
      ...screen,
      deviceName: device.name,
      deviceColor: device.color,
      platform: device.platform,
      online: device.online,
      inputReady: device.inputReady,
      upgrading: device.upgrading,
      role: device.role,
    })),
  )
}

export function getLayoutBounds(screens: Screen[]): LayoutBounds {
  const minX = Math.min(...screens.map((screen) => screen.x))
  const minY = Math.min(...screens.map((screen) => screen.y))
  const maxX = Math.max(...screens.map((screen) => screen.x + screen.width))
  const maxY = Math.max(...screens.map((screen) => screen.y + screen.height))

  return {
    minX,
    minY,
    maxX,
    maxY,
    width: Math.max(maxX - minX, 1),
    height: Math.max(maxY - minY, 1),
  }
}

function rangesOverlap(aStart: number, aEnd: number, bStart: number, bEnd: number) {
  return Math.min(aEnd, bEnd) - Math.max(aStart, bStart) > EDGE_TOLERANCE
}

function near(valueA: number, valueB: number) {
  return Math.abs(valueA - valueB) <= EDGE_TOLERANCE
}

function screensOverlap(a: Screen, b: Screen) {
  return (
    a.x < b.x + b.width &&
    a.x + a.width > b.x &&
    a.y < b.y + b.height &&
    a.y + a.height > b.y
  )
}

function screenOverlapArea(a: Screen, b: Screen) {
  const width = Math.min(a.x + a.width, b.x + b.width) - Math.max(a.x, b.x)
  const height = Math.min(a.y + a.height, b.y + b.height) - Math.max(a.y, b.y)
  return width > 0 && height > 0 ? width * height : 0
}

export function buildAdjacency(screens: Screen[]): ScreenAdjacency[] {
  const edges: ScreenAdjacency[] = []

  for (const from of screens) {
    for (const to of screens) {
      if (from.id === to.id) {
        continue
      }

      if (
        near(from.x + from.width, to.x) &&
        rangesOverlap(from.y, from.y + from.height, to.y, to.y + to.height)
      ) {
        edges.push(createAdjacency(from.id, to.id, 'right', 'left'))
      }

      if (
        near(from.x, to.x + to.width) &&
        rangesOverlap(from.y, from.y + from.height, to.y, to.y + to.height)
      ) {
        edges.push(createAdjacency(from.id, to.id, 'left', 'right'))
      }

      if (
        near(from.y + from.height, to.y) &&
        rangesOverlap(from.x, from.x + from.width, to.x, to.x + to.width)
      ) {
        edges.push(createAdjacency(from.id, to.id, 'bottom', 'top'))
      }

      if (
        near(from.y, to.y + to.height) &&
        rangesOverlap(from.x, from.x + from.width, to.x, to.x + to.width)
      ) {
        edges.push(createAdjacency(from.id, to.id, 'top', 'bottom'))
      }
    }
  }

  return edges
}

function createAdjacency(
  fromScreenId: string,
  toScreenId: string,
  fromEdge: ScreenEdge,
  toEdge: ScreenEdge,
): ScreenAdjacency {
  return {
    fromScreenId,
    toScreenId,
    fromEdge,
    toEdge,
  }
}

export function moveScreen(
  layout: LayoutState,
  screenId: string,
  nextPosition: ScreenPosition,
): LayoutState {
  return {
    ...layout,
    devices: layout.devices.map((device) => ({
      ...device,
      screens: device.screens.map((screen) =>
        screen.id === screenId ? { ...screen, ...nextPosition } : screen,
      ),
    })),
  }
}

export function snapScreenPosition(
  layout: LayoutState,
  screenId: string,
  nextPosition: ScreenPosition,
): ScreenPosition {
  const movingScreen = getScreenById(layout, screenId)
  if (!movingScreen) {
    return nextPosition
  }

  const screens = flattenScreens(layout).filter((screen) => screen.id !== screenId)

  return {
    x: snapAxis(
      nextPosition.x,
      screens.flatMap((screen) => [
        screen.x,
        screen.x + screen.width,
        screen.x - movingScreen.width,
        screen.x + screen.width - movingScreen.width,
      ]),
    ),
    y: snapAxis(
      nextPosition.y,
      screens.flatMap((screen) => [
        screen.y,
        screen.y + screen.height,
        screen.y - movingScreen.height,
        screen.y + screen.height - movingScreen.height,
      ]),
    ),
  }
}

function snapAxis(value: number, candidates: number[]) {
  let closest = value
  let closestDistance = SNAP_TOLERANCE + 1

  for (const candidate of candidates) {
    const distance = Math.abs(value - candidate)
    if (distance < closestDistance) {
      closest = candidate
      closestDistance = distance
    }
  }

  return closestDistance <= SNAP_TOLERANCE ? closest : value
}

export function getScreenById(layout: LayoutState, screenId: string) {
  return flattenScreens(layout).find((screen) => screen.id === screenId)
}
