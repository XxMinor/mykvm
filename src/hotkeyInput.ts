export interface HotkeyKeyboardEventLike {
  key: string;
  code?: string;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  metaKey: boolean;
}

export type MetaKeyLabel = "command" | "win" | "meta";

const MODIFIER_KEYS = new Set([
  "alt",
  "altgraph",
  "cmd",
  "command",
  "control",
  "ctrl",
  "meta",
  "os",
  "shift",
  "super",
  "win",
  "windows",
]);

export function edgeSwitchHotkeyFromKeyboardEvent(
  event: HotkeyKeyboardEventLike,
  metaKeyLabel: MetaKeyLabel = "meta",
): string | null {
  return hotkeyFromKeyboardEvent(event, metaKeyLabel);
}

/// Generic hotkey capture used by both the runtime-toggle recorder and the
/// screen-switch direction recorders. Returns the canonical hotkey string, or
/// `null` to ignore the event (e.g. a bare modifier press), or `"disabled"` to
/// clear the binding (Backspace/Delete).
export function hotkeyFromKeyboardEvent(
  event: HotkeyKeyboardEventLike,
  metaKeyLabel: MetaKeyLabel = "meta",
): string | null {
  if (event.key === "Backspace" || event.key === "Delete") {
    return "disabled";
  }

  if (MODIFIER_KEYS.has(event.key.toLowerCase())) {
    return null;
  }

  const key = normalizeHotkeyEventKey(event);
  if (!key) {
    return null;
  }

  const hasModifier =
    event.ctrlKey || event.altKey || event.shiftKey || event.metaKey;
  if (!hasModifier && !shouldCaptureSingleKey(key)) {
    return null;
  }

  const parts: string[] = [];
  if (event.ctrlKey) {
    parts.push("ctrl");
  }
  if (event.altKey) {
    parts.push("alt");
  }
  if (event.shiftKey) {
    parts.push("shift");
  }
  if (event.metaKey) {
    parts.push(metaKeyLabel);
  }
  parts.push(key);

  return parts.join("+");
}

export function metaKeyLabelForPlatform(platform: string): MetaKeyLabel {
  const normalized = platform.toLowerCase();
  if (normalized.includes("mac") || normalized.includes("darwin")) {
    return "command";
  }
  if (normalized.includes("win")) {
    return "win";
  }
  return "meta";
}

export function formatEdgeSwitchHotkeyForDisplay(
  hotkey: string,
  metaKeyLabel: MetaKeyLabel,
) {
  return hotkey
    .trim()
    .toLowerCase()
    .replace(/\s+/g, "")
    .split("+")
    .filter((part) => part.length > 0)
    .map((part) => (isMetaAlias(part) ? metaKeyLabel : part))
    .join("+");
}

function isMetaAlias(part: string) {
  return [
    "cmd",
    "command",
    "meta",
    "os",
    "super",
    "win",
    "windows",
  ].includes(part);
}

function normalizeHotkeyEventKey(event: HotkeyKeyboardEventLike) {
  const codeKey = normalizeHotkeyCode(event.code);
  if (codeKey) {
    return codeKey;
  }

  return normalizeHotkeyKey(event.key);
}

function normalizeHotkeyCode(code?: string) {
  if (!code) {
    return null;
  }

  const letter = /^Key([A-Z])$/.exec(code);
  if (letter) {
    return letter[1].toLowerCase();
  }

  const digit = /^Digit([0-9])$/.exec(code);
  if (digit) {
    return digit[1];
  }

  const functionKey = /^F([1-9]|1[0-9]|2[0-4])$/.exec(code);
  if (functionKey) {
    return `f${functionKey[1]}`;
  }

  switch (code) {
    case "Space":
      return "space";
    case "Tab":
      return "tab";
    case "Enter":
    case "NumpadEnter":
      return "enter";
    case "Escape":
      return "escape";
    case "ScrollLock":
      return "scrolllock";
    case "ArrowUp":
      return "up";
    case "ArrowDown":
      return "down";
    case "ArrowLeft":
      return "left";
    case "ArrowRight":
      return "right";
    default:
      return null;
  }
}

function normalizeHotkeyKey(key: string) {
  const normalized = key.toLowerCase();
  if (/^[a-z0-9]$/.test(normalized)) {
    return normalized;
  }

  if (/^f([1-9]|1[0-9]|2[0-4])$/.test(normalized)) {
    return normalized;
  }

  switch (normalized) {
    case " ":
    case "space":
    case "spacebar":
      return "space";
    case "tab":
      return "tab";
    case "enter":
    case "return":
      return "enter";
    case "esc":
    case "escape":
      return "escape";
    case "scrolllock":
    case "scroll":
    case "scrlk":
      return "scrolllock";
    case "arrowup":
    case "up":
      return "up";
    case "arrowdown":
    case "down":
      return "down";
    case "arrowleft":
    case "left":
      return "left";
    case "arrowright":
    case "right":
      return "right";
    default:
      return null;
  }
}

function shouldCaptureSingleKey(key: string) {
  return /^f([1-9]|1[0-9]|2[0-4])$/.test(key) || key === "scrolllock";
}
