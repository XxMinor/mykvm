use super::{RichText, MAX_FORMAT_BYTES};
use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

struct Owner(windows_sys::Win32::Foundation::HWND);
impl Drop for Owner {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
        }
    }
}
fn owner() -> Result<Owner, String> {
    let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
    let window = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            windows_sys::Win32::UI::WindowsAndMessaging::HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if window.is_null() {
        Err("无法建立剪贴板窗口。".into())
    } else {
        Ok(Owner(window))
    }
}
fn open_owned(owner: &Owner) -> Result<Open, String> {
    if unsafe { OpenClipboard(owner.0) } == 0 {
        Err("剪贴板被其它应用占用。".into())
    } else {
        Ok(Open)
    }
}

struct Open;
impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}
fn open() -> Result<Open, String> {
    if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        Err("剪贴板被其它应用占用。".into())
    } else {
        Ok(Open)
    }
}
fn format(name: &str) -> u32 {
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe { RegisterClipboardFormatW(name.as_ptr()) }
}

unsafe fn bytes(format: u32) -> Option<Vec<u8>> {
    let handle = GetClipboardData(format);
    if handle.is_null() {
        return None;
    }
    let size = GlobalSize(handle);
    if size == 0 || size > MAX_FORMAT_BYTES {
        return None;
    }
    let pointer = GlobalLock(handle);
    if pointer.is_null() {
        return None;
    }
    let mut data = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
    GlobalUnlock(handle);
    while data.last() == Some(&0) {
        data.pop();
    }
    Some(data)
}

unsafe fn put(format: u32, bytes: &[u8]) -> Result<(), String> {
    let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len() + 1);
    if handle.is_null() {
        return Err("剪贴板内存分配失败。".into());
    }
    let pointer = GlobalLock(handle);
    if pointer.is_null() {
        GlobalFree(handle);
        return Err("剪贴板内存锁定失败。".into());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
    *pointer.cast::<u8>().add(bytes.len()) = 0;
    GlobalUnlock(handle);
    if SetClipboardData(format, handle).is_null() {
        GlobalFree(handle);
        return Err("剪贴板写入失败。".into());
    }
    Ok(())
}

pub(super) fn read() -> Option<RichText> {
    let (html, rtf) = {
        let _open = open().ok()?;
        unsafe {
            (
                bytes(format("HTML Format"))
                    .and_then(|b| String::from_utf8(b).ok())
                    .and_then(|s| super::html_fragment(&s)),
                bytes(format("Rich Text Format")),
            )
        }
    };
    if html.is_none() && rtf.is_none() {
        return None;
    }
    Some(RichText {
        text: arboard::Clipboard::new()
            .ok()?
            .get_text()
            .unwrap_or_default(),
        html,
        rtf,
    })
}

pub(super) fn write_rtf(bytes: &[u8]) -> Result<(), String> {
    let _open = open()?;
    unsafe { put(format("Rich Text Format"), bytes) }
}

pub(super) fn write(rich: &RichText) -> Result<(), String> {
    let owner = owner()?;
    let _open = open_owned(&owner)?;
    let plain: Vec<u8> = rich
        .text
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    unsafe {
        if EmptyClipboard() == 0 {
            return Err("无法清空剪贴板。".into());
        }
        put(13, &plain)?;
        if let Some(html) = &rich.html {
            put(format("HTML Format"), super::windows_html(html).as_bytes())?;
        }
        if let Some(rtf) = &rich.rtf {
            put(format("Rich Text Format"), rtf)?;
        }
    }
    Ok(())
}

pub(super) fn read_files() -> Vec<String> {
    let Ok(_open) = open() else {
        return Vec::new();
    };
    unsafe {
        let drop = GetClipboardData(15);
        if drop.is_null() {
            return Vec::new();
        }
        let count =
            windows_sys::Win32::UI::Shell::DragQueryFileW(drop, u32::MAX, std::ptr::null_mut(), 0)
                .min(65);
        (0..count)
            .filter_map(|index| {
                let len = windows_sys::Win32::UI::Shell::DragQueryFileW(
                    drop,
                    index,
                    std::ptr::null_mut(),
                    0,
                );
                let mut name = vec![0_u16; len as usize + 1];
                if windows_sys::Win32::UI::Shell::DragQueryFileW(
                    drop,
                    index,
                    name.as_mut_ptr(),
                    name.len() as u32,
                ) == 0
                {
                    return None;
                }
                Some(String::from_utf16_lossy(&name[..len as usize]))
            })
            .collect()
    }
}

pub(super) fn write_files(paths: &[String]) -> Result<(), String> {
    let mut data = vec![0_u8; 20];
    data[0..4].copy_from_slice(&20_u32.to_le_bytes());
    data[16..20].copy_from_slice(&1_u32.to_le_bytes());
    for path in paths {
        for ch in path.encode_utf16().chain(Some(0)) {
            data.extend_from_slice(&ch.to_le_bytes());
        }
    }
    data.extend_from_slice(&0_u16.to_le_bytes());
    let owner = owner()?;
    let _open = open_owned(&owner)?;
    unsafe {
        if EmptyClipboard() == 0 {
            return Err("无法清空剪贴板。".into());
        }
        put(15, &data)
    }
}
