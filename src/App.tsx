import {
  type CSSProperties,
  useEffect,
  useEffectEvent,
  useMemo,
  useRef,
  useState,
} from "react";
import { isTauri } from "@tauri-apps/api/core";
import type { Theme } from "@tauri-apps/api/window";
import "./App.css";
import {
  checkForAppUpdate,
  hideMainWindow,
  installAppUpdate,
  isPortableMode,
  loadAppState,
  minimizeMainWindow,
  openRepositoryUrl,
  openUpdateReleasePage,
  probeLanPeer,
  readClipboardText,
  readPerformanceSample,
  readRuntimeStatus,
  restartAsAdmin,
  saveLayout,
  scanLanPeers,
  startRuntime,
  startWindowDrag,
  stopRuntime,
  syncWindowChrome,
  toggleMaximizeMainWindow,
  writeClipboardText,
} from "./desktopApi";
import type { AppUpdateInfo } from "./desktopApi";
import { APP_VERSION, REPOSITORY_URL } from "./constants";
import { TEXT } from "./i18n";
import type { AppText } from "./i18n";
import {
  flattenScreens,
  getLayoutBounds,
  getScreenById,
  moveScreen,
  snapScreenPosition,
} from "./layout";
import type { FlattenedScreen, LayoutBounds } from "./layout";
import type {
  AppStateSnapshot,
  LanPeer,
  LanPeerScreen,
  PerformanceSample,
} from "./runtime";
import type {
  AppLanguage,
  Device,
  LayoutState,
  MachineRole,
  Platform,
  Screen,
  ThemeMode,
  TransportPortMode,
} from "./types";

const DEFAULT_BOARD_WIDTH = 1040;
const DEFAULT_BOARD_HEIGHT = 640;
const BOARD_FILL_RATIO = 0.74;
const BOARD_ZOOM_MIN = 0.35;
const BOARD_ZOOM_MAX = 2.4;
const BOARD_ZOOM_STEP = 0.15;
const SNAP_SIZE = 20;
const REMOTE_SCREEN_GAP = 0;
const DEVICE_COLORS = [
  "#2f7af8",
  "#0f766e",
  "#b45309",
  "#7c3aed",
  "#be123c",
  "#0891b2",
];
const PLATFORM_LABELS = {
  windows: "Windows",
  macos: "macOS",
  unknown: "Unknown",
} as const;
const WORKSPACE_TABS = [
  { id: "layout" },
  { id: "devices" },
  { id: "settings" },
] as const;

type WorkspaceTab = (typeof WORKSPACE_TABS)[number]["id"];

const CLIENT_TABS: WorkspaceTab[] = ["settings"];
const PERFORMANCE_SAMPLE_LIMIT = 32;
const UPDATE_DISMISSED_VERSION_KEY = "mykvm:update:dismissedVersion";
type UpdateStatus =
  | "idle"
  | "checking"
  | "available"
  | "current"
  | "installing"
  | "error";

interface DragState {
  pointerId: number;
  screenId: string;
  originClientX: number;
  originClientY: number;
  startX: number;
  startY: number;
  viewport?: BoardViewport;
}

interface BoardMetrics {
  scale: number;
  offsetX: number;
  offsetY: number;
}

interface BoardViewport {
  bounds: LayoutBounds;
  metrics: BoardMetrics;
}

