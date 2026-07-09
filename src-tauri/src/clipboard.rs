use serde::{Deserialize, Serialize};

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
/// platform has no cheap counter (Linux) and callers must fall back to reading.
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

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
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
/// A single item takes the plain (arboard) path; a mix of text and image must be
/// written natively in one shot, because arboard clears the clipboard on each
/// set and would leave only the last representation.
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

/// Non-macOS receivers keep single-representation behaviour for now (writing the
/// image if present, else the text). Native multi-format write on Windows is the
/// next increment.
#[cfg(not(target_os = "macos"))]
fn write_multiple_contents(contents: &[ClipboardContent]) -> Result<(), String> {
    if let Some(image) = contents.iter().find_map(|content| match content {
        ClipboardContent::Image(image) => Some(image),
        _ => None,
    }) {
        return write_image(image);
    }
    if let Some(text) = contents.iter().find_map(|content| match content {
        ClipboardContent::Text(text) => Some(text),
        _ => None,
    }) {
        return write_text(text);
    }
    Ok(())
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
    // A pure-image hint means there is no text representation to fetch; a
    // pure-text hint means no image. Unknown (macOS) may be either or both.
    if !matches!(hint, ClipboardContentHint::Image) {
        contents.extend(read_text());
    }
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
}

fn read_image() -> Option<ClipboardImage> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

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
    use windows_sys::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_DIBV5, CF_UNICODETEXT};

    let png_format = unsafe { RegisterClipboardFormatW(crate::wide_null("PNG").as_ptr()) };
    let image_formats = [
        png_format,
        u32::from(CF_DIBV5),
        u32::from(CF_DIB),
        u32::from(CF_BITMAP),
    ];
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

#[cfg(not(target_os = "windows"))]
fn read_system_text() -> Result<String, String> {
    use std::process::Command;

    let output = if cfg!(target_os = "macos") {
        utf8_command("pbpaste").output()
    } else {
        Command::new("sh")
            .args([
                "-c",
                "wl-paste -n 2>/dev/null || xclip -selection clipboard -out",
            ])
            .output()
    }
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

#[cfg(target_os = "windows")]
fn write_system_text(text: &str) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("failed to open clipboard: {error}"))?;
    clipboard
        .set_text(text.to_string())
        .map_err(|error| format!("failed to write clipboard text: {error}"))
}

#[cfg(not(target_os = "windows"))]
fn write_system_text(text: &str) -> Result<(), String> {
    use std::{io::Write, process::Command, process::Stdio};

    let mut child = if cfg!(target_os = "macos") {
        utf8_command("pbcopy").stdin(Stdio::piped()).spawn()
    } else {
        Command::new("sh")
            .args(["-c", "wl-copy 2>/dev/null || xclip -selection clipboard"])
            .stdin(Stdio::piped())
            .spawn()
    }
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

#[cfg(not(target_os = "windows"))]
fn utf8_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command
        .env("LANG", "en_US.UTF-8")
        .env("LC_CTYPE", "en_US.UTF-8");
    command
}
