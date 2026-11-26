import { useEffect, useState } from "react";
import { cancelFileTransfer, loadAppState, shownFileTransfers } from "./desktopApi";
import { formatFileTransferBytes, TEXT, type AppText } from "./i18n";
import type { AppLanguage, FileTransferProgressEntry } from "./types";
import "./App.css";

// One line of 24-unit strokes per state, drawn in the tile beside the name.
const ICONS = {
  sending: "M12 19V5m-6 6 6-6 6 6",
  fetching: "M12 5v14m-6-6 6 6 6-6",
  done: "m5 12.5 4.5 4.5L19 7.5",
  failed: "M12 6.5v7m0 3.5v.5",
};

function formatRemaining(seconds: number) {
  const whole = Math.ceil(seconds);
  const minutes = Math.floor(whole / 60);
  const pad = (value: number) => String(value).padStart(2, "0");
  return minutes >= 60
    ? `${Math.floor(minutes / 60)}:${pad(minutes % 60)}:${pad(whole % 60)}`
    : `${minutes}:${pad(whole % 60)}`;
}

export function TransferRow({
  transfer,
  ui,
}: {
  transfer: FileTransferProgressEntry;
  ui: AppText;
}) {
  const failed = Boolean(transfer.error);
  const state = failed
    ? "failed"
    : transfer.done
      ? "done"
      : transfer.incoming
        ? "fetching"
        : "sending";
  const percent =
    transfer.totalBytes > 0
      ? Math.min(100, Math.round((transfer.sentBytes / transfer.totalBytes) * 100))
      : state === "done"
        ? 100
        : 0;
  const status = failed
    ? ui.devices.fileTransferFailed
    : transfer.incoming
      ? transfer.done
        ? ui.devices.fileTransferFetched
        : ui.devices.fileTransferFetching
      : transfer.done
        ? ui.devices.fileTransferSent
        : ui.devices.fileTransferSending;
  const counter =
    transfer.fileCount > 1 ? ` · ${transfer.fileIndex}/${transfer.fileCount}` : "";
  const running = !transfer.done && !failed;
  const speed = running ? transfer.bytesPerSecond : 0;
  const meta = failed
    ? transfer.error
    : !running
      ? formatFileTransferBytes(transfer.totalBytes)
      : [
          `${formatFileTransferBytes(transfer.sentBytes)} / ${formatFileTransferBytes(transfer.totalBytes)}`,
          speed > 0 ? `${formatFileTransferBytes(speed)}/s` : "",
          speed > 0
            ? ui.devices.fileTransferRemaining(
                formatRemaining((transfer.totalBytes - transfer.sentBytes) / speed),
              )
            : "",
        ]
          .filter(Boolean)
          .join(" · ");
  return (
    <div className={`file-transfer-toast ${state}`} role="status">
      <svg className="file-transfer-toast-icon" viewBox="0 0 24 24" aria-hidden="true">
        <path d={ICONS[state]} />
      </svg>
      <div className="file-transfer-toast-body">
        <div className="file-transfer-toast-head">
          <span className="file-transfer-toast-name" title={transfer.fileName}>
            {transfer.fileName}
          </span>
          {running ? (
            <button
              type="button"
              className="file-transfer-toast-cancel"
              onClick={() => void cancelFileTransfer(transfer.transferId)}
            >
              {ui.common.cancel}
            </button>
          ) : null}
        </div>
        <div className="file-transfer-toast-status">
          {status} · {transfer.targetName}
          {counter}
        </div>
        <div className="file-transfer-toast-track">
          <div
            className="file-transfer-toast-fill"
            style={{ width: `${failed ? 100 : percent}%` }}
          />
        </div>
        <div className="file-transfer-toast-meta">
          <span title={failed ? (transfer.error ?? "") : undefined}>{meta}</span>
          {running ? <strong>{percent}%</strong> : null}
        </div>
      </div>
    </div>
  );
}

// All there is in the pop-up window: a card for each transfer the backend
// says to show. The backend opens the window while the main one is not being
// looked at, makes it as tall as its cards, and closes it a moment after the
// last transfer ends.
export default function TransferToast() {
  const [transfers, setTransfers] = useState<FileTransferProgressEntry[]>([]);
  const [language, setLanguage] = useState<AppLanguage>("cn");

  useEffect(() => {
    let active = true;
    void loadAppState()
      .then((state) => {
        if (!active) {
          return;
        }
        if (state.layout.language in TEXT) {
          setLanguage(state.layout.language);
        }
        // Dark or light as the main window is.
        const theme = state.layout.themeMode;
        document.documentElement.dataset.theme =
          theme !== "system"
            ? theme
            : window.matchMedia("(prefers-color-scheme: light)").matches
              ? "light"
              : "dark";
      })
      .catch(() => {});
    // Asked for, not listened to: a card also goes when its transfer has been
    // over for a moment, which no report says.
    const refresh = () =>
      void shownFileTransfers()
        .then((shown) => {
          if (active) {
            setTransfers(shown);
          }
        })
        .catch(() => {});
    refresh();
    const timer = setInterval(refresh, 150);
    return () => {
      active = false;
      clearInterval(timer);
    };
  }, []);

  return transfers.length > 0 ? (
    <div className="transfer-toast-window">
      {transfers.map((transfer) => (
        <TransferRow key={transfer.transferId} transfer={transfer} ui={TEXT[language]} />
      ))}
    </div>
  ) : null;
}
