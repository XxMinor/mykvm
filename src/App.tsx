import {
  type CSSProperties,
  type FormEvent,
  useEffect,
  useEffectEvent,
  useMemo,
  useRef,
  useState,
} from "react";
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import type { Theme } from "@tauri-apps/api/window";
import "./App.css";
import {
  checkForAppUpdate,
  confirmLanPairing,
  dismissPairingRequest,
  hideMainWindow,
  installInputService,
  installAppUpdate,
  isAutostartEnabled,
  isPortableMode,
  loadAppState,
  minimizeMainWindow,
  openLogDirectory,
  openRepositoryUrl,
  openUpdateReleasePage,
  probeLanPeer,
  readDiagnosticInfo,
  readInputServiceStatus,
  readPerformanceSample,
  readRuntimeStatus,
  relaunchApp,
  requestLanPairing,
  resetPairing,
  restartAsAdmin,
  saveLayout,
  sendFilesToDevice,
  setAutostart,
  scanLanPeers,
  startRuntime,
  startWindowDrag,
  stopRuntime,
  syncWindowChrome,
  toggleMaximizeMainWindow,
  uninstallInputService,
  writeClipboardText,
} from "./desktopApi";
import type { AppUpdateInfo } from "./desktopApi";
import { APP_VERSION, REPOSITORY_URL } from "./constants";
import { TEXT } from "./i18n";
import type { AppText } from "./i18n";
import {
  edgeSwitchHotkeyFromKeyboardEvent,
  formatEdgeSwitchHotkeyForDisplay,
  hotkeyFromKeyboardEvent,
  metaKeyLabelForPlatform,
} from "./hotkeyInput";
import {
  flattenScreens,
  getLayoutBounds,
  getScreenById,
  moveScreen,
  screenPositionOverlaps,
  snapOverlappingScreenPosition,
  snapScreenPosition,
} from "./layout";
import type { FlattenedScreen, LayoutBounds } from "./layout";
import type {
  AppStateSnapshot,
  DiagnosticInfo,
  LanPeer,
  LanPeerScreen,
  PerformanceSample,
  RuntimeStatus,
} from "./runtime";
import type {
  AppLanguage,
  Device,
  FileTransferProgress,
  LayoutState,
  MachineRole,
  ModifierMap,
  ModifierTarget,
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
const SERVICE_GUIDE_DISMISSED_KEY = "mykvm:service:dismissed";
type UpdateStatus =
  | "idle"
  | "checking"
  | "available"
  | "current"
  | "installing"
  | "error";
type InputServiceAction = "install" | "uninstall";

interface ServerPairingState {
  peer: LanPeer;
  host: string;
}

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

interface FileDropPosition {
  x: number;
  y: number;
}

type NativeFileDragPayload =
  | { type: "over"; position: FileDropPosition }
  | { type: "drop"; paths: string[]; position?: FileDropPosition }
  | { type: "cancel" }
  | { type: string; paths?: string[]; position?: FileDropPosition };

// A client is treated as "present" for the upgrade guard if it was announced
// within this window; discovery prunes stale peers, this is a belt-and-braces.
const CLIENT_ONLINE_WINDOW_MS = 20000;

function parseSemverCore(value: string): number[] | null {
  const core = value
    .trim()
    .replace(/^v/i, "")
    .split("-")[0]
    .split("+")[0];
  if (!core) {
    return null;
  }
  const parts = core.split(".").map((part) => Number.parseInt(part, 10));
  return parts.some((part) => Number.isNaN(part)) ? null : parts;
}

// True only when `version` is confidently at least `target` (same or newer
// core version; pre-release suffixes are ignored since they share a protocol).
// An empty or unparseable version returns false, so an uncertain client blocks
// the upgrade rather than risking a lockout.
function versionAtLeast(version: string, target: string): boolean {
  const a = parseSemverCore(version);
  const b = parseSemverCore(target);
  if (!a || !b) {
    return false;
  }
  const len = Math.max(a.length, b.length);
  for (let i = 0; i < len; i += 1) {
    const na = a[i] ?? 0;
    const nb = b[i] ?? 0;
    if (na !== nb) {
      return na > nb;
    }
  }
  return true;
}

function App() {
  const [snapshot, setSnapshot] = useState<AppStateSnapshot | null>(null);
  const [fileTransfers, setFileTransfers] = useState<FileTransferProgress[]>([]);
  const [dragState, setDragState] = useState<DragState | null>(null);
  const [boardSize, setBoardSize] = useState({
    width: DEFAULT_BOARD_WIDTH,
    height: DEFAULT_BOARD_HEIGHT,
  });
  const [isSaving, setIsSaving] = useState(false);
  const [isRuntimePending, setIsRuntimePending] = useState(false);
  const [isScanningLan, setIsScanningLan] = useState(false);
  const [scanCountdown, setScanCountdown] = useState(0);
  const [isAddingDevice, setIsAddingDevice] = useState(false);
  const [isPairingDevice, setIsPairingDevice] = useState(false);
  const [isDismissingPairing, setIsDismissingPairing] = useState(false);
  const [fileTransferPendingTargetId, setFileTransferPendingTargetId] =
    useState<string | null>(null);
  const [fileDragTargetId, setFileDragTargetId] = useState<string | null>(null);
  const [fileTransferMessage, setFileTransferMessage] = useState<string | null>(
    null,
  );
  const [isAdminRestartPending, setIsAdminRestartPending] = useState(false);
  const [isAppRelaunchPending, setIsAppRelaunchPending] = useState(false);
  const [isInputServicePending, setIsInputServicePending] = useState(false);
  const [inputServiceAction, setInputServiceAction] =
    useState<InputServiceAction | null>(null);
  const [boardZoom, setBoardZoom] = useState(1);
  const [manualDeviceName, setManualDeviceName] = useState("");
  const [manualDeviceHost, setManualDeviceHost] = useState("");
  const [serverPairing, setServerPairing] =
    useState<ServerPairingState | null>(null);
  const [serverPairingCode, setServerPairingCode] = useState("");
  const [serverPairingError, setServerPairingError] = useState<string | null>(
    null,
  );
  const [diagnosticInfo, setDiagnosticInfo] =
    useState<DiagnosticInfo | null>(null);
  const [diagnosticMessage, setDiagnosticMessage] = useState<string | null>(
    null,
  );
  const [isDiagnosticPending, setIsDiagnosticPending] = useState(false);
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
  const [serviceGuideDismissed, setServiceGuideDismissed] = useState(
    () => localStorage.getItem(SERVICE_GUIDE_DISMISSED_KEY) === "1",
  );
  const [isPortable, setIsPortable] = useState(false);
  const [autostartEnabled, setAutostartEnabled] = useState(false);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const [isCapturingEdgeSwitchHotkey, setIsCapturingEdgeSwitchHotkey] =
    useState(false);
  const [capturingDirection, setCapturingDirection] = useState<
    "left" | "right" | "up" | "down" | null
  >(null);
  const [activeTab, setActiveTab] = useState<WorkspaceTab>("layout");
  const [systemTheme, setSystemTheme] = useState<Exclude<ThemeMode, "system">>(
    () => getSystemTheme(),
  );
  const boardRef = useRef<HTMLDivElement | null>(null);
  const edgeSwitchHotkeyButtonRef = useRef<HTMLButtonElement | null>(null);
  const screenSwitchButtonRefs = useRef<
    Record<"left" | "right" | "up" | "down", HTMLButtonElement | null>
  >({ left: null, right: null, up: null, down: null });
  const fileDragTargetIdRef = useRef<string | null>(null);
  const fileTransferFallbackTargetIdRef = useRef<string | null>(null);
  const startupUpdateCheckStarted = useRef(false);
  const snapshotRef = useRef<AppStateSnapshot | null>(null);

  useEffect(() => {
    snapshotRef.current = snapshot;
  }, [snapshot]);

  useEffect(() => {
    fileTransferFallbackTargetIdRef.current =
      snapshot?.layout.fileTransferEnabled &&
      snapshot.layout.machineRole === "client"
        ? (preferredPairedControllerId(
            snapshot.layout,
            snapshot.runtime.discovery.peers,
          ) ?? null)
        : null;
  }, [snapshot]);

  useEffect(() => {
    fileDragTargetIdRef.current = fileDragTargetId;
  }, [fileDragTargetId]);

  useEffect(() => {
    if (!isTauri()) {
      return;
    }

    const allowNativeFileDrop = (event: DragEvent) => {
      event.preventDefault();
      if (event.type === "dragover" && event.dataTransfer) {
        const targetDeviceId =
          snapshotRef.current?.layout.fileTransferEnabled === false
            ? null
            : fileTransferTargetIdAtPosition(
                {
                  x: event.clientX,
                  y: event.clientY,
                },
                fileTransferFallbackTargetIdRef.current,
              );
        event.dataTransfer.dropEffect =
          targetDeviceId === null ? "none" : "copy";
      }
    };

    window.addEventListener("dragover", allowNativeFileDrop);
    window.addEventListener("drop", allowNativeFileDrop);

    return () => {
      window.removeEventListener("dragover", allowNativeFileDrop);
      window.removeEventListener("drop", allowNativeFileDrop);
    };
  }, []);

  useEffect(() => {
    if (!isTauri()) {
      return;
    }

    let active = true;
    let unlistenRuntime: (() => void) | null = null;

    void import("@tauri-apps/api/event")
      .then(({ listen }) =>
        listen<RuntimeStatus>("runtime-state-changed", ({ payload }) => {
          if (!active) {
            return;
          }

          setSnapshot((current) =>
            current
              ? {
                  ...current,
                  runtime: payload,
                }
              : current,
          );
        }),
      )
      .then((unlisten) => {
        if (active) {
          unlistenRuntime = unlisten;
          return;
        }

        unlisten();
      })
      .catch(() => {
        // Runtime events are an instant UI sync path; polling remains the fallback.
      });

    return () => {
      active = false;
      unlistenRuntime?.();
    };
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | null = null;
    const removalTimers = new Map<string, number>();

    void import("@tauri-apps/api/event")
      .then(({ listen }) =>
        listen<FileTransferProgress>("file-transfer-progress", ({ payload }) => {
          if (!active) {
            return;
          }
          setFileTransfers((current) => [
            ...current.filter((item) => item.transferId !== payload.transferId),
            payload,
          ]);
          if (payload.done) {
            const existing = removalTimers.get(payload.transferId);
            if (existing) {
              window.clearTimeout(existing);
            }
            const timer = window.setTimeout(() => {
              setFileTransfers((current) =>
                current.filter((item) => item.transferId !== payload.transferId),
              );
              removalTimers.delete(payload.transferId);
            }, 2500);
            removalTimers.set(payload.transferId, timer);
          }
        }),
      )
      .then((nextUnlisten) => {
        if (active) {
          unlisten = nextUnlisten;
          return;
        }
        nextUnlisten();
      })
      .catch(() => {
        // Progress is a nice-to-have overlay; its absence never blocks a transfer.
      });

    return () => {
      active = false;
      unlisten?.();
      removalTimers.forEach((timer) => window.clearTimeout(timer));
    };
  }, []);

  useEffect(() => {
    let active = true;

    loadAppState()
      .then((nextSnapshot) => {
        if (active) {
          setSnapshot(nextSnapshot);
        }
        // A controlled (client) machine usually has no local operator, so make
        // sure it relaunches itself after a reboot/upgrade and reconnects on its
        // own (pairing is persisted). Enable launch-at-startup once per install,
        // so a later manual toggle is still respected.
        if (
          active &&
          nextSnapshot.layout.machineRole === "client" &&
          !localStorage.getItem("mykvm.clientAutostartInit")
        ) {
          localStorage.setItem("mykvm.clientAutostartInit", "1");
          void setAutostart(true).catch(() => {});
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
    let active = true;

    isAutostartEnabled()
      .then((enabled) => {
        if (active) {
          setAutostartEnabled(enabled);
        }
      })
      .catch(() => {
        // Launch-at-startup state is best-effort; ignore if unavailable.
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

      // Refresh runtime + layout together: device online/inputReady status
      // lives in layout, and without re-fetching it the UI shows stale
      // "offline" even after discovery has found the peer again.
      Promise.all([readRuntimeStatus(), loadAppState()])
        .then(([nextRuntime, nextSnapshot]) => {
          if (!active) {
            return;
          }

          setSnapshot((current) =>
            current
              ? { ...nextSnapshot, runtime: nextRuntime }
              : nextSnapshot,
          );

          const currentSnapshot = snapshotRef.current;
          if (
            currentSnapshot?.layout.machineRole === "client" &&
            currentSnapshot.layout.pairedControllers.length === 0 &&
            nextRuntime.pairing.state === "paired"
          ) {
            void loadAppState()
              .then((pairingSnapshot) => {
                if (active) {
                  setSnapshot({
                    ...pairingSnapshot,
                    runtime: nextRuntime,
                  });
                }
              })
              .catch(() => {});
          }

          // Keep a persistent blocking condition visible. The inject stage holds
          // the receiver-side reason keys/clicks get dropped (macOS Accessibility
          // missing, or Secure Keyboard Entry on); it is otherwise never shown.
          if (nextRuntime.started) {
            if (nextRuntime.capture.state === "error") {
              setErrorMessage(nextRuntime.capture.detail);
            } else if (nextRuntime.inject.state === "error") {
              setErrorMessage(nextRuntime.inject.detail);
            }
          }
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
  // The latest released version: the newer one when an update is available,
  // otherwise the current build once a check confirms we're up to date.
  const latestVersionLabel = availableUpdate
    ? availableUpdate.version
    : updateStatus === "current"
      ? APP_VERSION
      : null;
  const hasActionableUpdate =
    Boolean(availableUpdate) && !isAvailableUpdateDismissed;
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
  const localPlatform =
    runtime?.discovery.localPeer.platform.toLowerCase() ??
    navigator.platform.toLowerCase();
  const metaKeyLabel = metaKeyLabelForPlatform(localPlatform);
  const usesWindowsChrome = localPlatform.includes("win");
  const usesCustomChrome = usesWindowsChrome;
  const inputServiceInstalled = Boolean(runtime?.inputService.installed);
  // First-run nudge: a Windows client with no lock-screen service can't be
  // controlled once it locks (it has no local keyboard/mouse to unlock itself),
  // so prompt to install it up front rather than burying it in Settings.
  const showServiceGuide =
    usesWindowsChrome &&
    machineRole === "client" &&
    hasLoadedSnapshot &&
    !inputServiceInstalled &&
    !serviceGuideDismissed;
  const inputServiceReady =
    inputServiceInstalled &&
    Boolean(runtime?.inputService.running) &&
    Boolean(runtime?.inputService.pipeAvailable);
  const inputServiceStatusLabel = inputServiceReady
    ? ui.settings.inputServiceReady
    : inputServiceInstalled
      ? ui.settings.inputServiceInstalledStatus
      : ui.settings.inputServiceNeedsInstall;
  const canManageInputService =
    usesWindowsChrome &&
    machineRole === "client" &&
    Boolean(runtime?.privilege.isElevated);
  const hasBlockingOverlay =
    Boolean(inputServiceAction) ||
    Boolean(errorMessage) ||
    isScanningLan ||
    Boolean(serverPairing) ||
    runtime?.pairing.state === "requested";
  const isPerformanceActive =
    currentTab === "settings" &&
    Boolean(layout?.performanceMonitor) &&
    !hasBlockingOverlay;
  const chromeClassName = usesWindowsChrome
    ? "custom-chrome custom-chrome-windows"
    : "";
  const shellClassName = `app-shell ${chromeClassName} theme-${resolvedTheme}`;

  function renderServiceGuide() {
    if (!showServiceGuide) {
      return null;
    }
    return (
      <div className="service-guide-banner" role="alert">
        <div className="service-guide-copy">
          <strong>{ui.settings.inputServicePromptTitle}</strong>
          <span>{ui.settings.inputServicePromptCopy}</span>
        </div>
        <div className="service-guide-actions">
          <button
            type="button"
            className="primary-button compact-button"
            onClick={() => void handleInstallInputService()}
            disabled={isInputServicePending}
          >
            {ui.settings.installInputService}
          </button>
          <button
            type="button"
            className="ghost-button compact-button"
            onClick={dismissServiceGuide}
            disabled={isInputServicePending}
          >
            {ui.settings.inputServiceLater}
          </button>
        </div>
      </div>
    );
  }

  function renderFileTransferToasts() {
    if (fileTransfers.length === 0) {
      return null;
    }
    return (
      <div className="file-transfer-toasts" role="status" aria-live="polite">
        {fileTransfers.map((transfer) => {
          const percent =
            transfer.totalBytes > 0
              ? Math.min(
                  100,
                  Math.round((transfer.sentBytes / transfer.totalBytes) * 100),
                )
              : transfer.done
                ? 100
                : 0;
          return (
            <div
              key={transfer.transferId}
              className={`file-transfer-toast${transfer.done ? " is-done" : ""}`}
            >
              <div className="file-transfer-toast-row">
                <span className="file-transfer-toast-name" title={transfer.fileName}>
                  {transfer.fileName}
                </span>
                <span className="file-transfer-toast-percent">
                  {transfer.done ? ui.devices.fileTransferSent : `${percent}%`}
                </span>
              </div>
              <div className="file-transfer-toast-track">
                <div
                  className="file-transfer-toast-fill"
                  style={{ width: `${percent}%` }}
                />
              </div>
              <div className="file-transfer-toast-meta">
                {`${
                  transfer.done
                    ? ui.devices.fileTransferSent
                    : ui.devices.fileTransferSending
                } · ${transfer.targetName}`}
              </div>
            </div>
          );
        })}
      </div>
    );
  }

  function renderWindowTitlebar() {
    if (!usesCustomChrome) {
      return null;
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
          if (!active) {
            return;
          }

          if (!result.available || !result.update) {
            // Up to date: the updater returns nothing, so the current build is
            // the latest release. Record that so "Latest version" can show it.
            setAvailableUpdate(null);
            setUpdateStatus("current");
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

  useEffect(() => {
    if (currentTab !== "settings" || !hasLoadedSnapshot) {
      return;
    }

    let active = true;
    const refresh = () => {
      readDiagnosticInfo()
        .then((info) => {
          if (active) {
            setDiagnosticInfo(info);
          }
        })
        .catch(() => {
          // Diagnostics are helpful but not required for the settings screen.
        });
    };

    refresh();
    const intervalId = window.setInterval(refresh, 5000);

    return () => {
      active = false;
      window.clearInterval(intervalId);
    };
  }, [currentTab, hasLoadedSnapshot]);

  async function refreshDiagnostics() {
    setIsDiagnosticPending(true);
    setDiagnosticMessage(null);

    try {
      setDiagnosticInfo(await readDiagnosticInfo());
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
    } finally {
      setIsDiagnosticPending(false);
    }
  }

  async function copyDiagnostics() {
    setIsDiagnosticPending(true);
    setDiagnosticMessage(null);

    try {
      const info = diagnosticInfo ?? (await readDiagnosticInfo());
      setDiagnosticInfo(info);
      await navigator.clipboard
        .writeText(info.report)
        .catch(() => writeClipboardText(info.report));
      setDiagnosticMessage(ui.settings.diagnosticsCopied);
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.writeClipboard,
      );
    } finally {
      setIsDiagnosticPending(false);
    }
  }

  async function handleOpenLogDirectory() {
    setIsDiagnosticPending(true);
    setDiagnosticMessage(null);

    try {
      await openLogDirectory();
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
    } finally {
      setIsDiagnosticPending(false);
    }
  }

  async function persistLayout(nextLayout: LayoutState) {
    setIsSaving(true);
    try {
      const persistedSnapshot = await saveLayout(nextLayout);
      setSnapshot(persistedSnapshot);
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.saveLayout,
      );
    } finally {
      setIsSaving(false);
    }
  }

  const sendDroppedFiles = useEffectEvent(
    async (targetDeviceId: string, paths: string[]) => {
      if (snapshotRef.current?.layout.fileTransferEnabled === false) {
        return;
      }

      const transferPaths = paths.filter((path) => path.trim().length > 0);
      if (transferPaths.length === 0) {
        return;
      }

      setFileTransferPendingTargetId(targetDeviceId);
      setFileTransferMessage(null);
      setErrorMessage(null);

      try {
        const summary = await sendFilesToDevice(targetDeviceId, transferPaths);
        setFileTransferMessage(formatFileTransferSummary(summary, language));
      } catch (error: unknown) {
        setErrorMessage(
          error instanceof Error ? error.message : ui.errors.fileTransfer,
        );
      } finally {
        setFileTransferPendingTargetId(null);
      }
    },
  );

  useEffect(() => {
    if (!isTauri()) {
      return;
    }

    let disposed = false;
    let unlisten: (() => void) | null = null;

    getCurrentWebview()
      .onDragDropEvent((event) => {
        const payload = event.payload as NativeFileDragPayload;
        if (snapshotRef.current?.layout.fileTransferEnabled === false) {
          setFileDragTargetId(null);
          return;
        }

        if (payload.type === "over") {
          setFileDragTargetId(
            fileTransferTargetIdAtPosition(
              payload.position,
              fileTransferFallbackTargetIdRef.current,
            ),
          );
          return;
        }

        if (payload.type === "drop") {
          const targetDeviceId =
            fileTransferTargetIdAtPosition(
              payload.position,
              fileTransferFallbackTargetIdRef.current,
            ) ??
            fileDragTargetIdRef.current;
          setFileDragTargetId(null);
          if (targetDeviceId && payload.paths?.length) {
            void sendDroppedFiles(targetDeviceId, payload.paths);
          }
          return;
        }

        setFileDragTargetId(null);
      })
      .then((nextUnlisten) => {
        if (disposed) {
          nextUnlisten();
        } else {
          unlisten = nextUnlisten;
        }
      })
      .catch(() => {
        // File drag-and-drop is a native desktop affordance; the rest of the app
        // should keep working if the webview cannot register this listener.
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const endDrag = useEffectEvent(() => {
    if (layout && dragState) {
      const screen = getScreenById(layout, dragState.screenId);
      const currentPosition = screen ? { x: screen.x, y: screen.y } : null;
      const startPosition = { x: dragState.startX, y: dragState.startY };
      const nextLayout =
        screen &&
        currentPosition &&
        screenPositionOverlaps(layout, dragState.screenId, currentPosition)
          ? moveScreen(
              layout,
              dragState.screenId,
              snapOverlappingScreenPosition(
                layout,
                dragState.screenId,
                currentPosition,
                startPosition,
              ) ?? startPosition,
            )
          : layout;

      setSnapshot((current) =>
        current ? { ...current, layout: nextLayout } : current,
      );
      void persistLayout(nextLayout);
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

    setSnapshot((current) => {
      if (!current) {
        return current;
      }

      const nextPosition = snapScreenPosition(current.layout, dragState.screenId, {
        x: dragState.startX + deltaX,
        y: dragState.startY + deltaY,
      });

      return {
        ...current,
        layout: moveScreen(current.layout, dragState.screenId, nextPosition),
      };
    });
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
      if (nextStarted && nextRuntime.capture.state === "error") {
        setErrorMessage(nextRuntime.capture.detail);
      } else if (nextStarted && nextRuntime.inject.state === "error") {
        // On a client the capture stage is idle; the blocking reason (macOS
        // Accessibility not granted, or Secure Keyboard Entry intercepting every
        // key) lives in the inject stage, so surface that too.
        setErrorMessage(nextRuntime.inject.detail);
      }
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

  function updateInputServiceStatus(
    inputService: RuntimeStatus["inputService"],
  ) {
    setSnapshot((current) =>
      current
        ? {
            ...current,
            runtime: {
              ...current.runtime,
              inputService,
            },
          }
        : current,
    );
  }

  async function refreshInputService() {
    if (!isTauri()) {
      return;
    }

    setIsInputServicePending(true);
    setErrorMessage(null);
    try {
      updateInputServiceStatus(await readInputServiceStatus());
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
    } finally {
      setIsInputServicePending(false);
    }
  }

  async function handleInstallInputService() {
    setIsInputServicePending(true);
    setErrorMessage(null);
    try {
      updateInputServiceStatus(await installInputService());
      setInputServiceAction(null);
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
    } finally {
      setIsInputServicePending(false);
    }
  }

  async function handleUninstallInputService() {
    setIsInputServicePending(true);
    setErrorMessage(null);
    try {
      updateInputServiceStatus(await uninstallInputService());
      setInputServiceAction(null);
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
    } finally {
      setIsInputServicePending(false);
    }
  }

  async function scanLan() {
    setErrorMessage(null);
    setScanCountdown(6);
    setIsScanningLan(true);

    // Let the browser paint the modal before we hit the blocking backend call,
    // otherwise the IPC freezes the UI thread and the modal never shows.
    await new Promise((resolve) => requestAnimationFrame(resolve));
    await new Promise((resolve) => setTimeout(resolve, 0));

    const countdownTimer = setInterval(() => {
      setScanCountdown((remaining) => (remaining > 0 ? remaining - 1 : 0));
    }, 1000);
    const startedAt = Date.now();

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
      clearInterval(countdownTimer);
      const elapsed = Date.now() - startedAt;
      const minDisplayMs = 2500;
      if (elapsed < minDisplayMs) {
        setTimeout(() => {
          setScanCountdown(0);
          setIsScanningLan(false);
        }, minDisplayMs - elapsed);
      } else {
        setScanCountdown(0);
        setIsScanningLan(false);
      }
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

      const nextPosition = snapScreenPosition(layoutState, currentScreen.id, {
        x: currentScreen.x + delta.x,
        y: currentScreen.y + delta.y,
      });

      if (screenPositionOverlaps(layoutState, currentScreen.id, nextPosition)) {
        return layoutState;
      }

      return moveScreen(layoutState, currentScreen.id, nextPosition);
    });
  }

  function setClipboardSync(clipboardSync: boolean) {
    updateLayout((layoutState) => ({
      ...layoutState,
      clipboardSync,
    }));
  }

  function setFileTransferEnabled(fileTransferEnabled: boolean) {
    updateLayout((layoutState) => ({
      ...layoutState,
      fileTransferEnabled,
    }));
  }

  function setModifierRemap(modifierRemap: boolean) {
    updateLayout((layoutState) => ({
      ...layoutState,
      modifierRemap,
    }));
  }

  function setModifierMapTarget(key: keyof ModifierMap, value: ModifierTarget) {
    updateLayout((layoutState) => ({
      ...layoutState,
      modifierMap: { ...layoutState.modifierMap, [key]: value },
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

  function setEdgeSwitchHotkey(edgeSwitchHotkey: string) {
    updateLayout((layoutState) => ({
      ...layoutState,
      edgeSwitchHotkey,
    }));
  }

  function commitEdgeSwitchHotkey(value: string) {
    setEdgeSwitchHotkey(normalizeEdgeSwitchHotkeyInput(value));
  }

  const captureEdgeSwitchHotkey = useEffectEvent((event: KeyboardEvent) => {
    const hotkey = edgeSwitchHotkeyFromKeyboardEvent(event, metaKeyLabel);
    if (!hotkey) {
      return;
    }

    event.preventDefault();
    event.stopPropagation();
    commitEdgeSwitchHotkey(hotkey);
    setIsCapturingEdgeSwitchHotkey(false);
  });

  useEffect(() => {
    if (!isCapturingEdgeSwitchHotkey) {
      return;
    }

    const cancelIfOutsideRecorder = (event: Event) => {
      const target = event.target;
      const button = edgeSwitchHotkeyButtonRef.current;
      if (target instanceof Node && button?.contains(target)) {
        return;
      }
      setIsCapturingEdgeSwitchHotkey(false);
    };
    const cancelRecording = () => setIsCapturingEdgeSwitchHotkey(false);

    window.addEventListener("keydown", captureEdgeSwitchHotkey, true);
    document.addEventListener("pointerdown", cancelIfOutsideRecorder, true);
    document.addEventListener("focusin", cancelIfOutsideRecorder, true);
    window.addEventListener("blur", cancelRecording);
    return () => {
      window.removeEventListener("keydown", captureEdgeSwitchHotkey, true);
      document.removeEventListener(
        "pointerdown",
        cancelIfOutsideRecorder,
        true,
      );
      document.removeEventListener("focusin", cancelIfOutsideRecorder, true);
      window.removeEventListener("blur", cancelRecording);
    };
  }, [isCapturingEdgeSwitchHotkey]);

  function setScreenSwitchHotkey(
    direction: "left" | "right" | "up" | "down",
    value: string,
  ) {
    updateLayout((layoutState) => ({
      ...layoutState,
      screenSwitchHotkeys: {
        ...layoutState.screenSwitchHotkeys,
        [direction]: value,
      },
    }));
  }

  const captureScreenSwitchHotkey = useEffectEvent((event: KeyboardEvent) => {
    const direction = capturingDirection;
    if (!direction) {
      return;
    }
    const hotkey = hotkeyFromKeyboardEvent(event, metaKeyLabel);
    if (!hotkey) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    setScreenSwitchHotkey(direction, hotkey);
    setCapturingDirection(null);
  });

  useEffect(() => {
    if (!capturingDirection) {
      return;
    }
    const cancelIfOutsideRecorder = (event: Event) => {
      const target = event.target;
      const button = screenSwitchButtonRefs.current[capturingDirection];
      if (target instanceof Node && button?.contains(target)) {
        return;
      }
      setCapturingDirection(null);
    };
    const cancelRecording = () => setCapturingDirection(null);

    window.addEventListener("keydown", captureScreenSwitchHotkey, true);
    document.addEventListener("pointerdown", cancelIfOutsideRecorder, true);
    document.addEventListener("focusin", cancelIfOutsideRecorder, true);
    window.addEventListener("blur", cancelRecording);
    return () => {
      window.removeEventListener("keydown", captureScreenSwitchHotkey, true);
      document.removeEventListener(
        "pointerdown",
        cancelIfOutsideRecorder,
        true,
      );
      document.removeEventListener("focusin", cancelIfOutsideRecorder, true);
      window.removeEventListener("blur", cancelRecording);
    };
  }, [capturingDirection]);

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

  async function handleRelaunchApp() {
    setIsAppRelaunchPending(true);

    try {
      await relaunchApp();
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
      setIsAppRelaunchPending(false);
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

  async function handleResetPairing() {
    setErrorMessage(null);
    try {
      const nextSnapshot = await resetPairing();
      setSnapshot(nextSnapshot);
    } catch (error: unknown) {
      setErrorMessage(formatUnknownError(error, ui.errors.pairingFailed));
    }
  }

  async function handleSetAutostart(enabled: boolean) {
    if (enabled === autostartEnabled) {
      return;
    }
    setErrorMessage(null);
    try {
      const next = await setAutostart(enabled);
      setAutostartEnabled(next);
    } catch (error: unknown) {
      setErrorMessage(
        error instanceof Error ? error.message : ui.errors.updateRuntime,
      );
    }
  }

  async function handleAddManualDevice(
    event: FormEvent<HTMLFormElement>,
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

      if (peer.pairingRequired) {
        await beginPairing(host);
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

    if (peer.pairingRequired) {
      void beginPairing(peer.ip || peer.host);
      return;
    }

    updateLayout((layoutState) => {
      return upsertPeerDevice(layoutState, peer);
    });
  }

  async function beginPairing(hostInput: string) {
    const host = hostInput.trim();
    if (!host) {
      setErrorMessage(ui.errors.manualHostRequired);
      return;
    }

    setIsPairingDevice(true);
    setErrorMessage(null);

    try {
      const challengePeer = await requestLanPairing(host);
      setServerPairing({ peer: challengePeer, host });
      setServerPairingCode("");
      setServerPairingError(null);
    } catch (error: unknown) {
      setErrorMessage(formatUnknownError(error, ui.errors.pairingFailed));
    } finally {
      setIsPairingDevice(false);
    }
  }

  // Re-pair an already-added device (server side). The client shows the code
  // on its own screen, so this works even when the client has no keyboard or
  // mouse and the cursor can no longer cross to it.
  async function handleRepairDevice(device: Device) {
    const host = (device.host || "").split("/").pop()?.trim() || "";
    if (!host) {
      setErrorMessage(ui.errors.manualHostRequired);
      return;
    }
    await beginPairing(host);
  }

  async function confirmPairing(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!serverPairing) {
      return;
    }

    const code = serverPairingCode.trim();
    if (!code) {
      setServerPairingError(ui.errors.pairingCodeRequired);
      return;
    }

    setIsPairingDevice(true);
    setServerPairingError(null);

    try {
      const pairedPeer = await confirmLanPairing(serverPairing.host, code);
      if (pairedPeer.screens.length === 0) {
        setServerPairingError(
          `${pairedPeer.name}: ${ui.errors.connectedWithoutScreens}`,
        );
        return;
      }
      updateLayout((layoutState) => upsertPeerDevice(layoutState, pairedPeer));
      setServerPairing(null);
      setServerPairingCode("");
      setServerPairingError(null);
    } catch (error: unknown) {
      setServerPairingError(formatUnknownError(error, ui.errors.pairingFailed));
    } finally {
      setIsPairingDevice(false);
    }
  }

  async function dismissClientPairing() {
    setIsDismissingPairing(true);
    setErrorMessage(null);

    try {
      const nextRuntime = await dismissPairingRequest();
      setSnapshot((current) =>
        current
          ? {
              ...current,
              runtime: nextRuntime,
            }
          : current,
      );
    } catch (error: unknown) {
      setErrorMessage(formatUnknownError(error, ui.errors.pairingFailed));
    } finally {
      setIsDismissingPairing(false);
    }
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
      setUpdateMessage(formatUnknownError(error, ui.errors.checkUpdate));
    }
  }

  async function installDesktopUpdate() {
    if (!availableUpdate || updateStatus === "installing") {
      return;
    }

    // Guard against a self-inflicted lockout. This machine has the keyboard and
    // mouse; it drives the clients. If it upgrades to a build whose transport
    // protocol the still-old clients cannot speak, they drop offline — and since
    // a client has no local input of its own, it can no longer be driven to
    // update. Require every online client to already be on the target version.
    const now = Date.now();
    const outdatedClients = (snapshot?.runtime.discovery.peers ?? []).filter(
      (peer) =>
        peer.inputReady &&
        !peer.upgrading &&
        now - peer.lastSeenMs < CLIENT_ONLINE_WINDOW_MS &&
        !versionAtLeast(peer.appVersion, availableUpdate.version),
    );
    if (outdatedClients.length > 0) {
      const list = outdatedClients
        .map((peer) => `${peer.name || peer.host} (v${peer.appVersion || "?"})`)
        .join("、");
      setUpdateStatus("available");
      setUpdateMessage(`${ui.settings.updateBlockedByClients} ${list}`);
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
      const errorText = formatUpdaterError(
        error,
        ui.errors.installUpdate,
        ui.errors.updateSignatureMismatch,
      );
      setUpdateStatus("error");
      setUpdateMessage(`${errorText} ${ui.settings.updateFallback}`);
    }
  }

  function dismissServiceGuide() {
    localStorage.setItem(SERVICE_GUIDE_DISMISSED_KEY, "1");
    setServiceGuideDismissed(true);
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

  function renderErrorDialog(message: string) {
    const canRelaunch = shouldOfferPermissionRelaunch(message);

    return (
      <div className="error-dialog-backdrop" role="presentation">
        <section className="error-dialog" role="alertdialog" aria-modal="true">
          <button
            type="button"
            className="error-dialog-close"
            onClick={() => setErrorMessage(null)}
            aria-label={ui.errors.close}
            title={ui.errors.close}
          >
            <WindowCloseIcon />
          </button>
          <h2>{ui.errors.title}</h2>
          <p className="error-dialog-message">{message}</p>
          {canRelaunch ? (
            <button
              type="button"
              className="error-dialog-action"
              onClick={() => void handleRelaunchApp()}
              disabled={isAppRelaunchPending}
            >
              <RestartIcon />
              <span>
                {isAppRelaunchPending
                  ? ui.common.relaunching
                  : ui.common.relaunch}
              </span>
            </button>
          ) : null}
        </section>
      </div>
    );
  }

  function renderInfoBanner(message: string) {
    return (
      <div className="info-banner" role="status">
        <span>{message}</span>
      </div>
    );
  }

  function renderInputServicePrompt(action: InputServiceAction) {
    const title =
      action === "uninstall"
        ? ui.settings.uninstallInputServicePromptTitle
        : ui.settings.inputServicePromptTitle;
    const copy =
      action === "uninstall"
        ? ui.settings.uninstallInputServicePromptCopy
        : ui.settings.inputServicePromptCopy;
    const label =
      action === "uninstall"
        ? ui.settings.uninstallInputService
        : ui.settings.installInputService;
    const confirmAction =
      action === "uninstall"
        ? handleUninstallInputService
        : handleInstallInputService;

    return (
      <div className="pairing-modal-backdrop" role="presentation">
        <section
          className="pairing-modal pairing-modal-client"
          role="dialog"
          aria-modal="true"
          aria-labelledby="input-service-prompt-title"
        >
          <button
            type="button"
            className="pairing-close-button"
            onClick={() => setInputServiceAction(null)}
            disabled={isInputServicePending}
            title={ui.common.cancel}
            aria-label={ui.common.cancel}
          >
            <WindowCloseIcon />
          </button>
          <div>
            <p className="eyebrow">{ui.settings.inputServiceEyebrow}</p>
            <h2 id="input-service-prompt-title">{title}</h2>
            <p>{copy}</p>
          </div>
          <div className="pairing-actions">
            <button
              type="button"
              className={
                action === "uninstall"
                  ? "secondary-button compact-button danger-button"
                  : "primary-button compact-button"
              }
              onClick={() => void confirmAction()}
              disabled={isInputServicePending}
            >
              {isInputServicePending ? ui.common.pending : label}
            </button>
            <button
              type="button"
              className="secondary-button compact-button"
              onClick={() => setInputServiceAction(null)}
              disabled={isInputServicePending}
            >
              {ui.common.cancel}
            </button>
          </div>
        </section>
      </div>
    );
  }

  if (!snapshot || !layout || !runtime || !displayLayout) {
    return (
      <main className={shellClassName}>
        {renderWindowTitlebar()}
        <section className="loading-panel">
          <p className="eyebrow">mykvm</p>
          <h1>{ui.loading.title}</h1>
          <p>{ui.loading.copy}</p>
          {errorMessage ? renderErrorDialog(errorMessage) : null}
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
  const fileTransferEnabled = layout.fileTransferEnabled;
  const addedOnlyDevices = displayLayout.devices.filter(
    (device) => !lanPeers.some((peer) => deviceMatchesPeer(layout, device, peer)),
  );
  const serverFileTransferTargets = displayLayout.devices.filter(
    (device) =>
      device.role !== "local" &&
      device.online &&
      device.inputReady &&
      device.transportPublicKey.trim().length > 0,
  );
  const serverFileTransferTargetIds = new Set(
    serverFileTransferTargets.map((device) => device.id),
  );

  function screenFileTransferTargetId(screen: FlattenedScreen) {
    if (
      !fileTransferEnabled ||
      machineRole !== "server" ||
      screen.role === "local" ||
      !screen.online ||
      !screen.inputReady ||
      !serverFileTransferTargetIds.has(screen.deviceId)
    ) {
      return null;
    }

    return screen.deviceId;
  }

  function renderAddedDeviceActions(device: Device) {
    if (device.role === "local") {
      return null;
    }

    return (
      <>
        <button
          type="button"
          className="secondary-button compact-button"
          onClick={() => void handleRepairDevice(device)}
          disabled={isPairingDevice}
        >
          {ui.devices.repair}
        </button>
        <button
          type="button"
          className="secondary-button compact-button danger-button"
          onClick={() => handleRemoveDevice(device.id)}
        >
          {ui.common.remove}
        </button>
      </>
    );
  }

  return (
    <main className={shellClassName}>
      {renderWindowTitlebar()}
      {renderFileTransferToasts()}
      {renderServiceGuide()}
      <header className="app-header">
        <div className="brand-lockup">
          <span className="brand-mark">mk</span>
          <div className="brand-copy">
            <div className="brand-title-row">
              <strong>MyKVM</strong>
              {hasActionableUpdate && availableUpdate ? (
                <button
                  type="button"
                  className="brand-update-badge"
                  onClick={() => setActiveTab("settings")}
                  title={`${ui.settings.updateAvailable}: v${availableUpdate.version}`}
                  aria-label={`${ui.settings.updateAvailable}: v${availableUpdate.version}`}
                >
                  <DownloadIcon />
                </button>
              ) : null}
            </div>
            <span className="brand-subtitle">
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

      {errorMessage ? renderErrorDialog(errorMessage) : null}
      {fileTransferMessage ? renderInfoBanner(fileTransferMessage) : null}
      {inputServiceAction ? renderInputServicePrompt(inputServiceAction) : null}

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
                const fileTargetId = screenFileTransferTargetId(screen);
                const isFileTargetHovering =
                  fileTargetId !== null && fileDragTargetId === fileTargetId;
                const isFileTargetPending =
                  fileTargetId !== null &&
                  fileTransferPendingTargetId === fileTargetId;

                return (
                  <button
                    key={screen.id}
                    type="button"
                    className={`screen-rect ${layout.selectedScreenId === screen.id ? "selected" : ""} ${
                      dragState?.screenId === screen.id ? "dragging" : ""
                    } ${fileTargetId ? "file-target" : ""} ${
                      isFileTargetHovering ? "file-drag-over" : ""
                    } ${isFileTargetPending ? "file-transfer-pending" : ""} ${statusKind}`}
                    data-file-transfer-target={
                      fileTargetId && !fileTransferPendingTargetId
                        ? fileTargetId
                        : undefined
                    }
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
                  disabled={isAddingDevice || isPairingDevice}
                >
                  {isAddingDevice || isPairingDevice
                    ? ui.common.connecting
                    : ui.common.add}
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
                          className="device-badge device-badge-online"
                        />
                        <div>
                          <div className="connection-title">
                            <strong>{addedDevice?.name ?? peer.name}</strong>
                            <div className="connection-tags">
                              <span className="tag-pill tag-pill-lan">
                                {ui.devices.lan}
                              </span>
                              {addedDevice?.upgrading ? (
                                <span className="tag-pill tag-pill-upgrading">
                                  {ui.common.upgrading}
                                </span>
                              ) : null}
                              {!peer.inputReady ? (
                                <span className="tag-pill tag-pill-warning">
                                  {ui.devices.inputNotReady}
                                </span>
                              ) : null}
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
                            disabled={peer.screens.length === 0 || isPairingDevice}
                          >
                            {peer.screens.length === 0
                              ? ui.devices.noScreens
                              : peer.pairingRequired
                                ? ui.devices.pair
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
                            {device.upgrading ? (
                              <span className="tag-pill tag-pill-upgrading">
                                {ui.common.upgrading}
                              </span>
                            ) : null}
                            {device.role !== "local" &&
                            device.online &&
                            !device.inputReady ? (
                              <span className="tag-pill tag-pill-warning">
                                {ui.devices.inputNotReady}
                              </span>
                            ) : null}
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
                <div className="settings-control-row">
                  <span>{ui.settings.autostart}</span>
                  <div className="segmented-control">
                    <button
                      type="button"
                      className={autostartEnabled ? "active" : ""}
                      onClick={() => void handleSetAutostart(true)}
                    >
                      {ui.settings.autostartOn}
                    </button>
                    <button
                      type="button"
                      className={!autostartEnabled ? "active" : ""}
                      onClick={() => void handleSetAutostart(false)}
                    >
                      {ui.settings.autostartOff}
                    </button>
                  </div>
                </div>
                {machineRole === "server" ? (
                  <>
                    <div className="settings-control-row">
                      <span>{ui.settings.edgeSwitchHotkey}</span>
                      <button
                        type="button"
                        ref={edgeSwitchHotkeyButtonRef}
                        className={`hotkey-recorder-button ${
                          isCapturingEdgeSwitchHotkey ? "recording" : ""
                        }`}
                        aria-pressed={isCapturingEdgeSwitchHotkey}
                        onClick={() =>
                          setIsCapturingEdgeSwitchHotkey(
                            (recording) => !recording,
                          )
                        }
                      >
                        {isCapturingEdgeSwitchHotkey
                          ? ui.settings.edgeSwitchHotkeyRecording
                          : renderHotkeyTags(
                              formatEdgeSwitchHotkeyForDisplay(
                                layout.edgeSwitchHotkey,
                                metaKeyLabel,
                              ),
                              localPlatform,
                            )}
                      </button>
                    </div>
                    <div className="settings-control-row">
                      <span>
                        {ui.settings.screenSwitchTitle}
                        <span className="info-tooltip-host" tabIndex={0}>
                          ⓘ
                          <span className="info-tooltip">
                            {ui.settings.screenSwitchCopy}
                          </span>
                        </span>
                      </span>
                      <div className="screen-switch-hotkeys">
                        {(["left", "right", "up", "down"] as const).map(
                          (dir) => (
                            <button
                              key={dir}
                              type="button"
                              ref={(el) => {
                                screenSwitchButtonRefs.current[dir] = el;
                              }}
                              className={`hotkey-recorder-button ${
                                capturingDirection === dir ? "recording" : ""
                              }`}
                              aria-pressed={capturingDirection === dir}
                              onClick={() =>
                                setCapturingDirection((current) =>
                                  current === dir ? null : dir,
                                )
                              }
                            >
                              {capturingDirection === dir
                                ? ui.settings.screenSwitchRecording
                                : renderHotkeyTags(
                                    formatEdgeSwitchHotkeyForDisplay(
                                      layout.screenSwitchHotkeys[dir],
                                      metaKeyLabel,
                                    ),
                                    localPlatform,
                                  )}
                            </button>
                          ),
                        )}
                      </div>
                    </div>
                  </>
                ) : null}
                <div className="settings-control-row">
                  <span>{ui.settings.clipboard}</span>
                  <div className="segmented-control">
                    <button
                      type="button"
                      className={layout.clipboardSync ? "active" : ""}
                      onClick={() => setClipboardSync(true)}
                    >
                      {ui.common.enabled}
                    </button>
                    <button
                      type="button"
                      className={!layout.clipboardSync ? "active" : ""}
                      onClick={() => setClipboardSync(false)}
                    >
                      {ui.common.disabled}
                    </button>
                  </div>
                </div>
                <div className="settings-control-row">
                  <span>
                    {ui.settings.fileTransfer}
                    <span className="info-tooltip-host" tabIndex={0}>
                      ⓘ
                      <span className="info-tooltip">
                        {ui.settings.fileTransferCopy}
                      </span>
                    </span>
                  </span>
                  <div className="segmented-control">
                    <button
                      type="button"
                      className={layout.fileTransferEnabled ? "active" : ""}
                      onClick={() => setFileTransferEnabled(true)}
                    >
                      {ui.common.enabled}
                    </button>
                    <button
                      type="button"
                      className={!layout.fileTransferEnabled ? "active" : ""}
                      onClick={() => setFileTransferEnabled(false)}
                    >
                      {ui.common.disabled}
                    </button>
                  </div>
                </div>
                {machineRole === "client" ? (
                  <div className="settings-control-row paired-controller-row">
                    <span className="paired-controller-label">
                      {layout.pairedControllers.length > 0
                        ? `${ui.settings.pairedWith}: ${layout.pairedControllers
                            .map(
                              (controller) =>
                                controller.name ||
                                controller.ip ||
                                controller.id,
                            )
                            .join("、")}`
                        : ui.settings.notPaired}
                    </span>
                    <button
                      type="button"
                      className="secondary-button compact-button danger-button"
                      disabled={layout.pairedControllers.length === 0}
                      onClick={() => void handleResetPairing()}
                    >
                      {ui.settings.resetPairing}
                    </button>
                  </div>
                ) : null}
              </section>

              <section className="surface-card modifier-card">
                <div className="card-title-row">
                  <h2>{ui.settings.modifierTitle}</h2>
                  <button
                    type="button"
                    className={`switch-button ${layout.modifierRemap ? "active" : ""}`}
                    onClick={() => setModifierRemap(!layout.modifierRemap)}
                  >
                    {layout.modifierRemap
                      ? ui.common.enabled
                      : ui.common.disabled}
                  </button>
                </div>
                <p className="muted-copy">{ui.settings.modifierCopy}</p>
                {(
                  [
                    ["control", ui.settings.modifierRowControl],
                    ["alt", ui.settings.modifierRowAlt],
                    ["meta", ui.settings.modifierRowMeta],
                  ] as [keyof ModifierMap, string][]
                ).map(([rowKey, rowLabel]) => (
                  <div className="settings-control-row" key={rowKey}>
                    <span>{rowLabel}</span>
                    <div className="segmented-control">
                      {(
                        ["control", "alt", "meta", "same"] as ModifierTarget[]
                      ).map((target) => (
                        <button
                          key={target}
                          type="button"
                          disabled={!layout.modifierRemap}
                          className={
                            layout.modifierMap[rowKey] === target ? "active" : ""
                          }
                          onClick={() => setModifierMapTarget(rowKey, target)}
                        >
                          {ui.settings.modifierTargets[target]}
                        </button>
                      ))}
                    </div>
                  </div>
                ))}
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
                  {usesWindowsChrome && machineRole === "client" ? (
                    <div>
                      <dt>{ui.settings.inputServiceTitle}</dt>
                      <dd>
                        {inputServiceStatusLabel}
                      </dd>
                    </div>
                  ) : null}
                </dl>
                <p className="muted-copy">{runtime.privilege.detail}</p>
                {runtime.privilege.canElevate ||
                canManageInputService ? (
                  <div className="inline-actions">
                    {runtime.privilege.canElevate ? (
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
                    ) : null}
                    {canManageInputService && !inputServiceInstalled ? (
                      <button
                        type="button"
                        className="primary-button compact-button"
                        onClick={() => setInputServiceAction("install")}
                        disabled={isInputServicePending}
                      >
                        {isInputServicePending
                          ? ui.common.pending
                          : ui.settings.installInputService}
                      </button>
                    ) : null}
                    {canManageInputService && runtime.inputService.installed ? (
                      <button
                        type="button"
                        className="secondary-button compact-button danger-button"
                        onClick={() => setInputServiceAction("uninstall")}
                        disabled={isInputServicePending}
                      >
                        {ui.settings.uninstallInputService}
                      </button>
                    ) : null}
                    {canManageInputService ? (
                      <button
                        type="button"
                        className="secondary-button compact-button"
                        onClick={() => void refreshInputService()}
                        disabled={isInputServicePending}
                      >
                        {ui.common.refresh}
                      </button>
                    ) : null}
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
                      {latestVersionLabel ? `v${latestVersionLabel}` : "--"}
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

              <section className="surface-card settings-card diagnostic-card">
                <div className="card-title-row">
                  <h2>{ui.settings.diagnostics}</h2>
                  <button
                    type="button"
                    className="secondary-button compact-button"
                    onClick={() => void refreshDiagnostics()}
                    disabled={isDiagnosticPending}
                  >
                    {isDiagnosticPending
                      ? ui.common.refreshing
                      : ui.common.refresh}
                  </button>
                </div>
                <p className="muted-copy">{ui.settings.diagnosticsCopy}</p>
                <dl className="network-meta compact-meta">
                  <div>
                    <dt>{ui.settings.peers}</dt>
                    <dd>{diagnosticInfo?.peerCount ?? lanPeers.length}</dd>
                  </div>
                  <div>
                    <dt>{ui.settings.logDirectory}</dt>
                    <dd className="diagnostic-path">
                      {diagnosticInfo?.logDir || "--"}
                    </dd>
                  </div>
                </dl>
                {diagnosticInfo ? (
                  <p className="muted-copy diagnostic-note">
                    {diagnosticInfo.networkHint} {diagnosticInfo.firewallHint}
                  </p>
                ) : null}
                <div className="inline-actions">
                  <button
                    type="button"
                    className="secondary-button compact-button"
                    onClick={() => void copyDiagnostics()}
                    disabled={isDiagnosticPending}
                  >
                    {ui.settings.copyDiagnostics}
                  </button>
                  <button
                    type="button"
                    className="secondary-button compact-button"
                    onClick={() => void handleOpenLogDirectory()}
                    disabled={isDiagnosticPending || !isTauri()}
                  >
                    {ui.settings.openLogDirectory}
                  </button>
                </div>
                {diagnosticMessage ? (
                  <p className="muted-copy diagnostic-message">
                    {diagnosticMessage}
                  </p>
                ) : null}
              </section>
            </div>
          </div>
        </section>
      ) : null}

      {serverPairing ? (
        <div className="pairing-modal-backdrop" role="presentation">
          <form className="pairing-modal" onSubmit={confirmPairing}>
            <div>
              <p className="eyebrow">{ui.devices.pairingEyebrow}</p>
              <h2>{ui.devices.serverPairingTitle}</h2>
              <p>
                {ui.devices.serverPairingCopy} {serverPairing.peer.name} ·{" "}
                {serverPairing.peer.ip || serverPairing.peer.host}
              </p>
            </div>
            <input
              className="pairing-code-input"
              value={serverPairingCode}
              aria-invalid={serverPairingError ? "true" : undefined}
              aria-describedby={
                serverPairingError ? "server-pairing-error" : undefined
              }
              onChange={(event) => {
                if (serverPairingError) {
                  setServerPairingError(null);
                }
                setServerPairingCode(
                  event.target.value.replace(/\D/g, "").slice(0, 6),
                );
              }}
              inputMode="numeric"
              autoComplete="one-time-code"
              placeholder={ui.devices.pairingCodePlaceholder}
              autoFocus
            />
            {serverPairingError ? (
              <p
                id="server-pairing-error"
                className="pairing-inline-error"
                role="alert"
              >
                {serverPairingError}
              </p>
            ) : null}
            <div className="pairing-actions">
              <button
                type="button"
                className="secondary-button"
                onClick={() => {
                  setServerPairing(null);
                  setServerPairingCode("");
                  setServerPairingError(null);
                }}
                disabled={isPairingDevice}
              >
                {ui.common.cancel}
              </button>
              <button
                type="submit"
                className="primary-button"
                disabled={isPairingDevice || serverPairingCode.length < 6}
              >
                {isPairingDevice
                  ? ui.common.connecting
                  : ui.devices.confirmPairing}
              </button>
            </div>
          </form>
        </div>
      ) : null}

      {machineRole === "client" && runtime.pairing.state === "requested" ? (
        <div className="pairing-modal-backdrop" role="presentation">
          <div className="pairing-modal pairing-modal-client">
            <button
              type="button"
              className="pairing-close-button"
              onClick={dismissClientPairing}
              disabled={isDismissingPairing}
              title={ui.devices.closePairing}
              aria-label={ui.devices.closePairing}
            >
              <WindowCloseIcon />
            </button>
            <div>
              <p className="eyebrow">{ui.devices.pairingEyebrow}</p>
              <h2>{ui.devices.clientPairingTitle}</h2>
              <p>
                {runtime.pairing.requesterName || ui.roles.server} ·{" "}
                {runtime.pairing.requesterIp}
              </p>
            </div>
            <strong className="pairing-code-display">
              {runtime.pairing.code}
            </strong>
            <p>{ui.devices.clientPairingCopy}</p>
            <div className="pairing-actions">
              <button
                type="button"
                className="secondary-button"
                onClick={dismissClientPairing}
                disabled={isDismissingPairing}
              >
                {isDismissingPairing
                  ? ui.devices.dismissingPairing
                  : ui.devices.dismissPairing}
              </button>
            </div>
          </div>
        </div>
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

      {isScanningLan ? (
        <div className="scan-modal-backdrop" role="presentation">
          <div className="scan-modal">
            <div className="scan-modal-spinner" aria-hidden="true" />
            <strong className="scan-modal-title">
              {ui.devices.scanningTitle}
            </strong>
            <p className="muted-copy">{ui.devices.scanningCopy}</p>
            {scanCountdown > 0 ? (
              <span className="scan-modal-countdown">{scanCountdown}s</span>
            ) : null}
          </div>
        </div>
      ) : null}
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

function normalizeEdgeSwitchHotkeyInput(value: string) {
  const normalized = value.trim().toLowerCase().replace(/\s+/g, "");
  return normalized.length === 0 ? "alt+shift+k" : normalized;
}

function renderHotkeyTags(displayHotkey: string, platform = "") {
  const parts = displayHotkey
    .split("+")
    .map((part) => part.trim())
    .filter((part) => part.length > 0);

  return (
    <span className="hotkey-tag-list">
      {parts.map((part) => (
        <span className="hotkey-tag" key={part}>
          {hotkeyTagLabel(part, platform)}
        </span>
      ))}
    </span>
  );
}

function hotkeyTagLabel(part: string, platform: string) {
  const useTextModifiers = prefersTextHotkeyModifiers(platform);

  switch (part.toLowerCase()) {
    case "command":
    case "cmd":
    case "meta":
      return useTextModifiers ? "Meta" : "⌘";
    case "shift":
      return useTextModifiers ? "Shift" : "⇧";
    case "alt":
    case "option":
      return useTextModifiers ? "Alt" : "⌥";
    case "control":
    case "ctrl":
      return useTextModifiers ? "Ctrl" : "⌃";
    case "win":
    case "windows":
      return "Win";
    case "space":
      return "Space";
    case "enter":
      return "Enter";
    case "escape":
      return "Esc";
    case "up":
      return "↑";
    case "down":
      return "↓";
    case "left":
      return "←";
    case "right":
      return "→";
    case "disabled":
      return "Off";
    default:
      return part.length === 1 ? part.toUpperCase() : part.toUpperCase();
  }
}

function prefersTextHotkeyModifiers(platform: string) {
  const normalized = platform.toLowerCase();
  return normalized.includes("win") || normalized.includes("linux");
}

function clampZoom(value: number) {
  return Math.min(BOARD_ZOOM_MAX, Math.max(BOARD_ZOOM_MIN, value));
}

function formatZoom(value: number) {
  return `${Math.round(value * 100)}%`;
}

function shouldOfferPermissionRelaunch(message: string) {
  return ["辅助功能", "输入监控", "Accessibility", "Input Monitoring"].some(
    (keyword) => message.includes(keyword),
  );
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

function RestartIcon() {
  return (
    <svg className="button-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path
        d="M20 12a8 8 0 1 1-2.34-5.66L20 8.68M20 4v4.68h-4.68"
        fill="none"
        stroke="currentColor"
        strokeLinecap="round"
        strokeLinejoin="round"
        strokeWidth="2"
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

function DownloadIcon() {
  return (
    <svg className="runtime-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path
        fill="currentColor"
        d="M12 3a1 1 0 0 1 1 1v8.59l2.3-2.3a1 1 0 1 1 1.4 1.42l-4 4a1 1 0 0 1-1.4 0l-4-4a1 1 0 1 1 1.4-1.42l2.3 2.3V4a1 1 0 0 1 1-1Z"
      />
      <path
        fill="currentColor"
        d="M5 18a1 1 0 0 1 1 1v1h12v-1a1 1 0 1 1 2 0v1.2A1.8 1.8 0 0 1 18.2 22H5.8A1.8 1.8 0 0 1 4 20.2V19a1 1 0 0 1 1-1Z"
      />
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
        deviceMatchesPeer(layout, device, candidate),
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
        online: true,
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

  return nextLayout;
}

function upsertPeerDevice(
  layout: LayoutState,
  peer: LanPeer,
  alias = "",
): LayoutState {
  const existingIndex = layout.devices.findIndex(
    (device) =>
      device.id === peerDeviceId(peer) ||
      (device.transportPublicKey.trim().length > 0 &&
        device.transportPublicKey === peer.transportPublicKey),
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
    online: true,
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
  const layoutScreens = flattenScreens(layout);
  const layoutBounds = getLayoutBounds(
    layoutScreens.length > 0 ? layoutScreens : [fallbackScreen()],
  );
  const peerMinX = Math.min(...peerScreens.map((screen) => screen.x));
  const peerMinY = Math.min(...peerScreens.map((screen) => screen.y));
  const startX = layoutBounds.maxX;

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

function fileTransferTargetIdAtPosition(
  position?: FileDropPosition | null,
  fallbackTargetId?: string | null,
) {
  if (!position) {
    return fallbackTargetId ?? null;
  }

  const element = document.elementFromPoint(position.x, position.y);
  const target = element?.closest<HTMLElement>("[data-file-transfer-target]");
  return target?.dataset.fileTransferTarget ?? fallbackTargetId ?? null;
}

function preferredPairedControllerId(
  layout: LayoutState,
  peers: LanPeer[],
): string | null {
  if (layout.pairedControllers.length === 0) {
    return null;
  }

  return (
    layout.pairedControllers.find((controller) =>
      peers.some(
        (peer) =>
          peer.id === controller.id ||
          (controller.transportPublicKey.trim().length > 0 &&
            peer.transportPublicKey === controller.transportPublicKey),
      ),
    )?.id ?? layout.pairedControllers[0].id
  );
}

function formatFileTransferSummary(
  summary: { targetName: string; fileCount: number; byteCount: number },
  language: AppLanguage,
) {
  const size = formatFileTransferBytes(summary.byteCount);
  if (language === "en") {
    return `${TEXT.en.devices.fileTransferSent} ${summary.fileCount} ${
      summary.fileCount === 1 ? "file" : "files"
    } to ${summary.targetName} · ${size}`;
  }

  return `${summary.fileCount} 个文件${TEXT.cn.devices.fileTransferSent}到 ${summary.targetName} · ${size}`;
}

function formatFileTransferBytes(bytes: number) {
  const gib = 1024 * 1024 * 1024;
  const mib = 1024 * 1024;
  if (bytes >= gib) {
    return `${(bytes / gib).toFixed(1)} GiB`;
  }
  if (bytes >= mib) {
    return `${(bytes / mib).toFixed(1)} MiB`;
  }
  if (bytes >= 1024) {
    return `${(bytes / 1024).toFixed(1)} KiB`;
  }
  return `${bytes} B`;
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

function formatUnknownError(error: unknown, fallback: string) {
  if (error instanceof Error && error.message.trim()) {
    return error.message;
  }
  if (typeof error === "string" && error.trim()) {
    return error;
  }

  try {
    const text = JSON.stringify(error);
    return text && text !== "null" ? text : fallback;
  } catch {
    return fallback;
  }
}

function formatUpdaterError(
  error: unknown,
  fallback: string,
  signatureMismatch: string,
) {
  const message = formatUnknownError(error, fallback);
  return /different key|signature.*key/i.test(message)
    ? signatureMismatch
    : message;
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
  return layout.devices.find((device) => deviceMatchesPeer(layout, device, peer));
}

function deviceMatchesPeer(layout: LayoutState, device: Device, peer: LanPeer) {
  return (
    device.id === peerDeviceId(peer) ||
    (device.transportPublicKey.trim().length > 0 &&
      device.transportPublicKey === peer.transportPublicKey) ||
    sameClusterHost(layout, device, peer)
  );
}

function sameClusterHost(layout: LayoutState, device: Device, peer: LanPeer) {
  return (
    layout.clusterId.trim().length > 0 &&
    !peer.pairingRequired &&
    peer.clusterId === layout.clusterId &&
    (sameHost(device.host, peer.host) || sameHost(device.host, peer.ip))
  );
}

function sameHost(value: string, host: string) {
  const normalizedHost = host.trim().toLowerCase();
  return (
    normalizedHost.length > 0 &&
    value
      .split("/")
      .map((part) => part.trim().toLowerCase())
      .some((part) => part === normalizedHost)
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
