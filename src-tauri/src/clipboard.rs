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
    Rich(crate::rich_clipboard::RichText),
}

pub(crate) enum ClipboardSnapshot {
    Content(ClipboardContent),
    Files(Vec<String>),
}

impl ClipboardSnapshot {
    pub(crate) fn signature(&self) -> String {
        match self {
            Self::Content(content) => content.signature(),
            Self::Files(paths) => files_signature(paths),
        }
    }

    pub(crate) fn send_key(&self, version: Option<u64>) -> String {
        let signature = self.signature();
        match self {
            // Copying the same folder again must resend changed children,
            // without walking or hashing that folder on every clipboard poll.
            Self::Files(_) => format!("{signature}:copy:{version:?}"),
            Self::Content(_) => signature,
        }
    }
}

pub(crate) fn files_signature(paths: &[String]) -> String {
    // Encode boundaries so ["ab", "c"] and ["a", "bc"] are distinct.
    let encoded = serde_json::to_vec(paths).unwrap_or_default();
    format!("files:{:016x}", clipboard_signature_hash(&encoded))
}

pub(crate) fn read_snapshot() -> Option<ClipboardSnapshot> {
    let paths = crate::rich_clipboard::read_files();
    if !paths.is_empty() {
        Some(ClipboardSnapshot::Files(paths))
    } else {
        read_content().map(ClipboardSnapshot::Content)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardContentHint {
    Image,
    Text,
    Unknown,
}

fn clipboard_signature_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

impl ClipboardContent {
    pub(crate) fn is_oversized(&self) -> bool {
        match self {
            ClipboardContent::Rich(rich) => rich.text.len() > CLIPBOARD_MAX_TEXT_BYTES
                || rich.html.as_ref().is_some_and(|s| s.len() > crate::rich_clipboard::MAX_FORMAT_BYTES)
                || rich.rtf.as_ref().is_some_and(|s| s.len() > crate::rich_clipboard::MAX_FORMAT_BYTES),
            ClipboardContent::Text(text) => text.len() > CLIPBOARD_MAX_TEXT_BYTES,
            ClipboardContent::Image(image) => {
                // base64 inflates ~4/3; compare against the decoded RGBA budget.
                let padding = image
                    .rgba_base64
                    .bytes()
                    .rev()
                    .take(2)
                    .take_while(|byte| *byte == b'=')
                    .count();
                (image.rgba_base64.len() / 4 * 3).saturating_sub(padding)
                    > CLIPBOARD_MAX_IMAGE_BYTES
            }
        }
    }

    /// A stable fingerprint used to detect "did the clipboard change" and to
    /// suppress echoing content we just received from a peer.
    pub(crate) fn signature(&self) -> String {
        match self {
            ClipboardContent::Rich(rich) => format!("rich:{:x}:{:x}:{:x}", clipboard_signature_hash(rich.text.as_bytes()),
                clipboard_signature_hash(rich.html.as_deref().unwrap_or("").as_bytes()),
                clipboard_signature_hash(rich.rtf.as_deref().unwrap_or(&[]))),
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

pub(crate) fn read_text() -> Result<String, String> {
    read_system_text()
}

pub(crate) fn write_text(text: &str) -> Result<(), String> {
    write_system_text(text)
}

pub(crate) fn write_content(content: &ClipboardContent) -> Result<(), String> {
    // arboard's macOS image write is not wrapped in an autorelease pool. Synced
    // clipboards are written on long-lived QUIC worker threads, which have no
    // pool, so each received image's NSImage/TIFF temporaries (the whole image)
    // were never freed: a few dozen screenshots reached ~2 GB (discussion #32).
    #[cfg(target_os = "macos")]
    let _pool = crate::input::macos_appkit::autorelease_pool();
    match content {
        ClipboardContent::Rich(rich) => crate::rich_clipboard::write(rich),
        ClipboardContent::Text(text) => write_text(text),
        ClipboardContent::Image(image) => write_image(image),
    }
}

/// Reads whatever is currently on the clipboard. The shared policy lives here:
/// when the platform can identify a current image format, wait for an image
/// read instead of falling back to stale text from a previous clipboard format.
pub(crate) fn read_content() -> Option<ClipboardContent> {
    // Same reason as `write_content`; arboard pools its image read but not
    // `Clipboard::new()`, and this runs on the pool-less clipboard thread.
    #[cfg(target_os = "macos")]
    let _pool = crate::input::macos_appkit::autorelease_pool();
    if !crate::rich_clipboard::read_files().is_empty() { return None; }
    if let Some(rich) = crate::rich_clipboard::read() { return Some(ClipboardContent::Rich(rich)); }
    read_content_for_hint(content_hint(), read_text_content, read_image_content)
}

pub(crate) fn change_count() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        crate::input::macos_appkit::clipboard_change_count()
    }
    #[cfg(target_os = "windows")]
    {
        let sequence =
            unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() };
        (sequence != 0).then_some(u64::from(sequence))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
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
    fn file_clipboard_recopies_resend_without_changing_the_echo_signature() {
        let files = ClipboardSnapshot::Files(vec!["/folder".into(), "/中文.txt".into()]);
        assert_ne!(files.send_key(Some(1)), files.send_key(Some(2)));
        assert_eq!(files.signature(), files_signature(&["/folder".into(), "/中文.txt".into()]));
        assert_ne!(files_signature(&["ab".into(), "c".into()]), files_signature(&["a".into(), "bc".into()]));
        let text = ClipboardSnapshot::Content(ClipboardContent::Text("same text".into()));
        assert_eq!(text.send_key(Some(1)), text.send_key(Some(2)));
    }

    #[test]
    fn image_budget_accounts_for_base64_padding() {
        let encoded_len = CLIPBOARD_MAX_IMAGE_BYTES.div_ceil(3) * 4;
        let mut image = ClipboardImage {
            width: 8192,
            height: 1024,
            rgba_base64: "A".repeat(encoded_len - 1) + "=",
        };
        assert!(!ClipboardContent::Image(image.clone()).is_oversized());
        image.rgba_base64.replace_range(encoded_len - 1.., "A");
        assert!(ClipboardContent::Image(image).is_oversized());
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
}

fn read_image() -> Option<ClipboardImage> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let arboard_image = arboard::Clipboard::new().ok().and_then(|mut clipboard| {
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

fn write_image(image: &ClipboardImage) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let bytes = BASE64
        .decode(image.rgba_base64.as_bytes())
        .map_err(|error| format!("failed to decode clipboard image: {error}"))?;
    let width = image.width as usize;
    let height = image.height as usize;
    if width == 0
        || height == 0
        || bytes.len() > CLIPBOARD_MAX_IMAGE_BYTES
        || bytes.len() != width.saturating_mul(height).saturating_mul(4)
    {
        return Err("clipboard image has invalid dimensions".into());
    }

    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("failed to open clipboard: {error}"))?;
    clipboard
        .set_image(arboard::ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Owned(bytes),
        })
        .map_err(|error| format!("failed to write clipboard image: {error}"))
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
