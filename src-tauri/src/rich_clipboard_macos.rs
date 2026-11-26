use super::{RichText, MAX_FORMAT_BYTES};
use std::ffi::{c_char, c_void, CStr, CString};

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}

unsafe fn sel(name: &'static [u8]) -> *mut c_void {
    sel_registerName(name.as_ptr().cast())
}
unsafe fn obj(receiver: *mut c_void, name: &'static [u8]) -> *mut c_void {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, sel(name))
}
unsafe fn one(receiver: *mut c_void, name: &'static [u8], value: *mut c_void) -> *mut c_void {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, sel(name), value)
}
unsafe fn ns(text: &str) -> *mut c_void {
    let Ok(text) = CString::new(text) else {
        return std::ptr::null_mut();
    };
    one(
        objc_getClass(b"NSString\0".as_ptr().cast()),
        b"stringWithUTF8String:\0",
        text.as_ptr().cast_mut().cast(),
    )
}
unsafe fn text(value: *mut c_void) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let pointer = obj(value, b"UTF8String\0").cast::<c_char>();
    if pointer.is_null() {
        None
    } else {
        Some(CStr::from_ptr(pointer).to_string_lossy().into_owned())
    }
}
unsafe fn number(receiver: *mut c_void, name: &'static [u8]) -> usize {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> usize =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, sel(name))
}
unsafe fn pasteboard() -> *mut c_void {
    obj(
        objc_getClass(b"NSPasteboard\0".as_ptr().cast()),
        b"generalPasteboard\0",
    )
}

pub(super) fn read() -> Option<RichText> {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    unsafe {
        let board = pasteboard();
        let html = text(one(board, b"stringForType:\0", ns("public.html")));
        let data = one(board, b"dataForType:\0", ns("public.rtf"));
        let len = number(data, b"length\0");
        let pointer = obj(data, b"bytes\0").cast::<u8>();
        let rtf = (!pointer.is_null() && len > 0 && len <= MAX_FORMAT_BYTES)
            .then(|| std::slice::from_raw_parts(pointer, len).to_vec());
        if html.is_none() && rtf.is_none() {
            return None;
        }
        Some(RichText {
            text: text(one(
                board,
                b"stringForType:\0",
                ns("public.utf8-plain-text"),
            ))
            .unwrap_or_default(),
            html,
            rtf,
        })
    }
}

pub(super) fn write_rtf(bytes: &[u8]) -> Result<(), String> {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    unsafe {
        let make: unsafe extern "C" fn(*mut c_void, *mut c_void, *const u8, usize) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let data = make(
            objc_getClass(b"NSData\0".as_ptr().cast()),
            sel(b"dataWithBytes:length:\0"),
            bytes.as_ptr(),
            bytes.len(),
        );
        let set: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        if set(
            pasteboard(),
            sel(b"setData:forType:\0"),
            data,
            ns("public.rtf"),
        ) {
            Ok(())
        } else {
            Err("富文本剪贴板写入失败。".into())
        }
    }
}

// nspasteboard.org: the types clipboard managers agree to keep out of history.
const CONCEALED_TYPE: &str = "org.nspasteboard.ConcealedType";
const TRANSIENT_TYPE: &str = "org.nspasteboard.TransientType";

pub(super) fn is_concealed() -> bool {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    unsafe {
        let types = obj(pasteboard(), b"types\0");
        let contains: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        !types.is_null()
            && [CONCEALED_TYPE, TRANSIENT_TYPE]
                .into_iter()
                .any(|name| contains(types, sel(b"containsObject:\0"), ns(name)))
    }
}

pub(super) fn write_concealed_text(text: &str) -> Result<(), String> {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    unsafe {
        let board = pasteboard();
        let types = obj(
            objc_getClass(b"NSMutableArray\0".as_ptr().cast()),
            b"array\0",
        );
        one(types, b"addObject:\0", ns("public.utf8-plain-text"));
        one(types, b"addObject:\0", ns(CONCEALED_TYPE));
        // One declaration, so a clipboard manager never sees the text unmarked.
        let declare: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> isize =
            std::mem::transmute(objc_msgSend as *const ());
        declare(board, sel(b"declareTypes:owner:\0"), types, std::ptr::null_mut());
        let set: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        let string = ns(text);
        if string.is_null()
            || !set(board, sel(b"setString:forType:\0"), string, ns("public.utf8-plain-text"))
            || !set(board, sel(b"setString:forType:\0"), ns(""), ns(CONCEALED_TYPE))
        {
            return Err("剪贴板写入失败。".into());
        }
        Ok(())
    }
}

pub(super) fn read_files() -> Vec<String> {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    unsafe {
        let items = obj(pasteboard(), b"pasteboardItems\0");
        let get: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        (0..number(items, b"count\0").min(65))
            .filter_map(|index| {
                let item = get(items, sel(b"objectAtIndex:\0"), index);
                let value = one(item, b"stringForType:\0", ns("public.file-url"));
                if value.is_null() {
                    return None;
                }
                let url = one(
                    objc_getClass(b"NSURL\0".as_ptr().cast()),
                    b"URLWithString:\0",
                    value,
                );
                text(obj(url, b"path\0"))
            })
            .collect()
    }
}

pub(super) fn write_files(paths: &[String]) -> Result<(), String> {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    unsafe {
        let array = obj(
            objc_getClass(b"NSMutableArray\0".as_ptr().cast()),
            b"array\0",
        );
        for path in paths {
            let url = one(
                objc_getClass(b"NSURL\0".as_ptr().cast()),
                b"fileURLWithPath:\0",
                ns(path),
            );
            one(array, b"addObject:\0", url);
        }
        obj(pasteboard(), b"clearContents\0");
        let write: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        if write(pasteboard(), sel(b"writeObjects:\0"), array) {
            Ok(())
        } else {
            Err("文件剪贴板写入失败。".into())
        }
    }
}