function App() {
  const [snapshot, setSnapshot] = useState<AppStateSnapshot | null>(null);
  const [dragState, setDragState] = useState<DragState | null>(null);
  const [boardSize, setBoardSize] = useState({
    width: DEFAULT_BOARD_WIDTH,
    height: DEFAULT_BOARD_HEIGHT,
  });
  const [isSaving, setIsSaving] = useState(false);
  const [isRuntimePending, setIsRuntimePending] = useState(false);
  const [isScanningLan, setIsScanningLan] = useState(false);
  const [isAddingDevice, setIsAddingDevice] = useState(false);
  const [isAdminRestartPending, setIsAdminRestartPending] = useState(false);
  const [isClipboardPending, setIsClipboardPending] = useState(false);
  const [boardZoom, setBoardZoom] = useState(1);
  const [manualDeviceName, setManualDeviceName] = useState("");
  const [manualDeviceHost, setManualDeviceHost] = useState("");
  const [clipboardText, setClipboardText] = useState("");
  const [performanceSamples, setPerformanceSamples] = useState<
    PerformanceSample[]
  >([]);
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus>("idle");
  const [availableUpdate, setAvailableUpdate] =
    useState<AppUpdateInfo | null>(null);
  const [updateMessage, setUpdateMessage] = useState<string | null>(null);
  const [dismissedUpdateVersion, setDismissedUpdateVersion] = useState<
    string | null
  >(() => localStorage.getItem(UPDATE_DISMISSED_VERSION_KEY));
  const [isPortable, setIsPortable] = useState(false);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<WorkspaceTab>("layout");
  const [systemTheme, setSystemTheme] = useState<Exclude<ThemeMode, "system">>(
    () => getSystemTheme(),
  );
  const boardRef = useRef<HTMLDivElement | null>(null);
  const startupUpdateCheckStarted = useRef(false);

  useEffect(() => {
    let active = true;

    loadAppState()
      .then((nextSnapshot) => {
        if (active) {
          setSnapshot(nextSnapshot);
        }
        if (
          active &&
          nextSnapshot.layout.machineRole === "client" &&
          !nextSnapshot.runtime.started
        ) {
          setIsRuntimePending(true);
          startRuntime()
            .then((nextRuntime) => {
              if (!active) {
                return;
              }
              setSnapshot((current) =>
                current
                  ? {
                      ...current,
                      runtime: nextRuntime,
                    }
                  : current,
              );
            })
            .catch((error: unknown) => {
              if (active) {
                setErrorMessage(
                  error instanceof Error
                    ? error.message
                    : TEXT.cn.errors.updateRuntime,
                );
              }
            })
            .finally(() => {
              if (active) {
                setIsRuntimePending(false);
              }
            });
        }
      })
      .catch((error: unknown) => {
        if (active) {
          setErrorMessage(
            error instanceof Error ? error.message : TEXT.cn.errors.loadState,
          );
        }
      });

    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    let active = true;

    isPortableMode()
      .then((portable) => {
        if (active) {
          setIsPortable(portable);
        }
      })
      .catch(() => {
        // Portable detection is a convenience for update flow, not startup-critical.
      });

    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    const darkMedia = window.matchMedia("(prefers-color-scheme: dark)");
    const lightMedia = window.matchMedia("(prefers-color-scheme: light)");
    let active = true;
    let unlistenTheme: (() => void) | null = null;

    const syncSystemTheme = (theme?: Theme | null) => {
      if (!active) {
        return;
      }

      setSystemTheme(theme ?? getSystemTheme());
    };
    const syncMediaTheme = () => syncSystemTheme();

    darkMedia.addEventListener("change", syncMediaTheme);
    lightMedia.addEventListener("change", syncMediaTheme);
    syncSystemTheme();

    if (isTauri()) {
      void import("@tauri-apps/api/window")
        .then(async ({ getCurrentWindow }) => {
          if (!active) {
            return null;
          }

          const appWindow = getCurrentWindow();

          appWindow
            .theme()
            .then((theme) => syncSystemTheme(theme))
            .catch(() => {
              // Some platforms only report changes through media queries.
            });

          return appWindow.onThemeChanged(({ payload }) =>
            syncSystemTheme(payload),
          );
        })
        .then((unlisten) => {
          if (!unlisten) {
            return;
          }

          if (active) {
            unlistenTheme = unlisten;
            return;
          }

          unlisten();
        })
        .catch(() => {
          // Keep CSS media-query tracking as the fallback.
        });
    }

    return () => {
      active = false;
      darkMedia.removeEventListener("change", syncMediaTheme);
      lightMedia.removeEventListener("change", syncMediaTheme);
      unlistenTheme?.();
    };
  }, []);

  useEffect(() => {
    let active = true;

    const refreshRuntime = () => {
      if (document.visibilityState === "hidden") {
        return;
      }

      readRuntimeStatus()
        .then((nextRuntime) => {
          if (!active) {
            return;
          }

          setSnapshot((current) =>
            current
              ? {
                  ...current,
                  runtime: nextRuntime,
                }
              : current,
          );
        })
        .catch(() => {
          // Keep the current UI state if a transient refresh fails.
        });
    };

    const intervalId = window.setInterval(refreshRuntime, 2000);
    document.addEventListener("visibilitychange", refreshRuntime);

    return () => {
      active = false;
      window.clearInterval(intervalId);
      document.removeEventListener("visibilitychange", refreshRuntime);
    };
  }, []);

  useEffect(() => {
    const board = boardRef.current;
    if (!board) {
      return;
    }

    const updateBoardSize = () => {
      setBoardSize({
        width: Math.max(board.clientWidth, 1),
        height: Math.max(board.clientHeight, 1),
      });
    };
    const resizeObserver = new ResizeObserver(updateBoardSize);

    updateBoardSize();
    resizeObserver.observe(board);

    return () => resizeObserver.disconnect();
  }, [activeTab, snapshot?.layout.machineRole]);

  const layout = snapshot?.layout;
  const runtime = snapshot?.runtime;
  const discovery = runtime?.discovery;
  const displayLayout = useMemo(
    () => (layout ? applyPeerPresence(layout, discovery?.peers ?? []) : null),
    [layout, discovery],
  );
  const screens = useMemo(
    () => (displayLayout ? flattenScreens(displayLayout) : []),
    [displayLayout],
  );
  const bounds = useMemo(
    () => getLayoutBounds(screens.length > 0 ? screens : [fallbackScreen()]),
    [screens],
  );
  const boardMetrics = useMemo(
    () => getBoardMetrics(bounds, boardSize, boardZoom),
    [bounds, boardSize, boardZoom],
  );
  const boardViewport: BoardViewport = dragState?.viewport ?? {
    bounds,
    metrics: boardMetrics,
  };
  const machineRole = layout?.machineRole ?? "unset";
  const language = layout?.language ?? "cn";
  const themeMode = layout?.themeMode ?? "system";
  const resolvedTheme = resolveTheme(themeMode, systemTheme);
  const ui = TEXT[language];
  const hasLoadedSnapshot = Boolean(snapshot);
  const isAvailableUpdateDismissed =
    Boolean(availableUpdate) &&
    dismissedUpdateVersion === availableUpdate?.version;
  const visibleTabs = useMemo(
    () =>
      machineRole === "client"
        ? WORKSPACE_TABS.filter((tab) => CLIENT_TABS.includes(tab.id))
        : WORKSPACE_TABS,
    [machineRole],
  );
  const currentTab: WorkspaceTab =
    machineRole === "client" && !CLIENT_TABS.includes(activeTab)
      ? "settings"
      : activeTab;
  const isPerformanceActive =
    currentTab === "settings" && Boolean(layout?.performanceMonitor);
  const localPlatform =
    runtime?.discovery.localPeer.platform.toLowerCase() ??
    navigator.platform.toLowerCase();
  const usesMacChrome = localPlatform.includes("mac");
  const usesWindowsChrome = localPlatform.includes("win");
  const usesCustomChrome = usesMacChrome || usesWindowsChrome;
  const chromeClassName = usesMacChrome
    ? "custom-chrome custom-chrome-mac"
    : usesWindowsChrome
      ? "custom-chrome custom-chrome-windows"
      : "";
  const shellClassName = `app-shell ${chromeClassName} theme-${resolvedTheme}`;

  function renderWindowTitlebar() {
    if (!usesCustomChrome) {
      return null;
    }

    if (usesMacChrome) {
      return (
        <div className="app-titlebar app-titlebar-mac">
          <div className="window-controls mac-window-controls">
            <button
              type="button"
              className="window-control-button close"
              title={ui.common.close}
              aria-label={ui.common.close}
              onClick={() => void hideMainWindow()}
            />
            <button
              type="button"
              className="window-control-button minimize"
              title={ui.common.minimize}
              aria-label={ui.common.minimize}
              onClick={() => void minimizeMainWindow()}
            />
            <button
              type="button"
              className="window-control-button maximize"
              title={ui.common.maximize}
              aria-label={ui.common.maximize}
              onClick={() => void toggleMaximizeMainWindow()}
            />
          </div>
          <div
            className="titlebar-drag-region"
            data-tauri-drag-region
            aria-hidden="true"
            onPointerDown={(event) => {
              if (event.button === 0) {
                void startWindowDrag();
              }
            }}
            onDoubleClick={() => void toggleMaximizeMainWindow()}
          />
        </div>
      );
    }

    return (
      <div className="app-titlebar app-titlebar-windows">
        <div
          className="titlebar-drag-region"
          data-tauri-drag-region
          aria-hidden="true"
          onPointerDown={(event) => {
            if (event.button === 0) {
              void startWindowDrag();
            }
          }}
          onDoubleClick={() => void toggleMaximizeMainWindow()}
        />
        <div className="window-controls win-window-controls">
          <button
            type="button"
            className="window-control-button"
            title={ui.common.minimize}
            aria-label={ui.common.minimize}
            onClick={() => void minimizeMainWindow()}
          >
            <WindowMinimizeIcon />
          </button>
          <button
            type="button"
            className="window-control-button"
            title={ui.common.maximize}
            aria-label={ui.common.maximize}
            onClick={() => void toggleMaximizeMainWindow()}
          >
            <WindowMaximizeIcon />
          </button>
          <button
            type="button"
            className="window-control-button close"
            title={ui.common.close}
            aria-label={ui.common.close}
            onClick={() => void hideMainWindow()}
          >
            <WindowCloseIcon />
          </button>
        </div>
      </div>
    );
  }

  useEffect(() => {
    document.documentElement.dataset.theme = resolvedTheme;
    document.documentElement.style.colorScheme = resolvedTheme;

    if (isTauri()) {
      void import("@tauri-apps/api/window")
        .then(({ getCurrentWindow }) =>
          getCurrentWindow().setTheme(
            themeMode === "system" ? null : resolvedTheme,
          ),
        )
        .then(() => syncWindowChrome(resolvedTheme))
        .catch(() => {
          // Native theme sync is best-effort; CSS classes still drive the UI.
        });
    }
  }, [resolvedTheme, themeMode]);

  useEffect(() => {
    if (!hasLoadedSnapshot || !isTauri() || startupUpdateCheckStarted.current) {
      return;
    }

    let active = true;
    const timerId = window.setTimeout(() => {
      if (!active || startupUpdateCheckStarted.current) {
        return;
      }

      startupUpdateCheckStarted.current = true;
      checkForAppUpdate()
        .then((result) => {
          if (!active || !result.available || !result.update) {
            return;
          }

          setAvailableUpdate(result.update);
          if (dismissedUpdateVersion === result.update.version) {
            setUpdateStatus("idle");
            setUpdateMessage(ui.settings.updateDismissed);
            return;
          }

          setUpdateStatus("available");
          setUpdateMessage(`${ui.settings.updateAvailable}: v${result.update.version}`);
        })
        .catch(() => {
          // Startup checks should not interrupt normal app startup.
        });
    }, 1200);

    return () => {
      active = false;
      window.clearTimeout(timerId);
    };
  }, [dismissedUpdateVersion, hasLoadedSnapshot, ui]);

  useEffect(() => {
    if (!isPerformanceActive) {
      return;
    }

    let active = true;
    const samplePerformance = () => {
      readPerformanceSample()
        .then((sample) => {
          if (!active) {
            return;
          }

          setPerformanceSamples((samples) =>
            [...samples, sample].slice(-PERFORMANCE_SAMPLE_LIMIT),
          );
        })
        .catch(() => {
          // Keep the previous chart if a platform sample fails.
        });
    };
    samplePerformance();
    const intervalId = window.setInterval(samplePerformance, 3000);

    return () => {
      active = false;
      window.clearInterval(intervalId);
    };
  }, [isPerformanceActive]);

  async function persistLayout(nextLayout: LayoutState) {
    setIsSaving(true);
    try {
      const persistedLayout = await saveLayout(nextLayout);
      setSnapshot((current) =>
        current
          ? {
              ...current,
              layout: persistedLayout,
            }
          : current,
      );
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.saveLayout,
      );
    } finally {
      setIsSaving(false);
    }
  }

  const endDrag = useEffectEvent(() => {
    if (layout) {
      void persistLayout(layout);
    }
    setDragState(null);
  });

  const handleGlobalPointerMove = useEffectEvent((event: PointerEvent) => {
    if (!dragState || !layout || event.pointerId !== dragState.pointerId) {
      return;
    }

    event.preventDefault();

    const dragViewport = dragState.viewport ?? boardViewport;
    const deltaX =
      Math.round(
        (event.clientX - dragState.originClientX) /
          dragViewport.metrics.scale /
          SNAP_SIZE,
      ) * SNAP_SIZE;
    const deltaY =
      Math.round(
        (event.clientY - dragState.originClientY) /
          dragViewport.metrics.scale /
          SNAP_SIZE,
      ) * SNAP_SIZE;

    setSnapshot((current) =>
      current
        ? {
            ...current,
            layout: moveScreen(
              current.layout,
              dragState.screenId,
              snapScreenPosition(current.layout, dragState.screenId, {
                x: dragState.startX + deltaX,
                y: dragState.startY + deltaY,
              }),
            ),
          }
        : current,
    );
  });

  useEffect(() => {
    if (!dragState) {
      return;
    }

    const onPointerMove = (event: PointerEvent) =>
      handleGlobalPointerMove(event);
    const onPointerUp = () => endDrag();

    window.addEventListener("pointermove", onPointerMove);
    window.addEventListener("pointerup", onPointerUp);
    window.addEventListener("pointercancel", onPointerUp);

    return () => {
      window.removeEventListener("pointermove", onPointerMove);
      window.removeEventListener("pointerup", onPointerUp);
      window.removeEventListener("pointercancel", onPointerUp);
    };
  }, [dragState]);

  async function setRuntimeState(nextStarted: boolean) {
    setIsRuntimePending(true);
    setErrorMessage(null);

    try {
      const nextRuntime = nextStarted
        ? await startRuntime()
        : await stopRuntime();
      setSnapshot((current) =>
        current
          ? {
              ...current,
              runtime: nextRuntime,
            }
          : current,
      );
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
    } finally {
      setIsRuntimePending(false);
    }
  }

  async function scanLan() {
    setIsScanningLan(true);
    setErrorMessage(null);

    try {
      const discovery = await scanLanPeers();
      setSnapshot((current) =>
        current
          ? {
              ...current,
              runtime: {
                ...current.runtime,
                discovery,
              },
            }
          : current,
      );
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.scanLan,
      );
    } finally {
      setIsScanningLan(false);
    }
  }

  function boardRect(screen: Screen) {
    return {
      left:
        boardViewport.metrics.offsetX +
        (screen.x - boardViewport.bounds.minX) * boardViewport.metrics.scale,
      top:
        boardViewport.metrics.offsetY +
        (screen.y - boardViewport.bounds.minY) * boardViewport.metrics.scale,
      width: screen.width * boardViewport.metrics.scale,
      height: screen.height * boardViewport.metrics.scale,
    };
  }

  function setBoardZoomValue(nextZoom: number) {
    setBoardZoom(clampZoom(nextZoom));
  }

  function zoomBoard(delta: number) {
    setBoardZoom((currentZoom) => clampZoom(currentZoom + delta));
  }

  function updateLayout(mutator: (layoutState: LayoutState) => LayoutState) {
    setSnapshot((current) => {
      if (!current) {
        return current;
      }

      const nextLayout = mutator(current.layout);
      void persistLayout(nextLayout);

      return {
        ...current,
        layout: nextLayout,
      };
    });
  }

  function handleScreenPointerDown(
    event: React.PointerEvent<HTMLButtonElement>,
    screen: Screen,
  ) {
    event.preventDefault();
    const target = event.currentTarget;
    target.setPointerCapture(event.pointerId);
    setSnapshot((current) =>
      current
        ? {
            ...current,
            layout: {
              ...current.layout,
              activeDeviceId: screen.deviceId,
              selectedScreenId: screen.id,
            },
          }
        : current,
    );
    setDragState({
      pointerId: event.pointerId,
      screenId: screen.id,
      originClientX: event.clientX,
      originClientY: event.clientY,
      startX: screen.x,
      startY: screen.y,
      viewport: {
        bounds,
        metrics: boardMetrics,
      },
    });
  }

  function handleScreenKeyDown(
    event: React.KeyboardEvent<HTMLButtonElement>,
    screen: Screen,
  ) {
    const step = event.shiftKey ? SNAP_SIZE * 5 : SNAP_SIZE;
    const deltas: Partial<Record<string, { x: number; y: number }>> = {
      ArrowLeft: { x: -step, y: 0 },
      ArrowRight: { x: step, y: 0 },
      ArrowUp: { x: 0, y: -step },
      ArrowDown: { x: 0, y: step },
    };
    const delta = deltas[event.key];
    if (!delta) {
      return;
    }

    event.preventDefault();
    updateLayout((layoutState) => {
      const currentScreen = getScreenById(layoutState, screen.id);
      if (!currentScreen) {
        return layoutState;
      }

      return moveScreen(
        layoutState,
        currentScreen.id,
        snapScreenPosition(layoutState, currentScreen.id, {
          x: currentScreen.x + delta.x,
          y: currentScreen.y + delta.y,
        }),
      );
    });
  }

  function setClipboardSync(clipboardSync: boolean) {
    updateLayout((layoutState) => ({
      ...layoutState,
      clipboardSync,
    }));
  }

  function setLanguage(language: AppLanguage) {
    updateLayout((layoutState) => ({
      ...layoutState,
      language,
    }));
  }

  function setThemeMode(themeMode: ThemeMode) {
    updateLayout((layoutState) => ({
      ...layoutState,
      themeMode,
    }));
  }

  function setPerformanceMonitor(performanceMonitor: boolean) {
    updateLayout((layoutState) => ({
      ...layoutState,
      performanceMonitor,
    }));
    if (!performanceMonitor) {
      setPerformanceSamples([]);
    }
  }

  function setTransportPortMode(transportPortMode: TransportPortMode) {
    updateLayout((layoutState) => ({
      ...layoutState,
      transportPortMode,
    }));
  }

  function setTransportPort(transportPort: number) {
    const normalizedPort = normalizePort(transportPort);
    updateLayout((layoutState) => ({
      ...layoutState,
      transportPort: normalizedPort,
      quicPort: normalizePort(normalizedPort + 1),
      transportPortMode: "fixed",
    }));
  }

  async function refreshClipboardText() {
    setIsClipboardPending(true);
    setErrorMessage(null);

    try {
      setClipboardText(await readClipboardText());
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.readClipboard,
      );
    } finally {
      setIsClipboardPending(false);
    }
  }

  async function writeClipboard() {
    setIsClipboardPending(true);
    setErrorMessage(null);

    try {
      await writeClipboardText(clipboardText);
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.writeClipboard,
      );
    } finally {
      setIsClipboardPending(false);
    }
  }

  async function handleRestartAsAdmin() {
    setIsAdminRestartPending(true);
    setErrorMessage(null);

    try {
      await restartAsAdmin();
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
      setIsAdminRestartPending(false);
    }
  }

  async function setMachineRole(machineRole: Exclude<MachineRole, "unset">) {
    if (!layout) {
      return;
    }

    const nextLayout: LayoutState = {
      ...layout,
      machineRole,
      inputMode: machineRole === "client" ? "receive" : "control",
    };

    setErrorMessage(null);
    await persistLayout(nextLayout);
    setActiveTab(machineRole === "client" ? "settings" : "layout");

    if (machineRole === "client" && !runtime?.started) {
      await setRuntimeState(true);
    }
  }

  async function handleAddManualDevice(
    event: React.FormEvent<HTMLFormElement>,
  ) {
    event.preventDefault();
    const host = manualDeviceHost.trim();
    if (!host) {
      setErrorMessage(ui.errors.manualHostRequired);
      return;
    }

    setIsAddingDevice(true);
    setErrorMessage(null);

    try {
      const peer = await probeLanPeer(host);
      if (peer.screens.length === 0) {
        setErrorMessage(`${host}: ${ui.errors.connectedWithoutScreens}`);
        return;
      }

      updateLayout((layoutState) =>
        upsertPeerDevice(layoutState, peer, manualDeviceName.trim()),
      );
      setManualDeviceName("");
      setManualDeviceHost("");
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error
          ? error.message
          : `${host}: ${ui.errors.probeFailed}`,
      );
    } finally {
      setIsAddingDevice(false);
    }
  }

  function handleAddPeer(peer: LanPeer) {
    if (peer.screens.length === 0) {
      setErrorMessage(`${peer.name} ${ui.errors.peerWithoutScreens}`);
      return;
    }

    updateLayout((layoutState) => {
      return upsertPeerDevice(layoutState, peer);
    });
  }

  function handleRemoveDevice(deviceId: string) {
    updateLayout((layoutState) => {
      const nextDevices = layoutState.devices.filter(
        (device) => device.id !== deviceId,
      );
      const fallbackDevice = nextDevices[0];
      const activeDeviceId = nextDevices.some(
        (device) => device.id === layoutState.activeDeviceId,
      )
        ? layoutState.activeDeviceId
        : (fallbackDevice?.id ?? layoutState.activeDeviceId);
      const selectedScreenId = nextDevices.some((device) =>
        device.screens.some(
          (screen) => screen.id === layoutState.selectedScreenId,
        ),
      )
        ? layoutState.selectedScreenId
        : (fallbackDevice?.screens[0]?.id ?? layoutState.selectedScreenId);

      return {
        ...layoutState,
        devices: nextDevices,
        activeDeviceId,
        selectedScreenId,
      };
    });
  }

  function openRepository() {
    void openRepositoryUrl();
  }

  async function checkDesktopUpdate() {
    if (!isTauri()) {
      setUpdateStatus("current");
      setAvailableUpdate(null);
      setUpdateMessage(ui.settings.updatesBrowserCopy);
      return;
    }

    setUpdateStatus("checking");
    setUpdateMessage(null);

    try {
      const result = await checkForAppUpdate();

      if (result.available && result.update) {
        localStorage.removeItem(UPDATE_DISMISSED_VERSION_KEY);
        setDismissedUpdateVersion(null);
        setAvailableUpdate(result.update);
        setUpdateStatus("available");
        setUpdateMessage(`${ui.settings.updateAvailable}: v${result.update.version}`);
        return;
      }

      setAvailableUpdate(null);
      setUpdateStatus("current");
      setUpdateMessage(ui.settings.updateCurrent);
    } catch (error: unknown) {
      setUpdateStatus("error");
      setUpdateMessage(
        error instanceof Error ? error.message : ui.errors.checkUpdate,
      );
    }
  }

  async function installDesktopUpdate() {
    if (!availableUpdate || updateStatus === "installing") {
      return;
    }

    setUpdateStatus("installing");
    setUpdateMessage(`${ui.settings.updateInstalling}: v${availableUpdate.version}`);

    try {
      if (isPortable) {
        await openUpdateReleasePage();
        setUpdateStatus("available");
        setUpdateMessage(ui.settings.portableUpdateCopy);
        return;
      }

      await installAppUpdate();
    } catch (error: unknown) {
      await openUpdateReleasePage().catch(() => {
        // The original update error is more useful than a secondary browser error.
      });
      const errorText =
        error instanceof Error ? error.message : ui.errors.installUpdate;
      setUpdateStatus("error");
      setUpdateMessage(`${errorText} ${ui.settings.updateFallback}`);
    }
  }

  function dismissDesktopUpdate() {
    if (!availableUpdate) {
      return;
    }

    localStorage.setItem(UPDATE_DISMISSED_VERSION_KEY, availableUpdate.version);
    setDismissedUpdateVersion(availableUpdate.version);
    setUpdateStatus("idle");
    setUpdateMessage(ui.settings.updateDismissed);
  }

  function openUpdateDownloads() {
    void openUpdateReleasePage();
  }

  if (!snapshot || !layout || !runtime || !displayLayout) {
    return (
      <main className={shellClassName}>
        {renderWindowTitlebar()}
        <section className="loading-panel">
          <p className="eyebrow">mykvm</p>
          <h1>{ui.loading.title}</h1>
          <p>{ui.loading.copy}</p>
          {errorMessage ? <p className="error-banner">{errorMessage}</p> : null}
        </section>
      </main>
    );
  }

  if (machineRole === "unset") {
    return (
      <main className={`${shellClassName} onboarding-shell`}>
        {renderWindowTitlebar()}
        <section className="onboarding-panel">
          <div className="onboarding-copy">
            <p className="eyebrow">{ui.onboarding.eyebrow}</p>
            <h1>{ui.onboarding.title}</h1>
            <p>{ui.onboarding.copy}</p>
          </div>

          <div className="role-choice-grid">
            <button
              type="button"
              className="role-choice-card"
              onClick={() => void setMachineRole("server")}
            >
              <span>Server</span>
              <strong>{ui.onboarding.serverTitle}</strong>
              <p>{ui.onboarding.serverCopy}</p>
            </button>
            <button
              type="button"
              className="role-choice-card"
              onClick={() => void setMachineRole("client")}
            >
              <span>Client</span>
              <strong>{ui.onboarding.clientTitle}</strong>
              <p>{ui.onboarding.clientCopy}</p>
            </button>
          </div>
        </section>
      </main>
    );
  }

  const activeDevice = displayLayout.devices.find(
    (device) => device.id === layout.activeDeviceId,
  );
  const onlineDeviceCount = displayLayout.devices.filter(
    (device) => device.online,
  ).length;
  const runtimeStateLabel = runtime.started
    ? ui.common.running
    : ui.common.stopped;
  const roleLabel = ui.roles[machineRole];
  const lanPeers = runtime.discovery.peers;
  const addedOnlyDevices = displayLayout.devices.filter(
    (device) => !lanPeers.some((peer) => deviceMatchesPeer(device, peer)),
  );

  function renderAddedDeviceActions(device: Device) {
    if (device.role === "local") {
      return null;
    }

    return (
      <button
        type="button"
        className="secondary-button compact-button danger-button"
        onClick={() => handleRemoveDevice(device.id)}
      >
        {ui.common.remove}
      </button>
    );
  }

  return (
    <main className={shellClassName}>
      {renderWindowTitlebar()}
      <header className="app-header">
        <div className="brand-lockup">
          <span className="brand-mark">mk</span>
          <div>
            <strong>MyKVM</strong>
            <span>
              {roleLabel} · {runtimeStateLabel} · {onlineDeviceCount}/
              {layout.devices.length} {ui.common.online}
            </span>
          </div>
        </div>

        <div className="header-actions">
          <nav className="header-tabs" aria-label="mykvm sections">
            {visibleTabs.map((tab) => (
              <button
                key={tab.id}
                type="button"
                className={currentTab === tab.id ? "active" : ""}
                onClick={() => setActiveTab(tab.id)}
              >
                {ui.tabs[tab.id]}
              </button>
            ))}
          </nav>
          <button
            type="button"
            className={`runtime-toggle-button ${runtime.started ? "running" : "stopped"} ${
              isRuntimePending ? "pending" : ""
            }`}
            onClick={() => void setRuntimeState(!runtime.started)}
            disabled={isRuntimePending}
            aria-label={
              isRuntimePending
                ? ui.common.pending
                : runtime.started
                  ? ui.common.stop
                  : ui.common.start
            }
            title={
              isRuntimePending
                ? ui.common.pending
                : runtime.started
                  ? ui.common.stop
                  : ui.common.start
            }
          >
            {runtime.started ? <StopIcon /> : <PlayIcon />}
          </button>
        </div>
      </header>

      {errorMessage ? <p className="error-banner">{errorMessage}</p> : null}

      {machineRole === "server" && currentTab === "layout" ? (
        <section className="workspace-shell">
          <section className="layout-panel">
            <div className="layout-toolbar">
              <div>
                <p className="eyebrow">{ui.layout.eyebrow}</p>
                <h1>{ui.layout.title}</h1>
              </div>
              <div className="toolbar-actions">
                <span className={`status-pill ${isSaving ? "saving" : ""}`}>
                  {isSaving ? ui.common.saving : ui.common.synced}
                </span>
                <button
                  type="button"
                  className="primary-button compact-button"
                  onClick={() => setActiveTab("devices")}
                >
                  {ui.layout.addDevice}
                </button>
              </div>
            </div>

            <div className="layout-board" ref={boardRef}>
              <div className="board-grid" />
              {screens.map((screen) => {
                const rect = boardRect(screen);
                const statusKind = screenStatusKind(screen);

                return (
                  <button
                    key={screen.id}
                    type="button"
                    className={`screen-rect ${layout.selectedScreenId === screen.id ? "selected" : ""} ${
                      dragState?.screenId === screen.id ? "dragging" : ""
                    } ${statusKind}`}
                    style={
                      {
                        left: rect.left,
                        top: rect.top,
                        width: rect.width,
                        height: rect.height,
                        "--screen-color": screen.deviceColor,
                      } as CSSProperties
                    }
                    onPointerDown={(event) =>
                      handleScreenPointerDown(event, screen)
                    }
                    onKeyDown={(event) => handleScreenKeyDown(event, screen)}
                  >
                    {statusKind === "local" ||
                    statusKind === "online" ||
                    statusKind === "offline" ? (
                      <em className={`screen-status ${statusKind}`}>
                        {screenStatusLabel(screen, {
                          local: ui.devices.local,
                          online: ui.common.online,
                          offline: ui.common.offline,
                        })}
                      </em>
                    ) : null}
                    <strong>{screen.name}</strong>
                    <span>{screen.deviceName}</span>
                    <small>
                      {screen.width} x {screen.height}
                    </small>
                  </button>
                );
              })}
              <div className="board-zoom-controls">
                <button
                  type="button"
                  title={ui.layout.zoomOut}
                  aria-label={ui.layout.zoomOut}
                  onClick={() => zoomBoard(-BOARD_ZOOM_STEP)}
                  disabled={boardZoom <= BOARD_ZOOM_MIN}
                >
                  <ZoomOutIcon />
                </button>
                <button
                  type="button"
                  className="board-zoom-reset"
                  title={ui.layout.fitView}
                  aria-label={ui.layout.fitView}
                  onClick={() => setBoardZoomValue(1)}
                >
                  {formatZoom(boardZoom)}
                </button>
                <button
                  type="button"
                  title={ui.layout.zoomIn}
                  aria-label={ui.layout.zoomIn}
                  onClick={() => zoomBoard(BOARD_ZOOM_STEP)}
                  disabled={boardZoom >= BOARD_ZOOM_MAX}
                >
                  <ZoomInIcon />
                </button>
              </div>
            </div>
          </section>
        </section>
      ) : null}

      {machineRole === "server" && currentTab === "devices" ? (
        <section className="page-panel">
          <div className="page-heading">
            <div>
              <p className="eyebrow">{ui.devices.eyebrow}</p>
              <h1>{ui.devices.title}</h1>
              <p>{ui.devices.subtitle}</p>
            </div>
          </div>

          <div className="connection-stack">
            <section className="surface-card connection-add-card">
              <div>
                <h2>{ui.devices.addTitle}</h2>
                <p>{ui.devices.addCopy}</p>
              </div>
              <form
                className="add-device-form"
                onSubmit={handleAddManualDevice}
              >
                <input
                  value={manualDeviceName}
                  onChange={(event) => setManualDeviceName(event.target.value)}
                  placeholder={ui.devices.deviceNamePlaceholder}
                />
                <input
                  value={manualDeviceHost}
                  onChange={(event) => setManualDeviceHost(event.target.value)}
                  placeholder={ui.devices.hostPlaceholder}
                />
                <button
                  type="button"
                  className="secondary-button"
                  onClick={() => void scanLan()}
                  disabled={isScanningLan}
                >
                  {isScanningLan ? ui.devices.scanning : ui.devices.scanLan}
                </button>
                <button
                  type="submit"
                  className="primary-button"
                  disabled={isAddingDevice}
                >
                  {isAddingDevice ? ui.common.connecting : ui.common.add}
                </button>
              </form>
            </section>

            <section className="surface-card connection-list-card">
              <h2>{ui.devices.listTitle}</h2>
              <div className="connection-list">
                {lanPeers.map((peer) => {
                  const addedDevice = findPeerDevice(layout, peer);
                  const screenCount =
                    peer.screens.length || addedDevice?.screens.length || 0;

                  return (
                    <article
                      key={`peer-${peer.id}`}
                      className={`connection-row ${addedDevice?.id === layout.activeDeviceId ? "active" : ""}`}
                    >
                      <div className="connection-main">
                        <span
                          className={`device-badge ${peer.inputReady ? "device-badge-online" : "device-badge-offline"}`}
                        />
                        <div>
                          <div className="connection-title">
                            <strong>{addedDevice?.name ?? peer.name}</strong>
                            <div className="connection-tags">
                              <span className="tag-pill tag-pill-lan">
                                {ui.devices.lan}
                              </span>
                            </div>
                          </div>
                          <p className="connection-meta">
                            {peer.platform} · {peer.ip} ·{" "}
                            {screenCount > 0
                              ? formatScreenCount(screenCount, language)
                              : ui.devices.noScreens}
                          </p>
                        </div>
                      </div>

                      <div className="connection-actions">
                        {addedDevice ? (
                          renderAddedDeviceActions(addedDevice)
                        ) : (
                          <button
                            type="button"
                            className="secondary-button compact-button"
                            onClick={() => handleAddPeer(peer)}
                            disabled={peer.screens.length === 0}
                          >
                            {peer.screens.length === 0
                              ? ui.devices.noScreens
                              : ui.common.add}
                          </button>
                        )}
                      </div>
                    </article>
                  );
                })}

                {addedOnlyDevices.map((device) => (
                  <article
                    key={`device-${device.id}`}
                    className={`connection-row ${layout.activeDeviceId === device.id ? "active" : ""}`}
                  >
                    <div className="connection-main">
                      <span
                        className={`device-badge ${deviceBadgeStatusClass(device)}`}
                      />
                      <div>
                        <div className="connection-title">
                          <strong>{device.name}</strong>
                          <div className="connection-tags">
                            <span
                              className={`tag-pill ${deviceSourceTagClass(device)}`}
                            >
                              {deviceSourceLabel(device, language)}
                            </span>
                          </div>
                        </div>
                        <p className="connection-meta">
                          {PLATFORM_LABELS[device.platform]} · {device.host} ·{" "}
                          {formatScreenCount(device.screens.length, language)}
                        </p>
                      </div>
                    </div>

                    <div className="connection-actions">
                      {renderAddedDeviceActions(device)}
                    </div>
                  </article>
                ))}
              </div>
            </section>
          </div>
        </section>
      ) : null}

      {currentTab === "settings" ? (
        <section className="page-panel">
          <div className="page-heading">
            <div>
              <p className="eyebrow">{ui.settings.eyebrow}</p>
              <h1>{ui.settings.title}</h1>
              <p>{ui.settings.subtitle}</p>
            </div>
          </div>

          <div className="settings-layout">
            <div className="settings-column">
              <section className="surface-card settings-card">
                <h2>{ui.settings.roleTitle}</h2>
                <div className="role-switcher">
                  <button
                    type="button"
                    className={machineRole === "server" ? "active" : ""}
                    onClick={() => void setMachineRole("server")}
                  >
                    {ui.roles.server}
                  </button>
                  <button
                    type="button"
                    className={machineRole === "client" ? "active" : ""}
                    onClick={() => void setMachineRole("client")}
                  >
                    {ui.roles.client}
                  </button>
                </div>
                <p className="muted-copy">{ui.settings.roleCopy}</p>
              </section>

              <section className="surface-card settings-card">
                <h2>{ui.settings.transport}</h2>
                <p className="muted-copy">{ui.settings.transportCopy}</p>
                <div className="settings-control-row">
                  <span>{ui.settings.portMode}</span>
                  <div className="segmented-control">
                    {(["auto", "fixed"] as TransportPortMode[]).map((mode) => (
                      <button
                        key={mode}
                        type="button"
                        className={
                          layout.transportPortMode === mode ? "active" : ""
                        }
                        onClick={() => setTransportPortMode(mode)}
                      >
                        {mode === "auto"
                          ? ui.settings.autoPort
                          : ui.settings.fixedPort}
                      </button>
                    ))}
                  </div>
                </div>
                <div className="settings-control-row">
                  <span>{ui.settings.portValue}</span>
                  <input
                    className="settings-number-input"
                    type="number"
                    min="1024"
                    max="65535"
                    value={layout.transportPort}
                    placeholder={ui.settings.portPlaceholder}
                    onChange={(event) =>
                      setTransportPort(Number(event.target.value))
                    }
                  />
                </div>
              </section>

              <section className="surface-card settings-card">
                <h2>{ui.settings.appearanceTitle}</h2>
                <div className="settings-control-row">
                  <span>{ui.settings.language}</span>
                  <div className="segmented-control">
                    <button
                      type="button"
                      className={language === "cn" ? "active" : ""}
                      onClick={() => setLanguage("cn")}
                    >
                      {ui.settings.simplifiedChinese}
                    </button>
                    <button
                      type="button"
                      className={language === "en" ? "active" : ""}
                      onClick={() => setLanguage("en")}
                    >
                      {ui.settings.english}
                    </button>
                  </div>
                </div>
                <div className="settings-control-row">
                  <span>{ui.settings.theme}</span>
                  <div className="segmented-control">
                    {(["system", "dark", "light"] as ThemeMode[]).map(
                      (mode) => (
                        <button
                          key={mode}
                          type="button"
                          className={themeMode === mode ? "active" : ""}
                          onClick={() => setThemeMode(mode)}
                        >
                          {mode === "system"
                            ? ui.settings.system
                            : mode === "dark"
                              ? ui.settings.dark
                              : ui.settings.light}
                        </button>
                      ),
                    )}
                  </div>
                </div>
              </section>

              <section className="surface-card clipboard-card">
                <div className="card-title-row">
                  <h2>{ui.settings.clipboard}</h2>
                  <button
                    type="button"
                    className={`switch-button ${layout.clipboardSync ? "active" : ""}`}
                    onClick={() => setClipboardSync(!layout.clipboardSync)}
                  >
                    {layout.clipboardSync
                      ? ui.common.enabled
                      : ui.common.disabled}
                  </button>
                </div>
                <p className="muted-copy">{ui.settings.clipboardCopy}</p>
                <p className="muted-copy">{runtime.clipboard.detail}</p>
                <textarea
                  className="clipboard-textarea"
                  value={clipboardText}
                  onChange={(event) => setClipboardText(event.target.value)}
                  placeholder={ui.settings.clipboardPlaceholder}
                />
                <div className="inline-actions">
                  <button
                    type="button"
                    className="secondary-button compact-button"
                    onClick={() => void refreshClipboardText()}
                    disabled={isClipboardPending}
                  >
                    {ui.settings.readClipboard}
                  </button>
                  <button
                    type="button"
                    className="primary-button compact-button"
                    onClick={() => void writeClipboard()}
                    disabled={isClipboardPending}
                  >
                    {ui.settings.writeClipboard}
                  </button>
                </div>
              </section>
            </div>

            <div className="settings-column">
              <section className="surface-card settings-card">
                <h2>{ui.settings.localInfo}</h2>
                <dl className="network-meta compact-meta">
                  <div>
                    <dt>{ui.settings.name}</dt>
                    <dd>{runtime.discovery.localPeer.name}</dd>
                  </div>
                  <div>
                    <dt>{ui.settings.address}</dt>
                    <dd>{runtime.discovery.localPeer.ip}</dd>
                  </div>
                  <div>
                    <dt>{ui.settings.ports}</dt>
                    <dd>
                      UDP {runtime.discovery.port} · QUIC{" "}
                      {runtime.discovery.localPeer.quicPort}
                    </dd>
                  </div>
                  <div>
                    <dt>{ui.settings.activeDevice}</dt>
                    <dd>{activeDevice?.name ?? ui.common.none}</dd>
                  </div>
                  <div>
                    <dt>{ui.settings.privilege}</dt>
                    <dd>
                      {runtime.privilege.isElevated
                        ? ui.settings.adminPrivilege
                        : ui.settings.standardPrivilege}
                    </dd>
                  </div>
                </dl>
                <p className="muted-copy">{runtime.privilege.detail}</p>
                {runtime.privilege.canElevate ? (
                  <div className="inline-actions">
                    <button
                      type="button"
                      className="primary-button compact-button"
                      onClick={() => void handleRestartAsAdmin()}
                      disabled={isAdminRestartPending}
                    >
                      {isAdminRestartPending
                        ? ui.settings.adminRestarting
                        : ui.settings.restartAsAdmin}
                    </button>
                  </div>
                ) : null}
              </section>

              <section className="surface-card settings-card update-card">
                <div className="card-title-row">
                  <h2>{ui.settings.updates}</h2>
                  <span className={`update-status-badge ${updateStatus}`}>
                    {updateStatusLabel(updateStatus, ui)}
                  </span>
                </div>
                <p className="muted-copy">
                  {isTauri()
                    ? isPortable
                      ? ui.settings.portableUpdateCopy
                      : ui.settings.updatesCopy
                    : ui.settings.updatesBrowserCopy}
                </p>
                <dl className="network-meta compact-meta">
                  <div>
                    <dt>{ui.settings.currentVersion}</dt>
                    <dd>v{APP_VERSION}</dd>
                  </div>
                  <div>
                    <dt>{ui.settings.latestVersion}</dt>
                    <dd>
                      {availableUpdate ? `v${availableUpdate.version}` : "--"}
                    </dd>
                  </div>
                </dl>
                {updateMessage ? (
                  <p className={`muted-copy update-message ${updateStatus}`}>
                    {updateMessage}
                  </p>
                ) : null}
                <div className="inline-actions">
                  <button
                    type="button"
                    className="secondary-button compact-button"
                    onClick={() => void checkDesktopUpdate()}
                    disabled={
                      updateStatus === "checking" ||
                      updateStatus === "installing"
                    }
                  >
                    {updateStatus === "checking"
                      ? ui.settings.checkingUpdate
                      : ui.settings.checkUpdate}
                  </button>
                  <button
                    type="button"
                    className="primary-button compact-button"
                    onClick={() => void installDesktopUpdate()}
                    disabled={
                      !availableUpdate ||
                      updateStatus === "checking" ||
                      updateStatus === "installing"
                    }
                  >
                    {updateStatus === "installing"
                      ? ui.settings.installingUpdate
                      : ui.settings.installUpdate}
                  </button>
                  {availableUpdate ? (
                    <button
                      type="button"
                      className="secondary-button compact-button"
                      onClick={dismissDesktopUpdate}
                      disabled={
                        isAvailableUpdateDismissed ||
                        updateStatus === "checking" ||
                        updateStatus === "installing"
                      }
                    >
                      {ui.settings.dismissUpdate}
                    </button>
                  ) : null}
                  <button
                    type="button"
                    className="secondary-button compact-button"
                    onClick={openUpdateDownloads}
                    disabled={updateStatus === "installing"}
                  >
                    {ui.settings.openReleases}
                  </button>
                </div>
              </section>

              <section className="surface-card performance-card">
                <div className="card-title-row">
                  <h2>{ui.settings.performance}</h2>
                  <button
                    type="button"
                    className={`switch-button ${layout.performanceMonitor ? "active" : ""}`}
                    onClick={() =>
                      setPerformanceMonitor(!layout.performanceMonitor)
                    }
                  >
                    {layout.performanceMonitor
                      ? ui.common.enabled
                      : ui.common.disabled}
                  </button>
                </div>
                <p className="muted-copy">{ui.settings.performanceCopy}</p>
                <div
                  className="performance-chart"
                  aria-label={ui.settings.performance}
                >
                  {performanceSamples.length > 0 ? (
                    performanceSamples.map((sample) => (
                      <span
                        key={sample.timestampMs}
                        style={{
                          height: `${Math.max(6, Math.min(100, sample.appCpuPercent))}%`,
                        }}
                      />
                    ))
                  ) : (
                    <em>{ui.settings.noSamples}</em>
                  )}
                </div>
                <dl className="runtime-meta">
                  <div>
                    <dt>{ui.settings.cpu}</dt>
                    <dd>
                      {formatPercent(performanceSamples.at(-1)?.appCpuPercent)}
                    </dd>
                  </div>
                  <div>
                    <dt>{ui.settings.memory}</dt>
                    <dd>{formatMemory(performanceSamples.at(-1))}</dd>
                  </div>
                  <div>
                    <dt>{ui.settings.packets}</dt>
                    <dd>{formatPacketRate(performanceSamples)}</dd>
                  </div>
                  <div>
                    <dt>{ui.settings.inputRate}</dt>
                    <dd>{formatInputRate(performanceSamples)}</dd>
                  </div>
                  <div>
                    <dt>{ui.settings.clipboardPackets}</dt>
                    <dd>
                      {performanceSamples.at(-1)?.clipboardPackets ?? "--"}
                    </dd>
                  </div>
                </dl>
              </section>
            </div>
          </div>
        </section>
      ) : null}

      <footer className="app-footer">
        <span className="footer-copy">
          <span>
            Copyright © 2026{" "}
            <a
              href={REPOSITORY_URL}
              className="footer-link-button"
              title={REPOSITORY_URL}
              target="_blank"
              rel="noreferrer"
              onClick={(event) => {
                event.preventDefault();
                openRepository();
              }}
            >
              MyKVM
            </a>
          </span>
          <span>v{APP_VERSION}</span>
        </span>
        <a
          href={REPOSITORY_URL}
          className="footer-icon-button"
          title={REPOSITORY_URL}
          target="_blank"
          rel="noreferrer"
          aria-label={ui.common.github}
          onClick={(event) => {
            event.preventDefault();
            openRepository();
          }}
        >
          <GitHubIcon />
        </a>
      </footer>
    </main>
  );
}

