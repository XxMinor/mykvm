//! The file-type icon that follows the cursor while a drag from the controller
//! is over this machine, with a progress bar for the bytes still streaming in.
//! After the drop it docks above the taskbar until the transfer completes.
//!
//! A click-through, per-pixel-alpha layered window on its own thread, so it
//! keeps moving while `DoDragDrop` runs its modal loop on the drag thread.
//! Runtime behavior needs verification on real Windows hardware.
#![cfg(target_os = "windows")]

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
    SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DIB_RGB_COLORS,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetIconInfo, GetMessageW, PostQuitMessage, RegisterClassW, SetTimer, ShowWindow,
    SystemParametersInfoW, UpdateLayeredWindow, ICONINFO, MSG, SPI_GETWORKAREA, SW_SHOWNOACTIVATE,
    ULW_ALPHA, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const WIDTH: i32 = 44;
const HEIGHT: i32 = 48;
const ICON: i32 = 32;
const ICON_LEFT: i32 = (WIDTH - ICON) / 2;
const BAR_TOP: i32 = ICON + 6;
const BAR_HEIGHT: i32 = 5;

const FOLLOW: u8 = 0;
const DOCKED: u8 = 1;
const CLOSE: u8 = 2;

/// Handle to a running overlay; dropping it closes the window.
pub struct Overlay {
    mode: Arc<AtomicU8>,
}

impl Overlay {
    /// The drop happened but bytes are still arriving: park above the taskbar.
    pub fn dock(&self) {
        self.mode.store(DOCKED, Ordering::Relaxed);
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.mode.store(CLOSE, Ordering::Relaxed);
    }
}

/// Show the overlay for `file_name` (its type decides the icon). `progress`
/// returns the received fraction, 0.0..=1.0.
pub fn start(file_name: &str, progress: Arc<dyn Fn() -> f32 + Send + Sync>) -> Overlay {
    let mode = Arc::new(AtomicU8::new(FOLLOW));
    let thread_mode = Arc::clone(&mode);
    let file_name = file_name.to_string();
    std::thread::spawn(move || unsafe { run(&file_name, &thread_mode, &*progress) });
    Overlay { mode }
}

unsafe fn run(file_name: &str, mode: &AtomicU8, progress: &dyn Fn() -> f32) {
    // SHGetFileInfo wants COM initialized on its thread.
    let _ = windows::Win32::System::Ole::OleInitialize(None);
    let instance = GetModuleHandleW(std::ptr::null());
    let class = wide("MyKVMDragOverlay");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(DefWindowProcW),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..std::mem::zeroed()
    };
    RegisterClassW(&window_class); // Fails harmlessly once already registered.
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        std::ptr::null(),
        WS_POPUP,
        0,
        0,
        WIDTH,
        HEIGHT,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        instance,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        log::warn!("drag overlay: could not create its window");
        return;
    }
    let icon = file_icon_pixels(file_name);
    let mut shown_progress = progress();
    render(hwnd, icon.as_deref(), shown_progress, position(mode.load(Ordering::Relaxed)));
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, 1, 15, None);

    let mut message: MSG = std::mem::zeroed();
    while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
        if message.message == WM_TIMER && message.hwnd == hwnd {
            let current = mode.load(Ordering::Relaxed);
            if current == CLOSE {
                DestroyWindow(hwnd);
                PostQuitMessage(0);
                continue;
            }
            let at = position(current);
            let now = progress();
            if (now - shown_progress).abs() >= 0.005 {
                shown_progress = now;
                render(hwnd, icon.as_deref(), now, at);
            } else {
                // Move only: no source DC means no repaint.
                let point = POINT { x: at.0, y: at.1 };
                UpdateLayeredWindow(
                    hwnd,
                    std::ptr::null_mut(),
                    &point,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                );
            }
            continue;
        }
        DispatchMessageW(&message);
    }
    windows::Win32::System::Ole::OleUninitialize();
}

/// Beside the cursor while dragging; above the taskbar's right end once docked.
unsafe fn position(mode: u8) -> (i32, i32) {
    if mode == DOCKED {
        let mut area: RECT = std::mem::zeroed();
        if SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut area as *mut RECT).cast(), 0) != 0 {
            return (area.right - WIDTH - 16, area.bottom - HEIGHT - 16);
        }
    }
    let mut cursor = POINT { x: 0, y: 0 };
    GetCursorPos(&mut cursor);
    (cursor.x + 14, cursor.y + 18)
}

