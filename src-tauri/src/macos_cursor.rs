use core_graphics::geometry::CGPoint;
use std::{
    cell::{Cell, OnceCell},
    ffi::{c_char, c_void},
};

// NSImage initWithData: accepts an encoded image, not a raw RGBA pixel buffer.
const TRANSPARENT_PNG: &[u8] = include_bytes!("../icons/transparent-cursor.png");

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}

unsafe fn selector(name: &'static [u8]) -> *mut c_void {
    sel_registerName(name.as_ptr().cast())
}

unsafe fn object(receiver: *mut c_void, name: &'static [u8]) -> *mut c_void {
    let send: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, selector(name))
}

unsafe fn action(receiver: *mut c_void, name: &'static [u8]) {
    let send: extern "C" fn(*mut c_void, *mut c_void) =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, selector(name));
}

struct TransparentCursor {
    raw: *mut c_void,
    pushed: Cell<bool>,
}

impl Drop for TransparentCursor {
    fn drop(&mut self) {
        let _pool = crate::input::macos_appkit::autorelease_pool();
        unsafe {
            if self.pushed.replace(false) {
                action(objc_getClass(b"NSCursor\0".as_ptr().cast()), b"pop\0");
            }
            action(self.raw, b"release\0");
        }
    }
}

thread_local! {
    // Keep AppKit objects on their owning capture thread. Reasserting the
    // cursor must neither decode an image nor allocate a new retained cursor.
    static CURSOR: OnceCell<Option<TransparentCursor>> = const { OnceCell::new() };
}

fn create() -> Option<TransparentCursor> {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    unsafe {
        let data_class = objc_getClass(b"NSData\0".as_ptr().cast());
        let image_class = objc_getClass(b"NSImage\0".as_ptr().cast());
        let cursor_class = objc_getClass(b"NSCursor\0".as_ptr().cast());
        if data_class.is_null() || image_class.is_null() || cursor_class.is_null() {
            return None;
        }
        let data_with_bytes: extern "C" fn(
            *mut c_void,
            *mut c_void,
            *const u8,
            usize,
        ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
        let data = data_with_bytes(
            data_class,
            selector(b"dataWithBytes:length:\0"),
            TRANSPARENT_PNG.as_ptr(),
            TRANSPARENT_PNG.len(),
        );
        if data.is_null() {
            return None;
        }
        let init_image: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let image = init_image(
            object(image_class, b"alloc\0"),
            selector(b"initWithData:\0"),
            data,
        );
        if image.is_null() {
            return None;
        }
        let init_cursor: extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            CGPoint,
        ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
        let raw = init_cursor(
            object(cursor_class, b"alloc\0"),
            selector(b"initWithImage:hotSpot:\0"),
            image,
            CGPoint::new(0.0, 0.0),
        );
        // NSCursor.image retains the image; the temporary creator owns one ref.
        action(image, b"release\0");
        (!raw.is_null()).then(|| TransparentCursor {
            raw,
            pushed: Cell::new(false),
        })
    }
}

pub(crate) fn prepare() -> bool {
    CURSOR.with(|cursor| cursor.get_or_init(create).is_some())
}

pub(crate) fn push() {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    CURSOR.with(|cursor| {
        if let Some(cursor) = cursor.get_or_init(create) {
            if !cursor.pushed.replace(true) {
                unsafe {
                    action(cursor.raw, b"push\0");
                }
            }
        }
    });
}

pub(crate) fn set_current() {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    CURSOR.with(|cursor| {
        if let Some(cursor) = cursor.get_or_init(create) {
            unsafe {
                action(cursor.raw, b"set\0");
            }
        }
    });
}

pub(crate) fn pop() {
    let _pool = crate::input::macos_appkit::autorelease_pool();
    CURSOR.with(|cursor| {
        if let Some(Some(cursor)) = cursor.get() {
            if cursor.pushed.replace(false) {
                unsafe {
                    action(objc_getClass(b"NSCursor\0".as_ptr().cast()), b"pop\0");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_cursor_has_a_decodable_clear_image_and_is_cached() {
        // Construct and inspect only: never push/set a cursor or post input.
        let _pool = crate::input::macos_appkit::autorelease_pool();
        assert!(prepare());
        CURSOR.with(|cache| unsafe {
            let cursor = cache.get().unwrap().as_ref().unwrap();
            let first = cursor.raw;
            for _ in 0..1_000 {
                assert!(prepare());
                assert_eq!(cache.get().unwrap().as_ref().unwrap().raw, first);
            }
            let image = object(cursor.raw, b"image\0");
            let size: extern "C" fn(*mut c_void, *mut c_void) -> core_graphics::geometry::CGSize =
                std::mem::transmute(objc_msgSend as *const ());
            let dimensions = size(image, selector(b"size\0"));
            assert_eq!((dimensions.width, dimensions.height), (16.0, 16.0));
            let tiff = object(image, b"TIFFRepresentation\0");
            assert!(!tiff.is_null());
            let rep: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let bitmap = rep(
                objc_getClass(b"NSBitmapImageRep\0".as_ptr().cast()),
                selector(b"imageRepWithData:\0"),
                tiff,
            );
            assert!(!bitmap.is_null());
            let color_at: extern "C" fn(*mut c_void, *mut c_void, usize, usize) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let alpha: extern "C" fn(*mut c_void, *mut c_void) -> f64 =
                std::mem::transmute(objc_msgSend as *const ());
            for x in 0..16 {
                for y in 0..16 {
                    let color = color_at(bitmap, selector(b"colorAtX:y:\0"), x, y);
                    assert!(!color.is_null());
                    assert_eq!(alpha(color, selector(b"alphaComponent\0")), 0.0);
                }
            }
            assert!(!cursor.pushed.get());
        });
    }
}