function fallbackScreen(): Screen {
  return {
    id: "fallback",
    deviceId: "fallback",
    name: "Fallback",
    x: 0,
    y: 0,
    width: 1,
    height: 1,
    scale: 1,
    isPrimary: true,
  };
}

function getSystemTheme(): Exclude<ThemeMode, "system"> {
  if (window.matchMedia("(prefers-color-scheme: dark)").matches) {
    return "dark";
  }

  if (window.matchMedia("(prefers-color-scheme: light)").matches) {
    return "light";
  }

  return "dark";
}

function resolveTheme(
  themeMode: ThemeMode,
  systemTheme: Exclude<ThemeMode, "system">,
) {
  if (themeMode !== "system") {
    return themeMode;
  }

  return systemTheme;
}

function formatPercent(value?: number) {
  return typeof value === "number" ? `${Math.round(value)}%` : "--";
}

function formatMemory(sample?: PerformanceSample) {
  if (!sample) {
    return "--";
  }

  return `${Math.round(sample.appMemoryMb)} MB`;
}

function formatPacketRate(samples: PerformanceSample[]) {
  return formatCounterRate(samples, (sample) => sample.transportPackets, "/s");
}

function formatInputRate(samples: PerformanceSample[]) {
  return formatCounterRate(samples, (sample) => sample.inputEvents, "/s");
}