unsafe fn render(hwnd: HWND, icon: Option<&[u32]>, progress: f32, at: (i32, i32)) {
    let screen = GetDC(std::ptr::null_mut());
    let memory = CreateCompatibleDC(screen);
    let mut info = bitmap_info(WIDTH, HEIGHT);
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let bitmap = CreateDIBSection(memory, &mut info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if bitmap.is_null() || bits.is_null() {
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
        return;
    }
    let previous = SelectObject(memory, bitmap);
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (WIDTH * HEIGHT) as usize);
    pixels.fill(0);
    paint(pixels, icon, progress);

    let size = SIZE { cx: WIDTH, cy: HEIGHT };
    let source = POINT { x: 0, y: 0 };
    let destination = POINT { x: at.0, y: at.1 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    UpdateLayeredWindow(hwnd, screen, &destination, &size, memory, &source, 0, &blend, ULW_ALPHA);

    SelectObject(memory, previous);
    DeleteObject(bitmap);
    DeleteDC(memory);
    ReleaseDC(std::ptr::null_mut(), screen);
}

/// Premultiplied BGRA into the top-down `WIDTH` x `HEIGHT` canvas.
fn paint(pixels: &mut [u32], icon: Option<&[u32]>, progress: f32) {
    let put = |pixels: &mut [u32], x: i32, y: i32, value: u32| {
        if (0..WIDTH).contains(&x) && (0..HEIGHT).contains(&y) {
            pixels[(y * WIDTH + x) as usize] = value;
        }
    };
    match icon {
        Some(icon) => {
            for y in 0..ICON {
                for x in 0..ICON {
                    put(pixels, ICON_LEFT + x, y, icon[(y * ICON + x) as usize]);
                }
            }
        }
        // No shell icon: a plain page so something still follows the cursor.
        None => {
            for y in 2..ICON - 2 {
                for x in 6..ICON - 6 {
                    put(pixels, ICON_LEFT + x, y, 0xF0F0_F0F0);
                }
            }
        }
    }
    if progress >= 1.0 {
        return;
    }
    let bar = WIDTH - 8;
    let filled = (bar as f32 * progress.clamp(0.0, 1.0)).round() as i32;
    for y in BAR_TOP..BAR_TOP + BAR_HEIGHT {
        for x in 0..bar {
            // Accent blue for received bytes over a translucent dark track.
            let value = if x < filled { 0xFF2F_7AF8 } else { 0xB000_0000 };
            put(pixels, 4 + x, y, value);
        }
    }
}

/// The 32x32 shell icon for `file_name`'s type as premultiplied BGRA.
unsafe fn file_icon_pixels(file_name: &str) -> Option<Vec<u32>> {
    let extension = Path::new(file_name)
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_else(|| "file".into());
    let extension = wide(&extension);
    let mut file_info: SHFILEINFOW = std::mem::zeroed();
    let found = SHGetFileInfoW(
        extension.as_ptr(),
        FILE_ATTRIBUTE_NORMAL,
        &mut file_info,
        std::mem::size_of::<SHFILEINFOW>() as u32,
        SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
    );
    if found == 0 || file_info.hIcon.is_null() {
        return None;
    }
    let mut icon_info: ICONINFO = std::mem::zeroed();
    if GetIconInfo(file_info.hIcon, &mut icon_info) == 0 {
        DestroyIcon(file_info.hIcon);
        return None;
    }
    let screen = GetDC(std::ptr::null_mut());
    let mut info = bitmap_info(ICON, ICON);
    let mut color = vec![0u32; (ICON * ICON) as usize];
    let read = GetDIBits(
        screen,
        icon_info.hbmColor,
        0,
        ICON as u32,
        color.as_mut_ptr().cast(),
        &mut info,
        DIB_RGB_COLORS,
    );
    // Icons without an alpha channel carry transparency in their mask.
    if read != 0 && color.iter().all(|pixel| pixel >> 24 == 0) {
        let mut mask = vec![0u32; (ICON * ICON) as usize];
        let mut mask_info = bitmap_info(ICON, ICON);
        if GetDIBits(
            screen,
            icon_info.hbmMask,
            0,
            ICON as u32,
            mask.as_mut_ptr().cast(),
            &mut mask_info,
            DIB_RGB_COLORS,
        ) != 0
        {
            for (pixel, mask) in color.iter_mut().zip(&mask) {
                if mask & 0x00FF_FFFF == 0 {
                    *pixel |= 0xFF00_0000;
                }
            }
        }
    }
    ReleaseDC(std::ptr::null_mut(), screen);
    DeleteObject(icon_info.hbmColor);
    DeleteObject(icon_info.hbmMask);
    DestroyIcon(file_info.hIcon);
    if read == 0 {
        return None;
    }
    for pixel in color.iter_mut() {
        *pixel = premultiply(*pixel);
    }
    Some(color)
}

fn premultiply(pixel: u32) -> u32 {
    let alpha = pixel >> 24;
    let scale = |shift: u32| (((pixel >> shift) & 0xFF) * alpha / 255) << shift;
    (alpha << 24) | scale(16) | scale(8) | scale(0)
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        biHeight: -height, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    info
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply_scales_color_by_alpha() {
        assert_eq!(premultiply(0xFFFF_8040), 0xFFFF_8040);
        assert_eq!(premultiply(0x00FF_FFFF), 0);
        assert_eq!(premultiply(0x80FF_0000), 0x8080_0000);
    }

    #[test]
    fn progress_bar_fills_proportionally_and_hides_when_done() {
        let mut pixels = vec![0u32; (WIDTH * HEIGHT) as usize];
        paint(&mut pixels, None, 0.5);
        let row = |pixels: &[u32]| pixels[(BAR_TOP * WIDTH) as usize..][..WIDTH as usize].to_vec();
        let bar = row(&pixels);
        let filled = bar.iter().filter(|pixel| **pixel == 0xFF2F_7AF8).count() as i32;
        assert_eq!(filled, (WIDTH - 8) / 2);
        let mut done = vec![0u32; (WIDTH * HEIGHT) as usize];
        paint(&mut done, None, 1.0);
        assert!(row(&done).iter().all(|pixel| *pixel == 0));
    }
}
