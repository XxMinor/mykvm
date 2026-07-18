# Changelog

This file feeds the GitHub Release notes. Keep entries user-facing: describe what
changed for someone *using* MyKVM, not the internal/CI plumbing. The release
workflow publishes whatever is under `## [Unreleased]`, so move those entries
under a version heading when you cut a release (or just leave them — the next
release will reuse them).

## [Unreleased]

### Added

- Drag-and-drop files across machines (ShareMouse-style, experimental): drag files on the machine that owns the keyboard and mouse onto a controlled machine. Controlling Windows → Mac: drag files toward the screen edge that borders the Mac — a document icon follows the cursor onto the Mac, and releasing over an open Finder folder drops the files there (otherwise the Desktop). Controlling Mac → Windows client is also in. Requires file transfer to be enabled in Settings, and both sides on this version or newer.

### Fixed

- macOS: remote Caps Lock now switches the input source reliably. It no longer wedges when a key-up packet is dropped (which is what forced you to press it several times), and the injected ⌃Space is now paced so the focused app actually adopts the new source instead of only flipping the menu-bar indicator.
- macOS: closing the MacBook lid (or unplugging a monitor) now removes that display from the layout instead of leaving a phantom screen. The Mac re-checks its displays when the configuration changes and re-announces, instead of advertising the list it captured at startup.

- macOS: opening MyKVM while it is already running (a second .app copy, `open -n`, or launching from a mounted DMG) now brings the running window to the front instead of starting a second process that fights the first over the network ports.
- Windows: keyboard and mouse from the controller now keep working while a Remote Desktop session owns the machine and after it disconnects, so you can unlock the physical screen remotely instead of walking over to it (#21). The lock-screen input service now follows the physical console session when Remote Desktop swaps it, and the app reaches the service across that swap.

- Keyboard, mouse, and clipboard could fail to connect between machines — the QUIC handshake rejected the peer with `invalid peer certificate: BadSignature`. The transport now pins the device's advertised certificate directly instead of running brittle chain validation over a self-signed certificate, which fixes cross-platform (macOS ↔ Windows) handshakes.

## v0.4.0

### Added

- Update indicator in the title bar: a download icon appears next to "MyKVM" when a newer version is available — click it to open the update panel.

### Fixed

- "Latest version" in Settings now shows the latest released version once a check completes, instead of staying blank when you are already up to date.
- Corrected the clipboard sync description: images are synced too; only file clipboards are unsupported.

## v0.3.4

### Added

- Encrypted QUIC transport for keyboard, mouse, and clipboard traffic (TLS 1.3, pinned to the paired device's certificate).
- In-app updates: check GitHub Releases and install the latest version without leaving MyKVM.
- Clipboard image sync — copy a picture on one machine and paste it on the other (text was already supported).
- Roam across a remote machine's multiple monitors.
- Cross-platform installers for macOS, Windows, and Linux, built automatically on each release.
- Signed macOS builds, so the Accessibility permission survives app updates.

### Improved

- Smoother, more seamless mouse hand-off when crossing between machines and displays.
- Better modifier-key remapping between macOS and Windows.
- Smoother slide-back when MyKVM is not the front window on macOS.
- More reliable LAN discovery and manual peer connection.

### Fixed

- Trackpad two-finger scrolling on the Settings page.
- Faster, more reliable Windows clipboard sync.

## v0.1.0

- Added server/client onboarding and display layout editing.
- Added LAN discovery, manual peer connection, and shared input transport.
- Added text clipboard sync.
- Added English and Simplified Chinese UI strings.
- Added light, dark, and system theme modes.
- Added configurable single-port UDP transport with fallback.
- Added opt-in app performance monitoring.
- Added GitHub Actions CI and tag-based desktop release builds.