function formatCounterRate(
  samples: PerformanceSample[],
  pick: (sample: PerformanceSample) => number,
  suffix: string,
) {
  if (samples.length < 2) {
    return "--";
  }

  const previous = samples[samples.length - 2];
  const current = samples[samples.length - 1];
  const elapsedSeconds = Math.max(
    (current.timestampMs - previous.timestampMs) / 1000,
    0.001,
  );
  const rate = Math.max(0, (pick(current) - pick(previous)) / elapsedSeconds);

  return `${rate.toFixed(rate >= 10 ? 0 : 1)}${suffix}`;
}

function normalizePort(value: number) {
  if (!Number.isFinite(value)) {
    return 47833;
  }

  return Math.round(Math.min(65535, Math.max(1024, value)));
}

function clampZoom(value: number) {
  return Math.min(BOARD_ZOOM_MAX, Math.max(BOARD_ZOOM_MIN, value));
}

function formatZoom(value: number) {
  return `${Math.round(value * 100)}%`;
}

function GitHubIcon() {
  return (
    <svg
      className="github-icon"
      viewBox="0 0 24 24"
      aria-hidden="true"
      focusable="false"
    >
      <path
        fill="currentColor"
        d="M12 .5A11.5 11.5 0 0 0 8.36 22.9c.58.11.79-.25.79-.56v-2.18c-3.22.7-3.9-1.37-3.9-1.37-.53-1.34-1.29-1.7-1.29-1.7-1.05-.72.08-.7.08-.7 1.16.08 1.77 1.2 1.77 1.2 1.03 1.76 2.7 1.25 3.36.96.1-.75.4-1.25.73-1.54-2.57-.29-5.27-1.28-5.27-5.72 0-1.26.45-2.3 1.19-3.11-.12-.29-.52-1.48.11-3.07 0 0 .98-.31 3.18 1.19a10.96 10.96 0 0 1 5.78 0c2.2-1.5 3.17-1.19 3.17-1.19.64 1.59.24 2.78.12 3.07.74.81 1.18 1.85 1.18 3.11 0 4.45-2.71 5.43-5.29 5.72.42.36.79 1.07.79 2.16v3.17c0 .31.21.68.8.56A11.5 11.5 0 0 0 12 .5Z"
      />
    </svg>
  );
}

