use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};

#[cfg(target_os = "linux")]
fn with_linux_clipboard<T>(
    operation: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
) -> Result<T, String> {
    // Linux clipboards are ownership based: dropping the object that performed
    // a write can make the copied data disappear. Keep one process-lifetime
    // owner and serialize poll/receive threads through it.
    static CLIPBOARD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();
    let mut clipboard = CLIPBOARD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "linux clipboard lock poisoned".to_string())?;
    if clipboard.is_none() {
        *clipboard = Some(
            arboard::Clipboard::new()
                .map_err(|error| format!("failed to open clipboard: {error}"))?,
        );
    }
    operation(clipboard.as_mut().expect("clipboard just initialized"))
        .map_err(|error| format!("linux clipboard operation failed: {error}"))
}

#[cfg(target_os = "linux")]
struct LinuxClipboardChangeTracker {
    token: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl LinuxClipboardChangeTracker {
    fn start(initial_token: u64) -> Result<Self, String> {
        use x11rb::{
            connection::Connection as _,
            protocol::{
                xfixes::{self, ConnectionExt as _, SelectionEventMask},
                xproto::ConnectionExt as _,
                Event,
            },
        };

        let (connection, screen_number) = x11rb::connect(None)
            .map_err(|error| format!("failed to connect to X11 for clipboard tracking: {error}"))?;
        let root = connection
            .setup()
            .roots
            .get(screen_number)
            .ok_or_else(|| "X11 clipboard tracker could not find a screen".to_string())?
            .root;
        let clipboard = connection
            .intern_atom(false, b"CLIPBOARD")
            .map_err(|error| format!("failed to intern X11 CLIPBOARD atom: {error}"))?
            .reply()
            .map_err(|error| format!("failed to read X11 CLIPBOARD atom: {error}"))?
            .atom;

        // XFixes reports every SetSelectionOwner operation, including a new
        // copy made by an application which keeps using the same owner window.
        // Polling GetSelectionOwner alone misses that common case.
        xfixes::query_version(&connection, 5, 0)
            .map_err(|error| format!("failed to query XFixes: {error}"))?
            .reply()
            .map_err(|error| format!("XFixes is unavailable: {error}"))?;
        connection
            .xfixes_select_selection_input(
                root,
                clipboard,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .map_err(|error| format!("failed to subscribe to X11 clipboard changes: {error}"))?
            .check()
            .map_err(|error| format!("X11 rejected clipboard change subscription: {error}"))?;
        connection
            .flush()
            .map_err(|error| format!("failed to start X11 clipboard tracker: {error}"))?;

        let token = Arc::new(AtomicU64::new(initial_token));
        let alive = Arc::new(AtomicBool::new(true));
        let thread_token = Arc::clone(&token);
        let thread_alive = Arc::clone(&alive);
        std::thread::Builder::new()
            .name("mykvm-clipboard-xfixes".into())
            .spawn(move || {
                while let Ok(event) = connection.wait_for_event() {
                    if matches!(
                        event,
                        Event::XfixesSelectionNotify(event) if event.selection == clipboard
                    ) {
                        // This counter is process-local; wrapping after 2^64
                        // clipboard changes is harmless, but keep zero reserved
                        // so initialization is easy to distinguish in tests.
                        let _ = thread_token.fetch_update(
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                            |value| Some(value.wrapping_add(1).max(1)),
                        );
                    }
                }
                thread_alive.store(false, Ordering::Release);
            })
            .map_err(|error| format!("failed to spawn X11 clipboard tracker: {error}"))?;

        Ok(Self { token, alive })
    }

    fn current(&self) -> Option<u64> {
        self.alive
            .load(Ordering::Acquire)
            .then(|| self.token.load(Ordering::Relaxed))
    }
}

#[cfg(target_os = "linux")]
fn linux_wayland_session(
    wayland_display: Option<&std::ffi::OsStr>,
    session_type: Option<&str>,
) -> bool {
    wayland_display.is_some()
        || session_type.is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
}

const CLIPBOARD_MAX_TEXT_BYTES: usize = 256 * 1024;
// Raw RGBA can be large (a 2560x1440 frame is ~14 MB); cap it so a stray huge
// copy never floods the LAN transport. Images above this are skipped.
const CLIPBOARD_MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClipboardImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba_base64: String,
}

/// One unit of clipboard content read from (or written to) the local system.
pub(crate) enum ClipboardContent {
    Text(String),
    Image(ClipboardImage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardContentHint {
    Image,
    Text,
    Unknown,
}

/// Run an AppKit clipboard operation safely off the main thread. Two things are
/// required and were missing: (1) an autorelease pool, or the autoreleased
/// NSPasteboard objects are never drained and objc_msgSend eventually hits a
/// freed pointer (the SIGSEGV in NSPasteboard._updateTypeCacheIfNeeded); and
/// (2) serialization, because the clipboard poll thread and the QUIC receive
/// thread both touch the shared general pasteboard and concurrent access
/// crashes inside AppKit.
fn with_clipboard_appkit<T>(f: impl FnOnce() -> T) -> T {
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    macos_autorelease_pool(f)
}

#[cfg(target_os = "macos")]
fn macos_autorelease_pool<T>(f: impl FnOnce() -> T) -> T {
    use std::os::raw::c_void;
    extern "C" {
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }
    struct PoolGuard(*mut c_void);
    impl Drop for PoolGuard {
        fn drop(&mut self) {
            unsafe { objc_autoreleasePoolPop(self.0) }
        }
    }
    // Drops (drains) even if `f` panics, so a bad clipboard payload cannot leak
    // the pool or unbalance it.
    let _pool = PoolGuard(unsafe { objc_autoreleasePoolPush() });
    f()
}

#[cfg(not(target_os = "macos"))]
fn macos_autorelease_pool<T>(f: impl FnOnce() -> T) -> T {
    f()
}

fn clipboard_signature_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

impl ClipboardContent {
    pub(crate) fn is_oversized(&self) -> bool {
        match self {
            ClipboardContent::Text(text) => text.len() > CLIPBOARD_MAX_TEXT_BYTES,
            ClipboardContent::Image(image) => {
                // base64 inflates ~4/3; compare against the decoded RGBA budget.
                image.rgba_base64.len() / 4 * 3 > CLIPBOARD_MAX_IMAGE_BYTES
            }
        }
    }

    /// A stable fingerprint used to detect "did the clipboard change" and to
    /// suppress echoing content we just received from a peer.
    pub(crate) fn signature(&self) -> String {
        match self {
            ClipboardContent::Text(text) => format!("text:{text}"),
            ClipboardContent::Image(image) => {
                format!(
                    "image:{}x{}:{}:{:016x}",
                    image.width,
                    image.height,
                    image.rgba_base64.len(),
                    clipboard_signature_hash(image.rgba_base64.as_bytes())
                )
            }
        }
    }
}

/// A cheap OS-level clipboard change counter. When two calls return the same
/// token the clipboard has not changed in between, so callers can skip the
/// expensive full read (image decode + base64) entirely. `None` means the
/// platform has no cheap counter and callers must fall back to reading.
#[cfg(target_os = "macos")]
pub(crate) fn change_token() -> Option<u64> {
    use std::os::raw::{c_char, c_void};

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    with_clipboard_appkit(|| unsafe {
        let pasteboard_class = objc_getClass(b"NSPasteboard\0".as_ptr() as *const c_char);
        if pasteboard_class.is_null() {
            return None;
        }
        let general_sel = sel_registerName(b"generalPasteboard\0".as_ptr() as *const c_char);
        let get_object: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let pasteboard = get_object(pasteboard_class, general_sel);
        if pasteboard.is_null() {
            return None;
        }
        let change_count_sel = sel_registerName(b"changeCount\0".as_ptr() as *const c_char);
        let get_isize: extern "C" fn(*mut c_void, *mut c_void) -> isize =
            std::mem::transmute(objc_msgSend as *const ());
        Some(get_isize(pasteboard, change_count_sel) as u64)
    })
}

#[cfg(target_os = "windows")]
pub(crate) fn change_token() -> Option<u64> {
    use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
    Some(u64::from(unsafe { GetClipboardSequenceNumber() }))
}

#[cfg(target_os = "linux")]
pub(crate) fn change_token() -> Option<u64> {
    static TRACKER: OnceLock<Mutex<Option<LinuxClipboardChangeTracker>>> = OnceLock::new();
    static LAST_FAILURE: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
    static WAYLAND_WARNING: OnceLock<()> = OnceLock::new();

    if linux_wayland_session(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
    ) {
        WAYLAND_WARNING.get_or_init(|| {
            log::warn!(
                "Wayland has no MyKVM clipboard change counter; clipboard polling falls back to full reads. Linux remote input currently requires an Xorg session."
            );
        });
        return None;
    }

    let mut tracker = TRACKER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(token) = tracker
        .as_ref()
        .and_then(LinuxClipboardChangeTracker::current)
    {
        return Some(token);
    }

    let mut last_failure = LAST_FAILURE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if last_failure.is_some_and(|failed_at| failed_at.elapsed() < std::time::Duration::from_secs(5))
    {
        return None;
    }

    // A replacement connection must never restart at the previous token or
    // the sync loop could incorrectly treat a post-reconnect clipboard as
    // unchanged.
    let initial_token = tracker
        .as_ref()
        .map(|old| old.token.load(Ordering::Relaxed).wrapping_add(1).max(1))
        .unwrap_or(1);
    match LinuxClipboardChangeTracker::start(initial_token) {
        Ok(started) => {
            let token = started.current();
            *tracker = Some(started);
            *last_failure = None;
            token
        }
        Err(error) => {
            log::warn!("Linux clipboard change tracking unavailable: {error}");
            *last_failure = Some(std::time::Instant::now());
            None
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub(crate) fn change_token() -> Option<u64> {
    None
}

pub(crate) fn read_text() -> Result<String, String> {
    read_system_text()
}

pub(crate) fn write_text(text: &str) -> Result<(), String> {
    write_system_text(text)
}

pub(crate) fn write_content(content: &ClipboardContent) -> Result<(), String> {
    match content {
        ClipboardContent::Text(text) => write_text(text),
        ClipboardContent::Image(image) => write_image(image),
    }
}

/// Writes every representation of a received copy back to the clipboard at once.
/// A single item takes the plain arboard path. Multi-format copies use a native
/// platform write where available; a backend which cannot publish the whole
/// set must use an explicit, diagnostic fallback rather than silently clearing
/// an earlier representation with a second arboard call.
pub(crate) fn write_contents(contents: &[ClipboardContent]) -> Result<(), String> {
    match contents {
        [] => Ok(()),
        [single] => write_content(single),
        multiple => write_multiple_contents(multiple),
    }
}

#[cfg(target_os = "macos")]
fn write_multiple_contents(contents: &[ClipboardContent]) -> Result<(), String> {
    use std::os::raw::{c_char, c_void};

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    let text = contents.iter().find_map(|content| match content {
        ClipboardContent::Text(text) if !text.is_empty() => Some(text.as_str()),
        _ => None,
    });
    // ponytail: declareTypes carries one image representation (covers text+image
    // rich copies); multi-image via NSPasteboardItem is the next increment.
    let image_png = contents
        .iter()
        .find_map(|content| match content {
            ClipboardContent::Image(image) => Some(image),
            _ => None,
        })
        .and_then(clipboard_image_to_png);

    if text.is_none() && image_png.is_none() {
        return Ok(());
    }

    with_clipboard_appkit(|| unsafe {
        let pasteboard_class = objc_getClass(b"NSPasteboard\0".as_ptr() as *const c_char);
        let nsstring_class = objc_getClass(b"NSString\0".as_ptr() as *const c_char);
        let nsdata_class = objc_getClass(b"NSData\0".as_ptr() as *const c_char);
        let nsarray_class = objc_getClass(b"NSArray\0".as_ptr() as *const c_char);
        if pasteboard_class.is_null()
            || nsstring_class.is_null()
            || nsdata_class.is_null()
            || nsarray_class.is_null()
        {
            return Err("AppKit classes unavailable".into());
        }
        let msg0: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let general_sel = sel_registerName(b"generalPasteboard\0".as_ptr() as *const c_char);
        let pasteboard = msg0(pasteboard_class, general_sel);
        if pasteboard.is_null() {
            return Err("no general pasteboard".into());
        }

        // NSString type identifiers (UTIs) for the representations we set.
        let type_string = ns_string(nsstring_class, "public.utf8-plain-text");
        let type_png = ns_string(nsstring_class, "public.png");

        // Build the declareTypes array from whichever representations we have.
        let make_array: extern "C" fn(
            *mut c_void,
            *mut c_void,
            *const *mut c_void,
            usize,
        ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
        let array_sel = sel_registerName(b"arrayWithObjects:count:\0".as_ptr() as *const c_char);
        let mut type_objs: Vec<*mut c_void> = Vec::new();
        if text.is_some() {
            type_objs.push(type_string);
        }
        if image_png.is_some() {
            type_objs.push(type_png);
        }
        let types = make_array(
            nsarray_class,
            array_sel,
            type_objs.as_ptr(),
            type_objs.len(),
        );

        // declareTypes:owner:
        let declare_sel = sel_registerName(b"declareTypes:owner:\0".as_ptr() as *const c_char);
        let declare: extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> isize =
            std::mem::transmute(objc_msgSend as *const ());
        declare(pasteboard, declare_sel, types, std::ptr::null_mut());

        let mut ok = false;
        if let Some(text) = text {
            let value = ns_string(nsstring_class, text);
            let set_string_sel =
                sel_registerName(b"setString:forType:\0".as_ptr() as *const c_char);
            let set_string: extern "C" fn(
                *mut c_void,
                *mut c_void,
                *mut c_void,
                *mut c_void,
            ) -> bool = std::mem::transmute(objc_msgSend as *const ());
            ok |= set_string(pasteboard, set_string_sel, value, type_string);
        }
        if let Some(png) = image_png.as_ref() {
            let data_sel = sel_registerName(b"dataWithBytes:length:\0".as_ptr() as *const c_char);
            let make_data: extern "C" fn(
                *mut c_void,
                *mut c_void,
                *const u8,
                usize,
            ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
            let data = make_data(nsdata_class, data_sel, png.as_ptr(), png.len());
            let set_data_sel = sel_registerName(b"setData:forType:\0".as_ptr() as *const c_char);
            let set_data: extern "C" fn(
                *mut c_void,
                *mut c_void,
                *mut c_void,
                *mut c_void,
            ) -> bool = std::mem::transmute(objc_msgSend as *const ());
            ok |= set_data(pasteboard, set_data_sel, data, type_png);
        }

        if ok {
            Ok(())
        } else {
            Err("failed to write clipboard representations".into())
        }
    })
}

#[cfg(target_os = "macos")]
unsafe fn ns_string(
    nsstring_class: *mut std::os::raw::c_void,
    value: &str,
) -> *mut std::os::raw::c_void {
    use std::os::raw::{c_char, c_void};
    #[link(name = "objc")]
    extern "C" {
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }
    // stringWithBytes:length:encoding: with NSUTF8StringEncoding (4).
    let sel = sel_registerName(b"stringWithBytes:length:encoding:\0".as_ptr() as *const c_char);
    let make: extern "C" fn(*mut c_void, *mut c_void, *const u8, usize, usize) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    make(nsstring_class, sel, value.as_ptr(), value.len(), 4)
}

/// Encodes an RGBA clipboard image to PNG bytes for native pasteboard writes.
#[cfg(target_os = "macos")]
fn clipboard_image_to_png(image: &ClipboardImage) -> Option<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};

    let bytes = BASE64.decode(image.rgba_base64.as_bytes()).ok()?;
    let width = image.width;
    let height = image.height;
    if width == 0
        || height == 0
        || bytes.len()
            != (width as usize)
                .checked_mul(height as usize)?
                .checked_mul(4)?
    {
        return None;
    }
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&bytes, width, height, ExtendedColorType::Rgba8)
        .ok()?;
    Some(png)
}

#[cfg(target_os = "windows")]
fn write_multiple_contents(contents: &[ClipboardContent]) -> Result<(), String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
    use windows_sys::Win32::System::Ole::{CF_DIBV5, CF_UNICODETEXT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE,
    };

    let text = contents.iter().find_map(|content| match content {
        ClipboardContent::Text(text) if !text.is_empty() => Some(text.as_str()),
        _ => None,
    });
    let image = contents.iter().find_map(|content| match content {
        ClipboardContent::Image(image) => Some(image),
        _ => None,
    });
    let image_formats = image.map(clipboard_image_to_windows_formats).transpose()?;
    if text.is_none() && image_formats.is_none() {
        return Ok(());
    }

    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    struct OwnerWindowGuard(windows_sys::Win32::Foundation::HWND);
    impl Drop for OwnerWindowGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    unsafe fn set_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
        let handle = GlobalAlloc(GHND, bytes.len());
        if handle.is_null() {
            return Err("failed to allocate Windows clipboard memory".into());
        }
        let pointer = GlobalLock(handle);
        if pointer.is_null() {
            let _ = GlobalFree(handle);
            return Err("failed to lock Windows clipboard memory".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(handle);

        // On success ownership of the HGLOBAL transfers to the clipboard.
        if SetClipboardData(format, handle) == std::ptr::null_mut() {
            let _ = GlobalFree(handle);
            return Err(format!("failed to set Windows clipboard format {format}"));
        }
        Ok(())
    }

    // EmptyClipboard assigns ownership to the HWND passed to OpenClipboard.
    // Passing NULL leaves no owner and makes SetClipboardData fail, so create a
    // tiny message-only window owned by this clipboard worker thread.
    let class = crate::wide_null("STATIC");
    let name = crate::wide_null("MyKVM Clipboard Owner");
    let owner = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if owner.is_null() {
        return Err("failed to create Windows clipboard owner window".into());
    }
    let _owner_guard = OwnerWindowGuard(owner);

    if unsafe { OpenClipboard(owner) } == 0 {
        return Err("failed to open Windows clipboard".into());
    }
    let _guard = ClipboardGuard;
    if unsafe { EmptyClipboard() } == 0 {
        return Err("failed to clear Windows clipboard".into());
    }

    if let Some(text) = text {
        let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = unsafe {
            std::slice::from_raw_parts(
                utf16.as_ptr().cast::<u8>(),
                utf16.len() * std::mem::size_of::<u16>(),
            )
        };
        unsafe { set_bytes(u32::from(CF_UNICODETEXT), bytes)? };
    }
    if let Some(image_formats) = image_formats.as_ref() {
        let png_format = unsafe { RegisterClipboardFormatW(crate::wide_null("PNG").as_ptr()) };
        if png_format != 0 {
            unsafe { set_bytes(png_format, &image_formats.png)? };
        }
        unsafe { set_bytes(u32::from(CF_DIBV5), &image_formats.dib)? };
    }

    Ok(())
}

#[cfg(target_os = "windows")]
struct WindowsClipboardImageFormats {
    png: Vec<u8>,
    dib: Vec<u8>,
}

#[cfg(target_os = "windows")]
fn clipboard_image_to_windows_formats(
    image: &ClipboardImage,
) -> Result<WindowsClipboardImageFormats, String> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};
    use windows_sys::Win32::Graphics::Gdi::{BITMAPV5HEADER, BI_BITFIELDS};

    let rgba = BASE64
        .decode(image.rgba_base64.as_bytes())
        .map_err(|error| format!("failed to decode clipboard image: {error}"))?;
    let (_, image_bytes) = rgba_dimensions(&rgba, image.width, image.height)?;
    let image_size = u32::try_from(image_bytes).map_err(|_| "clipboard image is too large")?;
    let header = BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: image.width as i32,
        bV5Height: image.height as i32,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5SizeImage: image_size,
        bV5RedMask: 0x00ff_0000,
        bV5GreenMask: 0x0000_ff00,
        bV5BlueMask: 0x0000_00ff,
        bV5AlphaMask: 0xff00_0000,
        bV5CSType: 0x7352_4742, // LCS_sRGB
        bV5Intent: 4,           // LCS_GM_IMAGES
        ..BITMAPV5HEADER::default()
    };

    let header_size = std::mem::size_of::<BITMAPV5HEADER>();
    let mut dib = vec![0_u8; header_size + image_bytes];
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&header as *const BITMAPV5HEADER).cast::<u8>(),
            dib.as_mut_ptr(),
            header_size,
        );
    }
    write_bgra_bottom_up(&rgba, image.width, image.height, &mut dib[header_size..])?;

    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&rgba, image.width, image.height, ExtendedColorType::Rgba8)
        .map_err(|error| format!("failed to encode clipboard PNG: {error}"))?;

    Ok(WindowsClipboardImageFormats { png, dib })
}

