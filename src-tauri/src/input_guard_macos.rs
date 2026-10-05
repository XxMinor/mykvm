use super::ForegroundApplication;
use core_foundation::base::CFRelease;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use std::ffi::{c_char, c_void, CStr};

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, timeout: f32) -> i32;
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFBooleanGetTypeID() -> usize;
}

unsafe fn object(receiver: *mut c_void, selector: &'static [u8]) -> *mut c_void {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    send(receiver, sel_registerName(selector.as_ptr().cast()))
}

unsafe fn string(receiver: *mut c_void) -> Option<String> {
    if receiver.is_null() {
        return None;
    }
    let pointer = object(receiver, b"UTF8String\0").cast::<c_char>();
    if pointer.is_null() {
        None
    } else {
        Some(CStr::from_ptr(pointer).to_string_lossy().into_owned())
    }
}

unsafe fn is_fullscreen(pid: i32) -> bool {
    let application = AXUIElementCreateApplication(pid);
    if application.is_null() {
        return false;
    }
    AXUIElementSetMessagingTimeout(application, 0.05);
    let focused = CFString::new("AXFocusedWindow");
    let mut window = std::ptr::null();
    let has_window = AXUIElementCopyAttributeValue(
        application,
        focused.as_concrete_TypeRef().cast(),
        &mut window,
    ) == 0;
    let mut fullscreen = false;
    if has_window && !window.is_null() {
        AXUIElementSetMessagingTimeout(window, 0.05);
        let attribute = CFString::new("AXFullScreen");
        let mut value = std::ptr::null();
        if AXUIElementCopyAttributeValue(window, attribute.as_concrete_TypeRef().cast(), &mut value)
            == 0
            && !value.is_null()
        {
            fullscreen = CFGetTypeID(value) == CFBooleanGetTypeID()
                && value
                    == core_foundation::boolean::CFBoolean::true_value()
                        .as_concrete_TypeRef()
                        .cast();
            CFRelease(value);
        }
        CFRelease(window);
    }
    CFRelease(application);
    fullscreen
}

pub(super) fn foreground_application() -> ForegroundApplication {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let workspace = object(
            objc_getClass(b"NSWorkspace\0".as_ptr().cast()),
            b"sharedWorkspace\0",
        );
        let application = object(workspace, b"frontmostApplication\0");
        let send_pid: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
            std::mem::transmute(objc_msgSend as *const ());
        let pid = send_pid(
            application,
            sel_registerName(b"processIdentifier\0".as_ptr().cast()),
        );
        let result = if pid == 0 || pid == std::process::id() as i32 {
            ForegroundApplication::default()
        } else {
            let names = [
                string(object(application, b"bundleIdentifier\0")),
                string(object(application, b"localizedName\0")),
            ]
            .into_iter()
            .flatten()
            .collect();
            ForegroundApplication {
                names,
                fullscreen: is_fullscreen(pid),
            }
        };
        objc_autoreleasePoolPop(pool);
        result
    }
}