function PlayIcon() {
  return (
    <svg className="runtime-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path fill="currentColor" d="M7 4.5v15L18.8 12 7 4.5Z" />
    </svg>
  );
}

function StopIcon() {
  return (
    <svg className="runtime-icon" viewBox="0 0 24 24" aria-hidden="true">
      <rect width="12" height="12" x="6" y="6" rx="2.4" fill="currentColor" />
    </svg>
  );
}

function WindowMinimizeIcon() {
  return (
    <svg className="window-control-icon" viewBox="0 0 12 12" aria-hidden="true">
      <path d="M2.5 6.5h7" stroke="currentColor" strokeLinecap="round" />
    </svg>
  );
}

function WindowMaximizeIcon() {
  return (
    <svg className="window-control-icon" viewBox="0 0 12 12" aria-hidden="true">
      <rect
        x="3"
        y="3"
        width="6"
        height="6"
        fill="none"
        stroke="currentColor"
      />
    </svg>
  );
}

function WindowCloseIcon() {
  return (
    <svg className="window-control-icon" viewBox="0 0 12 12" aria-hidden="true">
      <path
        d="m3.25 3.25 5.5 5.5m0-5.5-5.5 5.5"
        stroke="currentColor"
        strokeLinecap="round"
      />
    </svg>
  );
}

