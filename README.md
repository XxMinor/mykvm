# MyKVM

**One keyboard, one mouse, one clipboard — shared across your Mac, Windows, and Linux machines on the same LAN.**

Move your cursor off the edge of one screen and it lands on the next machine. Your keyboard follows, and the clipboard (text and images) syncs automatically. No KVM hardware, no cables.

[![Download](https://img.shields.io/github/v/release/XxMinor/mykvm?label=Download&style=for-the-badge)](https://github.com/XxMinor/mykvm/releases/latest)
[![Stars](https://img.shields.io/github/stars/XxMinor/mykvm?label=Stars&logo=github&style=for-the-badge)](https://github.com/XxMinor/mykvm/stargazers)
[![Forks](https://img.shields.io/github/forks/XxMinor/mykvm?label=Forks&logo=github&style=for-the-badge)](https://github.com/XxMinor/mykvm/forks)
[![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Windows%20%7C%20Linux-2786ff?style=for-the-badge)](https://github.com/XxMinor/mykvm/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-green?style=for-the-badge)](./LICENSE)
[![LINUX DO](https://img.shields.io/badge/LINUX-DO-FFB003.svg?logo=data:image/svg%2bxml;base64,DQo8c3ZnIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgd2lkdGg9IjEwMCIgaGVpZ2h0PSIxMDAiPjxwYXRoIGQ9Ik00Ni44Mi0uMDU1aDYuMjVxMjMuOTY5IDIuMDYyIDM4IDIxLjQyNmM1LjI1OCA3LjY3NiA4LjIxNSAxNi4xNTYgOC44NzUgMjUuNDV2Ni4yNXEtMi4wNjQgMjMuOTY4LTIxLjQzIDM4LTExLjUxMiA3Ljg4NS0yNS40NDUgOC44NzRoLTYuMjVxLTIzLjk3LTIuMDY0LTM4LjAwNC0yMS40M1EuOTcxIDY3LjA1Ni0uMDU0IDUzLjE4di02LjQ3M0MxLjM2MiAzMC43ODEgOC41MDMgMTguMTQ4IDIxLjM3IDguODE3IDI5LjA0NyAzLjU2MiAzNy41MjcuNjA0IDQ2LjgyMS0uMDU2IiBzdHlsZT0ic3Ryb2tlOm5vbmU7ZmlsbC1ydWxlOmV2ZW5vZGQ7ZmlsbDojZWNlY2VjO2ZpbGwtb3BhY2l0eToxIi8+PHBhdGggZD0iTTQ3LjI2NiAyLjk1N3EyMi41My0uNjUgMzcuNzc3IDE1LjczOGE0OS43IDQ5LjcgMCAwIDEgNi44NjcgMTAuMTU3cS00MS45NjQuMjIyLTgzLjkzIDAgOS43NS0xOC42MTYgMzAuMDI0LTI0LjM4N2E2MSA2MSAwIDAgMSA5LjI2Mi0xLjUwOCIgc3R5bGU9InN0cm9rZTpub25lO2ZpbGwtcnVsZTpldmVub2RkO2ZpbGw6IzE5MTkxOTtmaWxsLW9wYWNpdHk6MSIvPjxwYXRoIGQ9Ik03Ljk4IDcwLjkyNmMyNy45NzctLjAzNSA1NS45NTQgMCA4My45My4xMTNRODMuNDI2IDg3LjQ3MyA2Ni4xMyA5NC4wODZxLTE4LjgxIDYuNTQ0LTM2LjgzMi0xLjg5OC0xNC4yMDMtNy4wOS0yMS4zMTctMjEuMjYyIiBzdHlsZT0ic3Ryb2tlOm5vbmU7ZmlsbC1ydWxlOmV2ZW5vZGQ7ZmlsbDojZjlhZjAwO2ZpbGwtb3BhY2l0eToxIi8+PC9zdmc+&style=for-the-badge)](https://linux.do/)
[![XQAPI](https://img.shields.io/badge/XQAPI-AI-2786FF.svg?logo=data%3aimage%2fpng%3bbase64%2ciVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAYAAAAf8%2f9hAAAAAXNSR0IArs4c6QAAAERlWElmTU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAA6ABAAMAAAABAAEAAKACAAQAAAABAAAAEKADAAQAAAABAAAAEAAAAAA0VXHyAAACkElEQVQ4EZVTTUhUURT%2b3pvne%2fPn5MZo1Ewc1MUMJE1t8o9oHZQTBQpty8pdQbQp6Fcq%2bqEoopD%2bFoW%2f7YoWiaUoTjALNQMHrLHAmphxGuf33du5V2coCMID591zz%2fnOd8859z4Fa8I5LyHTnclkNMMweMH%2f50oxhXBmOGz95vUq2WIsl8vtZIyNMpPFSRNkSzVNMyH35DOFfzUWp%2fj7VCrVKgmI0c1Mc4HWdQmRRFaiK1Uqnb5HUdVq8H9WXayyaAgcqaIolVqp1q4JQwRFukI6MDiM5y%2f6yMHQ2dkBi6ri8dNntGUIBNpx8MB%2byicCgVcsVRqtjFQw4sHDXvQ%2beoLTp07C0A2cu3AJ2Vye9ifgdDpx%2fmIPfkSjONZ1WKSQECu1cEY0H4vFeHPbbv5xbo4z2g8ODfOpYJBHIos8nU7z%2fv4BPjY%2bzptadvFo9KdI4ZR7WV1lAggIh8OBhvp6TExMYt%2feAEZGRlFZWYFXr99Q%2bQHMzMzC7d5E2Mhamooigd1uRz7PSPOwGgacZWVwuVwSaLdZYXVsgE5tpdIZCGxBigTV1ZvhKnXg7r37aGzcivF3b7Hw%2bQuuXL2OtrZWBCfHsLT0nUhKUFOzZS2fQREz0DTtrPDMz4dx5Gi3bMXQdVg0TU4%2fm82SbUF8OYHbN6%2bhrq5OwEW1PeIWZBU0E3g8tXg51Iep4AeEQiF4amuhWjRMT09jx3Y%2f%2fP5tsnyBFbdGqapGzzVGFch7FQGbzYaW5iZsLC%2fHjVt3wMw8uo93wefziUPlG5AGfWhmMSwvpxvonccpeV1CB%2f9KJBJeSZZKZTrobS%2bS%2fpdEYEi%2f0s90SCSLRqQkk8kKXdcbAJoW8gX3Xyt1SmXDpMF%2foqtcFMHfSC7T5ZzIvHoAAAAASUVORK5CYII%3d&style=for-the-badge)](https://xqapi.com/)

[中文说明](./README.zh-CN.md)

![MyKVM tour](docs/screenshots/tour.gif)

## Screenshots

| Display layout | Devices | Settings |
| --- | --- | --- |
| ![Layout](docs/screenshots/layout.png) | ![Devices](docs/screenshots/devices.png) | ![Settings](docs/screenshots/settings.png) |

## Quick Start

1. **Install on both machines.** Download the installer for each OS from the [latest release](https://github.com/XxMinor/mykvm/releases/latest).
2. **Pick roles.** On the machine whose keyboard and mouse you want to share, open MyKVM and keep **Server** mode (the default). On the other machine, open MyKVM and switch to **Client** mode in Settings.
3. **Connect.** On the same LAN the two find each other automatically. Otherwise open **Devices**, type the other machine's IP (optionally `IP:port`), and click **Add**. Only devices that report their screen info join the layout.
4. **Arrange screens.** Open **Layout** and drag the monitors so their touching edges match how they sit on your desk.
5. **Cross over.** Push the cursor past a shared edge — it moves to the other machine. The keyboard follows, and copy/paste works in both directions.

## Permissions

- **macOS (server).** Grant MyKVM both **Accessibility** and **Input Monitoring** under System Settings → Privacy & Security. These are required to capture and inject keyboard/mouse input. Signed builds keep the grant across updates; if it ever drops, toggle it off and on.
- **macOS first launch.** Builds are free self-signed (not Apple-notarized), so Gatekeeper warns the first time. Right-click the app → **Open** → **Open** to allow it once.
- **Windows.** No special permission for normal use. Run as Administrator only if you need to control elevated/admin windows.
- **Linux.** If you use the AppImage, mark it executable (`chmod +x`).

## Limitations

- **Trusted LAN only.** Pairing codes and paired certificate binding are supported; discovery remains plaintext. Do not expose the ports to public or untrusted networks.
- Input and clipboard ride an **encrypted QUIC/TLS** connection pinned to the peer's advertised certificate, but MyKVM is a prototype and is not hardened for hostile networks.
- Clipboard sharing supports text, images, HTML and RTF through one sync switch. File/folder transfers are limited to 2 GiB.
- macOS builds are **self-signed, not notarized** — expect a Gatekeeper prompt on first open.
- Experimental software: the protocol and behavior may change between versions.

---

## Features

- Runs in Server or Client mode.
- Discovers nearby peers on the LAN.
- Supports manual peer connection by host or IP.
- Detects local displays and lets you arrange multi-monitor layouts.
- Shares keyboard and mouse input over an encrypted QUIC connection.
- Syncs text, images and rich clipboard formats over the same encrypted connection.
- Device mouse speeds are configured in a modal form. The existing quick start/stop shortcut remains the way to pause sharing; separate input protection and lock shortcuts have been removed.
- Clipboard sync automatically shares copied content; it has no separate mode or paste-shortcut controls.
- Folder transfers keep nested and empty directories and can be cancelled. Both peers must be updated. Linux native X11/Wayland input is not implemented yet.
- Provides light, dark, and system theme modes.
- Includes English and Simplified Chinese UI.
- Supports tray behavior for hiding and restoring the main window.
- Checks GitHub Releases and updates itself in place.

## Current Status

MyKVM is an experimental early release. It is useful for local testing and iteration, but it is not hardened for untrusted networks. See the [Releases page](https://github.com/XxMinor/mykvm/releases) for the current version and installers.

- License: MIT
- Default ports: UDP `47833` (discovery) and UDP `47834` (QUIC transport)
- Clipboard payload caps: 256 KB text, 32 MB image
- Transport security: input and clipboard run over a TLS 1.3 (QUIC) connection pinned to the peer's advertised certificate
- Security model: trusted LAN prototype
- Not yet included: fully authenticated discovery and production transport hardening for untrusted networks

Do not expose the transport ports to public or untrusted networks.

## Protocol

MyKVM runs two channels. LAN discovery uses a plain UDP port; input and clipboard run over an encrypted QUIC connection on a second UDP port.

| Channel | Default port | Transport | Marker | Purpose |
| --- | --- | --- | --- | --- |
| Discovery | UDP `47833` | UDP datagrams | `mykvm.discovery.v1` | LAN discovery, peer probe/reply, host info, and display metadata |
| Input | UDP `47834` | QUIC datagrams | `mykvm.input.v1` | Mouse movement, mouse buttons, scroll, and keyboard events (low latency, loss tolerant) |
| Clipboard | UDP `47834` | QUIC streams | `mykvm.clipboard.v1` | Clipboard text and image sync (reliable, ordered) |

The discovery port is configurable in Settings (default UDP `47833`); the QUIC transport port defaults to the discovery port + 1 (UDP `47834`). Both auto-fall-back through nearby ports if a port is taken, and can use a system-selected port if needed. Peers advertise their active discovery port, QUIC port, transport public key, and protocol version, so discovered and manually added devices connect to the right port and pin the right certificate.

The QUIC connection is TLS 1.3 encrypted: each peer generates a self-signed certificate at startup and advertises it during discovery, and the connecting side pins that certificate, so input and clipboard traffic is encrypted and bound to the advertised peer. Discovery itself is still plaintext and unauthenticated, so keep MyKVM on a trusted LAN.

## Requirements

- Node.js 22+
- Rust stable
- Platform desktop toolchain:
  - Windows: Microsoft C++ Build Tools
  - macOS: Xcode Command Line Tools
  - Linux: WebKitGTK and appindicator development packages

## Development

Install dependencies:

```bash
npm install
```

Run the web UI:

```bash
npm run dev
```

Run the Tauri desktop app:

```bash
npm run tauri:dev
```

Build without bundling installers:

```bash
npm run tauri:build
```

Build desktop bundles:

```bash
npm run tauri:bundle
```

## Platform Helpers

Windows:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\check-dev-env.ps1
powershell -ExecutionPolicy Bypass -File .\scripts\run-tauri-dev.ps1
```

macOS and Linux:

```bash
sh scripts/check-dev-env.sh
sh scripts/run-tauri-dev.sh
```

macOS input capture and injection require Accessibility and Input Monitoring permissions in System Settings.

## Verification

Run these before opening a pull request or cutting a release:

```bash
npm run build
npm run lint
cargo check --manifest-path src-tauri/Cargo.toml
```

## Release

Git itself only stores and pushes source history. GitHub Actions does the actual packaging on GitHub-hosted runners.

The release workflow watches pushes to `main`:

- `feat:` publishes the next minor version, such as `v0.1.0` to `v0.2.0`.
- `fix:` publishes the next patch version, such as `v0.1.0` to `v0.1.1`.
- Other prefixes run normal checks but do not publish a release.
- If no release tag exists yet, the first `feat:` or `fix:` push publishes `v0.1.0`.

Release notes come from the `## [Unreleased]` section of [CHANGELOG.md](./CHANGELOG.md) (user-facing wording), falling back to filtered commit subjects. Keep that section up to date as you land changes.

Example:

```bash
git commit -m "feat: initial desktop release"
git push origin main
```

The workflow creates the git tag, builds macOS, Windows, and Linux bundles, then publishes a GitHub Release with the generated installers.

## Project Layout

| Path | Purpose |
| --- | --- |
| `src/App.tsx` | Main React desktop console |
| `src/desktopApi.ts` | Frontend bridge to Tauri commands |
| `src/layout.ts` | Display layout transforms and adjacency logic |
| `src/runtime.ts` | Runtime status types |
| `src-tauri/src/lib.rs` | Tauri commands, UDP discovery, clipboard sync, app state, and performance sampling |
| `src-tauri/src/input.rs` | Input capture, forwarding, and injection runtime |
| `src-tauri/src/quic_transport.rs` | Encrypted QUIC transport (input datagrams, clipboard streams) with certificate pinning |
| `scripts/` | Development and build helper scripts |

## Contributing

Issues and pull requests are welcome. Keep changes focused, document behavior that affects the protocol, and verify both the web build and the Tauri backend when touching shared runtime code.

See [CONTRIBUTING.md](./CONTRIBUTING.md) for commit prefixes and versioning notes.

## License

MIT. See [LICENSE](./LICENSE).
