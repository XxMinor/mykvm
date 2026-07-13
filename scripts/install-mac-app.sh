#!/usr/bin/env sh
set -eu

APP_SOURCE="${1:-src-tauri/target/aarch64-apple-darwin/release/bundle/macos/mykvm.app}"
APP_DEST="${2:-/Applications/mykvm.app}"

if [ "$(uname -s)" != "Darwin" ]; then
  printf "macOS installation must run on macOS.\n" >&2
  exit 1
fi

if [ ! -d "$APP_SOURCE" ]; then
  printf "Built app bundle not found: %s\n" "$APP_SOURCE" >&2
  exit 1
fi

osascript -e 'tell application id "com.xzhpl.mykvm" to quit' 2>/dev/null || true
sleep 2
pkill -f '/mykvm\.app/Contents/MacOS/mykvm' 2>/dev/null || true
sleep 1

ditto "$APP_SOURCE" "$APP_DEST"
xattr -dr com.apple.quarantine "$APP_DEST" 2>/dev/null || true

"$(dirname "$0")/sign-mac-app.sh" "$APP_DEST"

case "$APP_SOURCE" in
  src-tauri/target/*/release/bundle/macos/mykvm.app|*/src-tauri/target/*/release/bundle/macos/mykvm.app)
    rm -rf "$APP_SOURCE"
    ;;
esac

open "$APP_DEST"

if command -v netstat >/dev/null 2>&1; then
  sleep 6
  if ! netstat -anv -p udp 2>/dev/null | grep -q 'mykvm:'; then
    osascript -e 'tell application id "com.xzhpl.mykvm" to quit' 2>/dev/null || true
    sleep 2
    pkill -f '/mykvm\.app/Contents/MacOS/mykvm' 2>/dev/null || true
    sleep 1
    open "$APP_DEST"
  fi
fi