function ZoomOutIcon() {
  return (
    <svg className="zoom-icon" viewBox="0 0 24 24" aria-hidden="true">
      <circle
        cx="10.5"
        cy="10.5"
        r="6.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
      />
      <path
        d="M7.5 10.5h6M15.5 15.5 21 21"
        fill="none"
        stroke="currentColor"
        strokeLinecap="round"
        strokeWidth="2"
      />
    </svg>
  );
}

function ZoomInIcon() {
  return (
    <svg className="zoom-icon" viewBox="0 0 24 24" aria-hidden="true">
      <circle
        cx="10.5"
        cy="10.5"
        r="6.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
      />
      <path
        d="M10.5 7.5v6M7.5 10.5h6M15.5 15.5 21 21"
        fill="none"
        stroke="currentColor"
        strokeLinecap="round"
        strokeWidth="2"
      />
    </svg>
  );
}

function screenStatusKind(screen: FlattenedScreen) {
  if (screen.role === "local") {
    return "local";
  }

  return screen.online ? "online" : "offline";
}

function screenStatusLabel(
  screen: FlattenedScreen,
  labels: { local: string; online: string; offline: string },
) {
  if (screen.role === "local") {
    return labels.local;
  }

  return screen.online ? labels.online : labels.offline;
}

function applyPeerPresence(layout: LayoutState, peers: LanPeer[]): LayoutState {
  const nextLayout = {
    ...layout,
    devices: layout.devices.map((device) => {
      if (device.role === "local") {
        return {
          ...device,
          online: true,
          inputReady: false,
          transportPort: layout.transportPort,
          quicPort: layout.quicPort,
          protocolVersion: 1,
        };
      }

      const peer = peers.find((candidate) =>
        deviceMatchesPeer(device, candidate),
      );
      if (!peer) {
        return {
          ...device,
          online: false,
          inputReady: false,
        };
      }

      return {
        ...device,
        online: peer.inputReady,
        inputReady: peer.inputReady,
        host: peer.ip || peer.host || device.host,
        transportPort: peer.transportPort,
        quicPort: peer.quicPort,
        transportPublicKey: peer.transportPublicKey,
        protocolVersion: peer.protocolVersion,
        platform: normalizePlatform(peer.platform),
      };
    }),
  };

  const autoDevices = peers
    .filter(
      (peer) =>
        peer.screens.length > 0 &&
        !nextLayout.devices.some((device) => deviceMatchesPeer(device, peer)),
    )
    .map((peer) => createDeviceFromPeer(nextLayout, peer));

  return {
    ...nextLayout,
    devices: [...nextLayout.devices, ...autoDevices],
  };
}

