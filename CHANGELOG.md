# Changelog

This file feeds the GitHub Release notes. Keep entries user-facing: describe what
changed for someone *using* MyKVM, not the internal/CI plumbing. The release
workflow publishes whatever is under `## [Unreleased]`, so move those entries
under a version heading when you cut a release (or just leave them — the next
release will reuse them).

## [Unreleased]

### Added

- Device settings use a modal form. Pointer and scroll speeds are saved together; pairing and removal actions are grouped inside the form.
- Folder transfers preserve nested directories and empty folders. Windows native drags include directory descriptors; ordinary transfers unpack into the receiving folder. New folder and file-clipboard protocols require both peers to be updated.
- Rich clipboard sharing includes HTML and RTF with a plain-text fallback.
- File clipboard paste transfers selected files/folders without requiring network file shares. Active transfer toasts have a Cancel button.
- Manually added peers keep their selected connection IP, including after pairing, rediscovery, and address changes (#26).
- Drag-and-drop files across machines (ShareMouse-style, experimental): drag files on the machine that owns the keyboard and mouse onto a controlled machine. Controlling Windows → Mac: drag files toward the screen edge that borders the Mac — a document icon follows the cursor onto the Mac, and releasing over an open Finder folder drops the files there (otherwise the Desktop). Controlling Mac → Windows client is also in. Requires file transfer to be enabled in Settings, and both sides on this version or newer.
- Drag files the other way too — from a controlled machine back to the controller. While controlling a Mac from Windows, grab a file on the Mac and drag it back across the edge onto Windows: it becomes a real native drag on Windows that you can drop into any folder, app, or field. Requires both sides on this version or newer.
- Fetch the other machine's log from Settings → Diagnostics: a server pulls its online clients' logs ("Fetch Client Log"), a client pulls its server's ("Fetch Server Log"). The log lands in Downloads/MyKVM Remote Logs, so troubleshooting no longer means copying files between machines. Requires both sides on this version or newer.
- Update a client from the controller: in Devices, a client running an older version shows "Update available" and an Update button. The client installs the release its own updater finds (still verified against its update key) and restarts, so a machine without its own keyboard and mouse never needs one for an update. Requires the client on this version or newer.
- Media keys — volume up/down, mute, play/pause, next and previous — reach the machine you are controlling, in both directions.
- Per-device pointer and scroll speed: on the controller, each added device in Devices has its own pointer (0.5×–2×) and scroll (0.5×–3×) speed.
- Lock sync (Settings, off by default): locking the controller locks its online clients too. Only locking is synced; each computer unlocks with its own password.
- Windows clients offer the unattended-control service once when it is not installed; it is what keeps a client controllable before sign-in and while MyKVM restarts for an update.

### Fixed

- Screen-switch shortcuts restore the cursor position used before leaving each device and screen. Only a first visit without a remembered position lands at the center; hidden client parking does not overwrite the remembered position.
- MyKVM shortcuts release logical keys on both devices while tracking physically held modifiers separately. Holding Alt for repeated screen switches remains possible; releasing Alt stops plain arrow keys from switching screens.
- macOS: leaving a controlled Mac requests cursor hiding until the next movement. Re-entry, local mouse movement, and stopping sharing restore it; ordered cursor messages prevent delayed parking from overriding re-entry.
- macOS: remote wheel events use independent input state and explicit discrete scrolling flags. Rate-limited scroll logs help distinguish repeated remote events from continued scrolling inside an application.
- Windows: startup requests administrator privileges through the normal UAC prompt, so elevated desktop programs do not require a separate trip to Settings.
- Settings and Devices pages drop redundant page introductions; concise help is available beside labels. Clipboard sync has a single switch and old on-demand settings migrate to automatic sync without changing the switch state.
- Windows: keyboard capture uses a separate message thread and a fresh worker after screenshot capture or a remote-screen handoff. Snipaste Ctrl+F1 temporarily returns both inputs to Windows for capture, then restores the original client. Remote cursor hiding uses a dedicated non-activating owner window.
- The separate screen-protection UI and local-lock shortcut are removed. Existing quick start/stop shortcuts are preserved and legacy protection settings are disabled during migration.
- Windows: keyboard capture recovers independently when mouse sharing still works, including after a single missed key. Recovery keeps the current controlled screen; returning the mouse to the server is no longer needed to restore typing.
- Windows native file drags spool data to disk instead of retaining whole files in memory. Cancellation wakes blocked readers; received transfers do not overwrite existing destinations and expired partial transfers are removed.
- Version labels and updater comparisons use the configured package version, including local beta test builds, so an older published beta is not offered as an upgrade.
- Windows: an old capture thread cannot clear a newer thread's context, and local control cannot forward clicks/keys through a stale remote target. Hook health uses callback receipt time rather than delayed event timestamps. A second instance cannot start when access to the existing instance's lock is refused.
- Windows: dragging files back from a controlled Mac finishes on the local mouse release instead of waiting for a remote drop signal. Its own drag input stays local, failed drag startup cancels cleanly, and Escape cancels in either direction.
- Windows: local keyboard activity no longer makes the input monitor mistake healthy hooks for removed ones. Returning to the screen edge briefly prevents bouncing straight back into the controlled machine.
- Windows: after an idle period, a queued input callback no longer triggers a false hook restart. The hidden pointer stays at the display center during remote control, so a fullscreen game recentering it there does not look like a jump back to the controlling machine. Screen-layout refreshes run outside the mouse callback to reduce work while using the local mouse.
- Windows: leaving the mouse still no longer restarts the input hooks just because two input timestamps differ. A successful hook repair keeps control on the current machine instead of sending the cursor back to the controller.
- Windows: the controlling machine no longer takes control back on its own when one check of the input desktop fails; only a secure desktop (UAC prompt, lock screen) that stays up does.
- Quitting or updating MyKVM tells the other machine at once, so it reconnects right away — on a Windows client, to the input service, which keeps the machine controllable while the installer runs — instead of sending into a dead connection for about 10 seconds.
- Windows installer: upgrading over an older version, including "uninstall before installing", keeps the input service and no longer asks to restart Windows; an interactive install starts MyKVM as soon as its files are in place instead of at the finish page; it no longer waits 12 seconds trying to stop a service it has no rights to stop, and it runs standard Windows tools instead of hidden PowerShell that security software flagged.
- One input that fails to send — a mouse move, click, key or scroll — no longer hands control back to the controlling machine when the connection is being rebuilt or Wi-Fi hiccups; only inputs that keep failing for a second do. Before, your next keys and shortcuts could land on the controlling machine while you were still looking at the other screen, until you moved the mouse over again.
- Pairing a client that was already paired (the two machines swapped roles, or the server was reinstalled) now shows the pairing code on the client, instead of bringing its window up with no code to type.
- macOS: crossing from the Mac onto another machine hides the Mac cursor right away, instead of leaving it painted at the screen edge for up to a second.
- macOS: with its window closed or minimized, MyKVM leaves the Dock and Cmd+Tab and stays in the menu bar; it comes back when the window is shown.
- A machine running a proxy in TUN mode (Clash, Mihomo, Surge, sing-box) keeps the same device identity whether the proxy is on or off, instead of showing up as a second device.
- Clipboard sync to a machine that stopped answering backs off to one retry a minute and logs once, instead of retrying every 2 seconds and filling the log.

- macOS: memory no longer grows with every image received through clipboard sync. Each synced image leaked its full size, so a few dozen screenshots could push MyKVM to around 2 GB (discussion #32).
- Controlling another machine no longer drops you back to local control when that machine refuses a clipboard sync (for example clipboard sync is off there, or it runs an older version). The keyboard/mouse connection stays up, refused content is not re-sent every 2 seconds, and large clipboard images get enough time to be acknowledged.
- After a Wi-Fi stall the controlled machine no longer replays seconds of stale mouse movement and clicks.
- macOS: pushing the cursor against the bottom of a display no longer jumps into a machine arranged above it (#34).
- macOS: when the controlled machine stops accepting input, clicks and keys now hand control back to the Mac instead of freezing the trackpad and mouse until MyKVM is force-quit. A file drag that an older client refuses is delivered to that machine's Desktop instead (#33).
- The device list and diagnostics show every IPv4 address of this machine, so a direct-cable or Thunderbolt-bridge address is visible for manual pairing, not just the Wi-Fi one (#33).
- Windows: MyKVM starts a stopped lock-screen input service from an older install, so clicks work on the lock screen again (#27).
- macOS: launch at startup opens MyKVM once and silently, instead of racing macOS "Reopen windows when logging back in" and showing the window. Display changes on wake are applied once instead of several times, without blocking the app, and input-capture errors are now written to the log.
- macOS: returning to the Mac after a long session with the window hidden no longer stalls while the hidden cursor is restored.
- Mouse movement keeps a steady 125 Hz instead of dropping to about 62 Hz when input callbacks jitter.
- Windows: precision touchpads and smooth scroll wheels now scroll the controlled machine.
- Windows: a clipboard held open by another app no longer drops a synced copy.
- Updates: a stalled update check gives up after 20 seconds with a clear message, and .deb/.rpm installs are offered their own update package instead of the AppImage.
- Background input: blocking clipboard/file handlers no longer occupy QUIC workers; reconnecting input stays local until the transport is ready and retries with full pairing credentials.
- Clipboard images are encoded once, so a 4K screenshot fits the stream limit. Unchanged clipboard contents use the OS change counter instead of repeatedly reading and encoding the image.
- macOS: display changes refresh both saved placement and native input coordinates. Same-resolution displays keep separate placements, and active sharing stays responsive to input while the window is hidden, including receive-only mode.
- Windows input service: installation/repair configures automatic recovery after failures; status files are refreshed at most once per second unless state changes, and new input is skipped if attaching the current desktop fails.
- LAN discovery cannot replace a paired device's transport certificate; an identity change now requires re-pairing.
- Linux reports its unavailable input backend explicitly instead of advertising input readiness.
- Preserve the saved display layout when no displays are temporarily available during sleep or startup, instead of replacing it with a 1×1 placeholder (#30).
- macOS: remote Caps Lock now switches the input source reliably. It switches the source directly (via Carbon TIS) instead of injecting the ⌃Space hotkey, which a Chinese IME such as WeType would swallow so nothing changed. Caps now toggles between English and the input method you last used, and no longer wedges when a key-up packet is dropped (which is what forced you to press it several times).
- macOS: closing the MacBook lid (or unplugging a monitor) now removes that display from the layout instead of leaving a phantom screen. The Mac re-checks its displays when the configuration changes and re-announces, instead of advertising the list it captured at startup. Re-opening the lid (or replugging) now restores the display to the exact spot you had arranged it — it's matched by resolution and remembered across the disconnect, so the controller can reach it again instead of the screen coming back in the wrong place (or not at all).

- macOS: opening MyKVM while it is already running (a second .app copy, `open -n`, or launching from a mounted DMG) now brings the running window to the front instead of starting a second process that fights the first over the network ports.
- Windows: keyboard and mouse from the controller now keep working while a Remote Desktop session owns the machine and after it disconnects, so you can unlock the physical screen remotely instead of walking over to it (#21). The lock-screen input service now follows the physical console session when Remote Desktop swaps it, and the app reaches the service across that swap.

### Known limitations

- Linux keyboard/mouse capture and injection are not implemented yet (#11, #31). Linux packages do not provide input sharing.
- Native cross-screen file dragging remains experimental. Folder transfer and file clipboard sharing require both peers to be updated.
- The reported intermittent continuous scrolling still needs reproduction with the new event logs; the wheel-state changes have not yet been confirmed to resolve every case.

## v0.9.12

### Fixed

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
