# Linux 键鼠捕获与注入 —— 实现方案

## 现状（为什么 issue #11 "Linux server 无反应"）

Linux 目前**捕获和注入两侧都是空 stub**：

- 控制端捕获：`start_platform_capture` 的 non-macOS / non-Windows 分支（`input.rs`）直接
  `remote_active.store(false)` + `unsupported_capture_status()`，从不采集本机键鼠。
- 被控端注入：`inject_mouse_move` / `inject_mouse_button` / `inject_scroll` / `inject_key`
  的 non-macOS / non-Windows 分支（`input.rs:5589+`）是空函数 `{}`。

所以 Linux 既不能当 server（配对显示在线但鼠标划过去没反应，正是 #11），也不能当 client
被 mac/win 控制。**发现 / QUIC 传输 / 配对 / 剪贴板（wl-paste/xclip，clipboard.rs 已支持）都是跨平台的，缺的只有输入 I/O 这一层。**

## 核心难点：X11 与 Wayland 二分

Linux 没有统一的全局输入 API。必须运行时探测会话类型
（`XDG_SESSION_TYPE` == `wayland` / `x11`，或 `WAYLAND_DISPLAY` 是否存在）后走两条完全不同的路径。
建议 **X11 先行**（成熟、单一 API、覆盖面仍大），Wayland 分合成器逐步补齐。

## 键码映射（两条路径共用，先做）

wire 格式是 **Windows 虚拟键码（VK）**（见 [[mykvm-input-architecture]]）。Linux 两侧都要一张映射表：

- 注入（被控端）：VK → X11 keysym → keycode（`XKeysymToKeycode`）；Wayland 下 VK → evdev/xkb keycode。
- 捕获（控制端）：keycode → keysym → VK。

这是 `mac_key_to_windows_vk_pairs` 的 Linux 对应物。先建这张表（纯数据 + 纯函数，**可在 mac 上单测**，是唯一能本机验证的部分），其余平台代码只能在 Linux 上验证。

## X11 路径（阶段 1–2）

依赖：`x11rb`（纯 Rust，features: `xtest` `xinput` `record` `xfixes`）。cfg 仅在 `target_os = "linux"` 编译。

### 注入（阶段 1，最小可用：Linux 当 client）
- `XTestFakeKeyEvent` / `XTestFakeButtonEvent` / `XTestFakeMotionEvent` + `XFlush`。
- 填 `inject_mouse_move/button/scroll/key` 的 linux 分支。滚动 = button 4/5/6/7。
- 复用现有 `InputCommand` 分发；只是出口从 mac CGEvent / win SendInput 换成 XTEST。

### 捕获（阶段 2：Linux 当 server）
- 全局键鼠：XInput2 raw 事件（`XI_RawMotion` / `XI_RawButtonPress` / `XI_RawKeyPress`）在 root 窗口 select；
  或 XRecord 扩展。raw 事件拿的是设备增量，正好对应现有"解耦 HID 增量"模型（mac 同思路）。
- 边缘穿越回锚：`XWarpPointer` 把指针拉回锚点（类比 Windows warp-back-each-event）。
- 本机指针隐藏：`XFixesHideCursor`（控制远端时藏本地光标）。
- 复用 `send_packet` 转发（wire 不变）、`build_input_targets` / `is_crossing_screen` 等纯逻辑跨平台。

## Wayland 路径（阶段 3–4，碎片化）

Wayland 安全模型禁止应用随意全局抓输入，各合成器接口不一，**运行时探测可用协议**。

### 注入（阶段 3：先覆盖 wlroots 系）
- wlroots（Hyprland / niri / sway）：`zwlr_virtual_pointer_v1` + `zwlr_virtual_keyboard_v1`。
- GNOME / KDE：走 `libei`（经 xdg-desktop-portal 的 RemoteDesktop portal）。
- 依赖：`wayland-client` + wlr-protocols，或 `ashpd`（portal）+ `libei`。

### 捕获（阶段 4：最难）
- 无合成器专有扩展时，唯一可移植方案是 xdg-desktop-portal 的 **InputCapture** portal（GNOME 46+/KDE 较新），
  经 `libei` 收事件。老合成器或无 portal 环境**可能永远无法无障碍全局捕获**——这是 Wayland 的设计限制，非本项目缺陷，需在 UI 明说。

## 与现有架构对接（改动点集中、小）

- `start_platform_capture`（linux 分支）：起 X11/Wayland 捕获线程，复用 `send_packet`。
- `inject_mouse_*` / `inject_key`（linux 分支）：X11 XTEST / Wayland virtual-input。
- `input_receive_status`（linux）：探测会话类型 + 所需权限（X11 需 XTEST；Wayland 需 portal 授权），
  给出可操作的中文错误（对标 mac 的辅助功能提示）。
- 边缘穿越 / 屏幕布局 / 键码 wire / 剪贴板：**全部已跨平台，不动**。

## 分阶段与验证（必须在真实 Linux 桌面）

| 阶段 | 内容 | 验证 |
|---|---|---|
| 0 | VK↔keysym 映射表 | **mac 可单测**（纯函数） |
| 1 | X11 XTEST 注入 | X11 会话：mac/win 控制 Linux client |
| 2 | X11 XInput2/XRecord 捕获 + warp/hide | X11 会话：Linux server 控制 mac/win |
| 3 | Wayland 注入（wlroots→portal） | Hyprland/niri/sway，再 GNOME/KDE |
| 4 | Wayland 捕获（InputCapture portal） | GNOME 46+/KDE，标注不支持的环境 |

## 限制（要在文档/UI 说明）

- **本机无法验证**：`cfg(target_os = "linux")` 的代码在开发用 mac 上不参与编译，语法错误都发现不了
  （同 [[mac-code-signing]] 里 ring 交叉编译问题）。除阶段 0 的映射表外，必须在 Linux 环境开发+联调。
- Wayland 全局捕获受合成器与 portal 支持限制，非全环境可用。
- 建议按阶段独立 PR，每阶段能在对应会话类型下真实驱动一条端到端流程再合并。

见 [[mykvm-input-architecture]]、[[mykvm-perf-clipboard-fixes]]。