function upsertPeerDevice(
  layout: LayoutState,
  peer: LanPeer,
  alias = "",
): LayoutState {
  const existingIndex = layout.devices.findIndex(
    (device) =>
      device.id === peerDeviceId(peer) ||
      sameHost(device.host, peer.ip) ||
      sameHost(device.host, peer.host),
  );
  const existingDevice =
    existingIndex >= 0 ? layout.devices[existingIndex] : undefined;
  const nextDevice = createDeviceFromPeer(layout, peer, alias, existingDevice);
  const devices =
    existingIndex >= 0
      ? layout.devices.map((device, index) =>
          index === existingIndex ? nextDevice : device,
        )
      : [...layout.devices, nextDevice];

  return {
    ...layout,
    devices,
    inputMode: "control",
    activeDeviceId: nextDevice.id,
    selectedScreenId: nextDevice.screens[0]?.id ?? layout.selectedScreenId,
  };
}

function createDeviceFromPeer(
  layout: LayoutState,
  peer: LanPeer,
  alias = "",
  existingDevice?: Device,
): Device {
  const id = peerDeviceId(peer);

  return {
    id,
    name: alias || existingDevice?.name || peer.name,
    platform: normalizePlatform(peer.platform),
    host: peer.ip || peer.host,
    transportPort: peer.transportPort,
    quicPort: peer.quicPort,
    transportPublicKey: peer.transportPublicKey,
    protocolVersion: peer.protocolVersion,
    color: existingDevice?.color ?? nextDeviceColor(layout),
    online: peer.inputReady,
    inputReady: peer.inputReady,
    role: "client",
    source: "detected",
    screens: createScreensFromPeer(layout, id, peer.screens, existingDevice),
  };
}