#[cfg(any(target_os = "windows", test))]
fn rgba_to_bgra_bottom_up(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let (_, expected) = rgba_dimensions(rgba, width, height)?;
    let mut bgra = vec![0_u8; expected];
    write_bgra_bottom_up(rgba, width, height, &mut bgra)?;
    Ok(bgra)
}

#[cfg(any(target_os = "windows", test))]
fn rgba_dimensions(rgba: &[u8], width: u32, height: u32) -> Result<(usize, usize), String> {
    let row_bytes = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| "clipboard image dimensions overflow".to_string())?;
    let expected = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| "clipboard image dimensions overflow".to_string())?;
    if width == 0 || height == 0 || rgba.len() != expected {
        return Err("clipboard image has invalid dimensions".into());
    }
    Ok((row_bytes, expected))
}

#[cfg(any(target_os = "windows", test))]
fn write_bgra_bottom_up(
    rgba: &[u8],
    width: u32,
    height: u32,
    bgra: &mut [u8],
) -> Result<(), String> {
    let (row_bytes, expected) = rgba_dimensions(rgba, width, height)?;
    if bgra.len() != expected {
        return Err("clipboard image output has invalid dimensions".into());
    }
    let mut offset = 0;
    for row in rgba.chunks_exact(row_bytes).rev() {
        for pixel in row.chunks_exact(4) {
            bgra[offset..offset + 4].copy_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            offset += 4;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const LINUX_RICH_CLIPBOARD_DEGRADATION: &str = "arboard 3.6 cannot atomically publish text and image/png on Linux; MyKVM preserved the image and omitted its plain-text companion";

#[cfg(target_os = "linux")]
fn linux_multi_format_fallback(
    contents: &[ClipboardContent],
) -> (Option<&ClipboardContent>, Option<&'static str>) {
    let image = contents
        .iter()
        .find(|content| matches!(content, ClipboardContent::Image(_)));
    let text = contents
        .iter()
        .find(|content| matches!(content, ClipboardContent::Text(_)));

    match (image, text) {
        // arboard's Linux X11 backend internally supports a list of selection
        // targets, but its public API only exposes text+HTML as a pair. Calling
        // set_text then set_image clears the first target. Prefer the native
        // image representation (which cannot be reconstructed from plain text)
        // and make the loss visible in logs instead of silently claiming full
        // rich-copy support.
        (Some(image), Some(_)) => (Some(image), Some(LINUX_RICH_CLIPBOARD_DEGRADATION)),
        (Some(image), None) => (Some(image), None),
        (None, Some(text)) => (Some(text), None),
        (None, None) => (None, None),
    }
}

#[cfg(target_os = "linux")]
fn write_multiple_contents(contents: &[ClipboardContent]) -> Result<(), String> {
    let (selected, diagnostic) = linux_multi_format_fallback(contents);
    if let Some(diagnostic) = diagnostic {
        log::warn!("Linux rich clipboard degraded: {diagnostic}");
    }
    match selected {
        Some(content) => write_content(content),
        None => Ok(()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn write_multiple_contents(contents: &[ClipboardContent]) -> Result<(), String> {
    contents.first().map_or(Ok(()), write_content)
}

/// Reads whatever is currently on the clipboard. The shared policy lives here:
/// when the platform can identify a current image format, wait for an image
/// read instead of falling back to stale text from a previous clipboard format.
pub(crate) fn read_content() -> Option<ClipboardContent> {
    read_content_for_hint(content_hint(), read_text_content, read_image_content)
}

/// Reads *every* representation on the clipboard, not just one, so a rich copy
/// that carries both text and an image (or several images) travels intact
/// instead of losing all but the first. A fresh copy always clears the previous
/// contents, so reading both text and image here cannot mix stale data.
pub(crate) fn read_contents() -> Vec<ClipboardContent> {
    read_contents_for_hint(content_hint(), read_text_content, read_all_image_contents)
}

fn read_contents_for_hint<F, G>(
    hint: ClipboardContentHint,
    mut read_text: F,
    mut read_images: G,
) -> Vec<ClipboardContent>
where
    F: FnMut() -> Option<ClipboardContent>,
    G: FnMut() -> Vec<ClipboardContent>,
{
    let mut contents = Vec::new();
    // Windows reports Image as soon as any image representation exists, even
    // when CF_UNICODETEXT exists alongside it. Always probe text for the
    // multi-format path; a missing representation simply returns None.
    contents.extend(read_text());
    if !matches!(hint, ClipboardContentHint::Text) {
        contents.extend(read_images());
    }
    contents
}

fn read_all_image_contents() -> Vec<ClipboardContent> {
    read_all_images()
        .into_iter()
        .map(ClipboardContent::Image)
        .collect()
}

fn read_content_for_hint<F, G>(
    hint: ClipboardContentHint,
    mut read_text: F,
    mut read_image: G,
) -> Option<ClipboardContent>
where
    F: FnMut() -> Option<ClipboardContent>,
    G: FnMut() -> Option<ClipboardContent>,
{
    match hint {
        ClipboardContentHint::Image => read_image(),
        ClipboardContentHint::Text => read_text(),
        ClipboardContentHint::Unknown => read_unknown_content(read_text, read_image),
    }
}

fn read_text_content() -> Option<ClipboardContent> {
    read_text()
        .ok()
        .filter(|text| !text.is_empty())
        .map(ClipboardContent::Text)
}

fn read_image_content() -> Option<ClipboardContent> {
    read_image().map(ClipboardContent::Image)
}

#[cfg(target_os = "windows")]
fn read_unknown_content<F, G>(read_text: F, mut read_image: G) -> Option<ClipboardContent>
where
    F: FnMut() -> Option<ClipboardContent>,
    G: FnMut() -> Option<ClipboardContent>,
{
    read_image().or_else(read_text)
}

#[cfg(not(target_os = "windows"))]
fn read_unknown_content<F, G>(mut read_text: F, read_image: G) -> Option<ClipboardContent>
where
    F: FnMut() -> Option<ClipboardContent>,
    G: FnMut() -> Option<ClipboardContent>,
{
    read_text().or_else(read_image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_format_image_hint_keeps_text_and_image() {
        let contents = read_contents_for_hint(
            ClipboardContentHint::Image,
            || Some(ClipboardContent::Text("image description".into())),
            || {
                vec![ClipboardContent::Image(ClipboardImage {
                    width: 1,
                    height: 1,
                    rgba_base64: "AAAAAA==".into(),
                })]
            },
        );

        assert_eq!(contents.len(), 2, "rich clipboard text must not be dropped");
        assert!(
            matches!(&contents[0], ClipboardContent::Text(text) if text == "image description")
        );
        assert!(matches!(&contents[1], ClipboardContent::Image(_)));
    }

    #[test]
    fn windows_dib_pixels_are_bgra_and_bottom_up() {
        // Top then bottom pixels in source RGBA order.
        let rgba = [10, 20, 30, 40, 50, 60, 70, 80];
        let bgra = rgba_to_bgra_bottom_up(&rgba, 1, 2).expect("convert pixels");

        assert_eq!(
            bgra,
            [70, 60, 50, 80, 30, 20, 10, 40],
            "DIB starts with the bottom pixel in BGRA, followed by the top pixel"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unknown_clipboard_prefers_text_before_image() {
        let content = read_content_for_hint(
            ClipboardContentHint::Unknown,
            || Some(ClipboardContent::Text("中文测试 abc 123".into())),
            || {
                Some(ClipboardContent::Image(ClipboardImage {
                    width: 1,
                    height: 1,
                    rgba_base64: "AAAAAA==".into(),
                }))
            },
        );

        match content {
            Some(ClipboardContent::Text(text)) => assert_eq!(text, "中文测试 abc 123"),
            _ => panic!("expected text to win when the platform cannot identify clipboard format"),
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn unknown_clipboard_keeps_windows_image_first_fallback() {
        let content = read_content_for_hint(
            ClipboardContentHint::Unknown,
            || Some(ClipboardContent::Text("中文测试 abc 123".into())),
            || {
                Some(ClipboardContent::Image(ClipboardImage {
                    width: 1,
                    height: 1,
                    rgba_base64: "AAAAAA==".into(),
                }))
            },
        );

        match content {
            Some(ClipboardContent::Image(image)) => assert_eq!(image.width, 1),
            _ => panic!("expected Windows fallback to keep image priority"),
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn utf8_command_sets_locale_for_clipboard_tools() {
        let command = utf8_command("pbpaste");
        let envs: std::collections::HashMap<_, _> = command
            .get_envs()
            .filter_map(|(key, value)| Some((key.to_str()?, value?.to_str()?)))
            .collect();

        assert_eq!(envs.get("LANG"), Some(&"en_US.UTF-8"));
        assert_eq!(envs.get("LC_CTYPE"), Some(&"en_US.UTF-8"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a real X11 display or xvfb-run"]
    fn linux_clipboard_text_survives_the_writer_call() {
        let expected = format!("mykvm-linux-clipboard-{}", std::process::id());
        write_system_text(&expected).expect("write Linux clipboard text");

        // Use a separate client to prove MyKVM kept ownership alive after the
        // write helper returned; reading through the same object is too weak.
        let mut observer = arboard::Clipboard::new().expect("open observer clipboard");
        let actual = observer.get_text().expect("read text from separate client");
        assert_eq!(actual, expected);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wayland_detection_is_explicit() {
        use std::ffi::OsStr;

        assert!(linux_wayland_session(Some(OsStr::new("wayland-0")), None));
        assert!(linux_wayland_session(None, Some("Wayland")));
        assert!(!linux_wayland_session(None, Some("x11")));
        assert!(!linux_wayland_session(None, None));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_rich_clipboard_fallback_is_diagnostic_and_prefers_image() {
        let contents = vec![
            ClipboardContent::Text("image description".into()),
            ClipboardContent::Image(ClipboardImage {
                width: 1,
                height: 1,
                rgba_base64: "AQID/w==".into(),
            }),
        ];

        let (selected, diagnostic) = linux_multi_format_fallback(&contents);
        assert!(matches!(selected, Some(ClipboardContent::Image(_))));
        assert_eq!(diagnostic, Some(LINUX_RICH_CLIPBOARD_DEGRADATION));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a real X11 display or xvfb-run"]
    fn linux_xfixes_token_changes_once_per_copy_and_skips_unchanged_reads() {
        use std::time::{Duration, Instant};

        fn wait_for_change(previous: u64) -> u64 {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Some(current) = change_token() {
                    if current != previous {
                        return current;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("XFixes clipboard token did not change after a copy");
        }

        let initial = change_token().expect("XFixes clipboard change token");
        let mut writer = arboard::Clipboard::new().expect("open X11 clipboard writer");
        writer
            .set_text("mykvm-token-first")
            .expect("make first X11 copy");
        let first = wait_for_change(initial);

        // A stable token is the contract used by the sync loop to avoid
        // re-reading and base64-encoding an unchanged screenshot every 50 ms.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(change_token(), Some(first));

        // arboard reuses one selection-owner window. XFixes must still report
        // its second copy; merely polling GetSelectionOwner would miss this.
        writer
            .set_text("mykvm-token-second")
            .expect("make second X11 copy from the same owner");
        let second = wait_for_change(first);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(change_token(), Some(second));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a real X11 display or xvfb-run"]
    fn linux_rich_clipboard_fallback_preserves_native_image() {
        let contents = vec![
            ClipboardContent::Text("image description".into()),
            ClipboardContent::Image(ClipboardImage {
                width: 1,
                height: 1,
                rgba_base64: "AQID/w==".into(),
            }),
        ];
        write_contents(&contents).expect("write degraded Linux rich clipboard");

        let mut observer = arboard::Clipboard::new().expect("open observer clipboard");
        let image = observer.get_image().expect("read native image/png target");
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.bytes.as_ref(), &[1, 2, 3, 255]);
        assert!(
            observer.get_text().is_err(),
            "the unsupported text loss must match the documented diagnostic fallback"
        );
    }
}

fn read_image() -> Option<ClipboardImage> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    #[cfg(target_os = "linux")]
    let arboard_image = with_linux_clipboard(|clipboard| clipboard.get_image())
        .ok()
        .and_then(|image| {
            if image.width == 0
                || image.height == 0
                || image.bytes.is_empty()
                || image.bytes.len() > CLIPBOARD_MAX_IMAGE_BYTES
            {
                return None;
            }
            Some(ClipboardImage {
                width: image.width as u32,
                height: image.height as u32,
                rgba_base64: BASE64.encode(image.bytes.as_ref()),
            })
        });

    #[cfg(not(target_os = "linux"))]
    let arboard_image = with_clipboard_appkit(|| {
        arboard::Clipboard::new().ok().and_then(|mut clipboard| {
            let image = clipboard.get_image().ok()?;
            if image.width == 0 || image.height == 0 || image.bytes.is_empty() {
                return None;
            }
            if image.bytes.len() > CLIPBOARD_MAX_IMAGE_BYTES {
                return None;
            }

            Some(ClipboardImage {
                width: image.width as u32,
                height: image.height as u32,
                rgba_base64: BASE64.encode(image.bytes.as_ref()),
            })
        })
    });

    arboard_image.or_else(|| {
        #[cfg(target_os = "windows")]
        {
            read_windows_dib_image()
        }

        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    })
}

/// Reads all image representations on the clipboard.
/// ponytail: single image via arboard for now; macOS NSPasteboard multi-item
/// read (真正多图) is the next increment.
fn read_all_images() -> Vec<ClipboardImage> {
    read_image().into_iter().collect()
}

fn write_image(image: &ClipboardImage) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let bytes = BASE64
        .decode(image.rgba_base64.as_bytes())
        .map_err(|error| format!("failed to decode clipboard image: {error}"))?;
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0 || height == 0 || bytes.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("clipboard image has invalid dimensions".into());
    }

    #[cfg(target_os = "linux")]
    return with_linux_clipboard(|clipboard| {
        clipboard.set_image(arboard::ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Owned(bytes),
        })
    });

    #[cfg(not(target_os = "linux"))]
    with_clipboard_appkit(|| {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|error| format!("failed to open clipboard: {error}"))?;
        clipboard
            .set_image(arboard::ImageData {
                width,
                height,
                bytes: std::borrow::Cow::Owned(bytes),
            })
            .map_err(|error| format!("failed to write clipboard image: {error}"))
    })
}

#[cfg(target_os = "windows")]
fn content_hint() -> ClipboardContentHint {
    use windows_sys::Win32::System::DataExchange::{
        IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };
    use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_UNICODETEXT};

    let png_format = unsafe { RegisterClipboardFormatW(crate::wide_null("PNG").as_ptr()) };
    let image_formats = [png_format, u32::from(CF_DIBV5), u32::from(CF_DIB)];
    if image_formats
        .iter()
        .any(|format| *format != 0 && unsafe { IsClipboardFormatAvailable(*format) } != 0)
    {
        return ClipboardContentHint::Image;
    }
    if unsafe { IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT)) } != 0 {
        ClipboardContentHint::Text
    } else {
        ClipboardContentHint::Unknown
    }
}

#[cfg(not(target_os = "windows"))]
fn content_hint() -> ClipboardContentHint {
    ClipboardContentHint::Unknown
}

#[cfg(target_os = "windows")]
fn read_windows_dib_image() -> Option<ClipboardImage> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5};

    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        return None;
    }
    let _guard = ClipboardGuard;

    for format in [u32::from(CF_DIBV5), u32::from(CF_DIB)] {
        let handle = unsafe { GetClipboardData(format) };
        if handle.is_null() {
            continue;
        }
        let len = unsafe { GlobalSize(handle) };
        if len == 0 || len > CLIPBOARD_MAX_IMAGE_BYTES.saturating_add(256) {
            continue;
        }
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            continue;
        }
        let data = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
        let decoded = decode_windows_dib_image(data);
        unsafe {
            let _ = GlobalUnlock(handle);
        }
        if decoded.is_some() {
            return decoded;
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn decode_windows_dib_image(data: &[u8]) -> Option<ClipboardImage> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use image::{codecs::bmp::BmpDecoder, DynamicImage, ImageDecoder};

    let decoder = BmpDecoder::new_without_file_header(std::io::Cursor::new(data)).ok()?;
    let (width, height) = decoder.dimensions();
    let rgba = DynamicImage::from_decoder(decoder).ok()?.into_rgba8();
    let bytes = rgba.into_raw();
    if width == 0 || height == 0 || bytes.is_empty() || bytes.len() > CLIPBOARD_MAX_IMAGE_BYTES {
        return None;
    }

    Some(ClipboardImage {
        width,
        height,
        rgba_base64: BASE64.encode(bytes),
    })
}

#[cfg(target_os = "windows")]
fn read_system_text() -> Result<String, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("failed to open clipboard: {error}"))?;
    clipboard
        .get_text()
        .map_err(|error| format!("failed to read clipboard text: {error}"))
}

#[cfg(target_os = "macos")]
fn read_system_text() -> Result<String, String> {
    let output = utf8_command("pbpaste")
        .output()
        .map_err(|error| format!("failed to read clipboard: {error}"))?;

    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("clipboard text is not valid UTF-8: {error}"))
    } else {
        Err(format!(
            "clipboard command exited with status {}",
            output.status
        ))
    }
}

#[cfg(target_os = "linux")]
fn read_system_text() -> Result<String, String> {
    with_linux_clipboard(|clipboard| clipboard.get_text())
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn read_system_text() -> Result<String, String> {
    Err("clipboard text is not implemented on this platform".into())
}

#[cfg(target_os = "windows")]
fn write_system_text(text: &str) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("failed to open clipboard: {error}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|error| format!("failed to write clipboard text: {error}"))
}

#[cfg(target_os = "macos")]
fn write_system_text(text: &str) -> Result<(), String> {
    use std::{io::Write, process::Stdio};

    let mut child = utf8_command("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to write clipboard: {error}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| format!("failed to send clipboard text: {error}"))?;
    }

    let status = child
        .wait()
        .map_err(|error| format!("failed to finish clipboard write: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("clipboard command exited with status {status}"))
    }
}

#[cfg(target_os = "linux")]
fn write_system_text(text: &str) -> Result<(), String> {
    with_linux_clipboard(|clipboard| clipboard.set_text(text.to_string()))
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn write_system_text(_text: &str) -> Result<(), String> {
    Err("clipboard text is not implemented on this platform".into())
}

#[cfg(not(target_os = "windows"))]
fn utf8_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command
        .env("LANG", "en_US.UTF-8")
        .env("LC_CTYPE", "en_US.UTF-8");
    command
}
