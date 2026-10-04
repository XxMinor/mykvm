#[cfg(target_os = "windows")]
#[path = "rich_clipboard_windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "rich_clipboard_macos.rs"]
mod platform;

#[derive(Clone, Debug)]
pub(crate) struct RichText {
    pub text: String,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
}
pub(crate) const MAX_FORMAT_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn write(rich: &RichText) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        platform::write(rich)
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(html) = &rich.html {
            arboard::Clipboard::new()
                .map_err(|e| e.to_string())?
                .set_html(html, Some(&rich.text))
                .map_err(|e| e.to_string())?;
        } else {
            crate::clipboard::write_text(&rich.text)?;
        }
        if let Some(rtf) = &rich.rtf {
            write_rtf(rtf)?;
        }
        Ok(())
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn windows_html(fragment: &str) -> String {
    let header = |start, end, fragment_start, fragment_end| {
        format!("Version:0.9\r\nStartHTML:{start:010}\r\nEndHTML:{end:010}\r\nStartFragment:{fragment_start:010}\r\nEndFragment:{fragment_end:010}\r\n")
    };
    let prefix = "<html><body><!--StartFragment-->";
    let suffix = "<!--EndFragment--></body></html>";
    let start = header(0, 0, 0, 0).len();
    let fragment_start = start + prefix.len();
    let fragment_end = fragment_start + fragment.len();
    format!(
        "{}{prefix}{fragment}{suffix}",
        header(
            start,
            fragment_end + suffix.len(),
            fragment_start,
            fragment_end
        )
    )
}

pub(crate) fn read() -> Option<RichText> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        platform::read()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

pub(crate) fn write_rtf(bytes: &[u8]) -> Result<(), String> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        platform::write_rtf(bytes)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = bytes;
        Ok(())
    }
}

pub(crate) fn read_files() -> Vec<String> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        platform::read_files()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Vec::new()
    }
}

pub(crate) fn write_files(paths: &[String]) -> Result<(), String> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        platform::write_files(paths)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = paths;
        Err("此平台尚未支持文件剪贴板。".into())
    }
}

pub(crate) fn html_fragment(html: &str) -> Option<String> {
    let offset = |name: &str| {
        html.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|s| s.trim().parse::<usize>().ok())
    };
    if let (Some(start), Some(end)) = (offset("StartFragment:"), offset("EndFragment:")) {
        if start <= end {
            return html.get(start..end).map(str::to_string);
        }
    }
    if let Some((_, rest)) = html.split_once("<!--StartFragment-->") {
        return rest
            .split_once("<!--EndFragment-->")
            .map(|(fragment, _)| fragment.to_string());
    }
    (!html.starts_with("Version:")).then(|| html.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_fragment_offsets_are_bytes_and_cannot_split_unicode_or_overrun() {
        let fragment = "<b>中文</b>";
        let dummy = format!(
            "Version:1.0\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
            0, 0
        );
        let header = format!(
            "Version:1.0\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
            dummy.len(),
            dummy.len() + fragment.len()
        );
        assert_eq!(
            html_fragment(&format!("{header}{fragment}")),
            Some(fragment.into())
        );
        assert_eq!(
            html_fragment("<!--StartFragment--><b>中文</b><!--EndFragment-->"),
            Some("<b>中文</b>".into())
        );
        assert!(html_fragment("Version:1.0\nStartFragment:999\nEndFragment:1000").is_none());
    }
}