function createScreensFromPeer(
  layout: LayoutState,
  deviceId: string,
  peerScreens: LanPeerScreen[],
  existingDevice?: Device,
): Screen[] {
  if (peerScreens.length === 0) {
    return [];
  }

  const localScreens =
    layout.devices.find((device) => device.role === "local")?.screens ?? [];
  const localBounds = getLayoutBounds(
    localScreens.length > 0 ? localScreens : [fallbackScreen()],
  );
  const peerMinX = Math.min(...peerScreens.map((screen) => screen.x));
  const peerMinY = Math.min(...peerScreens.map((screen) => screen.y));
  const startX = localBounds.maxX + REMOTE_SCREEN_GAP;

  return peerScreens.map((screen, index) => {
    const id = uniqueScreenId(deviceId, screen, index);
    const existingScreen = existingDevice?.screens.find(
      (candidate) => candidate.id === id,
    );

    return {
      id,
      deviceId,
      name: screen.name || `Display ${index + 1}`,
      x: existingScreen?.x ?? startX + (screen.x - peerMinX),
      y: existingScreen?.y ?? localBounds.minY + (screen.y - peerMinY),
      width: screen.width,
      height: screen.height,
      scale: screen.scale,
      isPrimary: screen.isPrimary,
    };
  });
}

function formatScreenCount(count: number, language: AppLanguage) {
  return language === "en"
    ? `${count} ${count === 1 ? "screen" : "screens"}`
    : `${count} 屏`;
}

function updateStatusLabel(status: UpdateStatus, ui: AppText) {
  switch (status) {
    case "checking":
      return ui.settings.updateChecking;
    case "available":
      return ui.settings.updateAvailable;
    case "current":
      return ui.settings.updateCurrent;
    case "installing":
      return ui.settings.updateInstalling;
    case "error":
      return ui.settings.updateFailed;
    default:
      return ui.settings.updateIdle;
  }
}

function uniqueScreenId(
  deviceId: string,
  screen: LanPeerScreen,
  index: number,
) {
  return `${deviceId}-${sanitizeId(screen.id || screen.name || `display-${index + 1}`)}`;
}

function nextDeviceColor(layout: LayoutState) {
  return DEVICE_COLORS[layout.devices.length % DEVICE_COLORS.length];
}

function peerDeviceId(peer: LanPeer) {
  return sanitizeId(peer.id || peer.name || peer.ip) || "peer-device";
}

function findPeerDevice(layout: LayoutState, peer: LanPeer) {
  return layout.devices.find((device) => deviceMatchesPeer(device, peer));
}

function deviceMatchesPeer(device: Device, peer: LanPeer) {
  return (
    device.id === peerDeviceId(peer) ||
    sameHost(device.host, peer.ip) ||
    sameHost(device.host, peer.host)
  );
}

function deviceBadgeStatusClass(device: Device) {
  if (device.role === "local") {
    return "device-badge-local";
  }

  return device.online ? "device-badge-online" : "device-badge-offline";
}

function deviceSourceLabel(device: Device, language: AppLanguage) {
  if (device.role === "local") {
    return TEXT[language].devices.local;
  }

  if (device.source === "detected") {
    return TEXT[language].devices.lan;
  }

  return TEXT[language].devices.manual;
}

function deviceSourceTagClass(device: Device) {
  if (device.role === "local") {
    return "tag-pill-local";
  }

  if (device.source === "detected") {
    return "tag-pill-lan";
  }

  return "tag-pill-manual";
}

function sameHost(value: string, host: string) {
  if (!value.trim() || !host.trim()) {
    return false;
  }

  const normalizedHost = host.trim().toLowerCase();

  return value
    .split("/")
    .some((part) => part.trim().toLowerCase() === normalizedHost);
}

function sanitizeId(value: string) {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function normalizePlatform(platform: string): Platform {
  if (platform === "windows" || platform === "macos") {
    return platform;
  }

  return "unknown";
}

function getBoardMetrics(
  bounds: { width: number; height: number },
  boardSize: { width: number; height: number },
  zoom: number,
): BoardMetrics {
  const fitScale = Math.max(
    Math.min(
      (boardSize.width * BOARD_FILL_RATIO) / bounds.width,
      (boardSize.height * BOARD_FILL_RATIO) / bounds.height,
    ),
    0.01,
  );
  const scale = fitScale * clampZoom(zoom);
  const contentWidth = bounds.width * scale;
  const contentHeight = bounds.height * scale;

  return {
    scale,
    offsetX: (boardSize.width - contentWidth) / 2,
    offsetY: (boardSize.height - contentHeight) / 2,
  };
}

export default App;
