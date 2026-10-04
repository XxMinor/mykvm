use std::time::{Duration, Instant};

/// Compare keyboard input with keyboard callbacks only. Mouse activity cannot
/// prove that WH_KEYBOARD_LL is still installed.
#[derive(Default)]
pub(crate) struct KeyboardHookWatchdog {
    last_raw_tick: u32,
    missing_since: Option<Instant>,
}

impl KeyboardHookWatchdog {
    pub(crate) fn reset(&mut self, raw_tick: u32) {
        self.last_raw_tick = raw_tick;
        self.missing_since = None;
    }

    pub(crate) fn observe(&mut self, hook_tick: u32, raw_tick: u32, now: Instant) -> bool {
        let raw_progress = raw_tick != 0
            && (self.last_raw_tick == 0 || raw_tick.wrapping_sub(self.last_raw_tick) as i32 > 0);
        self.last_raw_tick = raw_tick;
        // WM_INPUT's posted time precedes the matching hook's receipt time.
        // Allow a small timing margin and service the queue before observing.
        if raw_tick == 0 || (hook_tick != 0 && raw_tick.wrapping_sub(hook_tick) as i32 <= 100) {
            self.missing_since = None;
            return false;
        }
        if raw_progress {
            self.missing_since.get_or_insert(now);
        }
        // One missed key is enough evidence. Unlike GetLastInputInfo, this is
        // keyboard-specific, so idle after that key must not erase the evidence.
        self.missing_since
            .is_some_and(|since| now.saturating_duration_since(since) >= Duration::from_millis(250))
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::{
        mem::size_of,
        ptr,
        sync::{
            atomic::{AtomicU32, Ordering},
            Arc, Mutex,
        },
    };
    use windows_sys::Win32::{
        Foundation::HWND,
        UI::{
            Input::{
                GetRawInputData, RegisterRawInputDevices, RAWINPUTDEVICE, RAWINPUTHEADER,
                RIDEV_INPUTSINK, RIDEV_REMOVE, RID_HEADER, RIM_TYPEKEYBOARD,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_MESSAGE, MSG, WM_INPUT,
            },
        },
    };

    // Capture threads can overlap briefly during restart. An old monitor must
    // not unregister the new thread's keyboard sink when it is dropped.
    static REGISTRATION_OWNER: Mutex<usize> = Mutex::new(0);

    pub(crate) struct KeyboardMonitor {
        window: HWND,
        raw_tick: Arc<AtomicU32>,
    }

    impl KeyboardMonitor {
        pub(crate) fn new(raw_tick: Arc<AtomicU32>) -> Option<Self> {
            let mut owner = REGISTRATION_OWNER.lock().ok()?;
            let window = unsafe {
                CreateWindowExW(
                    0,
                    windows_sys::w!("STATIC"),
                    ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if window.is_null() {
                log::warn!(
                    "could not create keyboard health monitor: {}",
                    std::io::Error::last_os_error()
                );
                return None;
            }
            // Only keyboard headers are observed; high-rate mouse input does
            // not acquire another listener. No legacy messages are suppressed.
            let device = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: window,
            };
            if unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) }
                == 0
            {
                log::warn!(
                    "could not register keyboard health monitor: {}",
                    std::io::Error::last_os_error()
                );
                unsafe {
                    DestroyWindow(window);
                }
                return None;
            }
            *owner = window as usize;
            raw_tick.store(0, Ordering::Relaxed);
            Some(Self { window, raw_tick })
        }

        pub(crate) fn observe_message(&self, message: &MSG) {
            if message.message != WM_INPUT {
                return;
            }
            if message.hwnd == self.window {
                let mut header = RAWINPUTHEADER::default();
                let mut bytes = size_of::<RAWINPUTHEADER>() as u32;
                let read = unsafe {
                    GetRawInputData(
                        message.lParam as _,
                        RID_HEADER,
                        (&mut header as *mut RAWINPUTHEADER).cast(),
                        &mut bytes,
                        size_of::<RAWINPUTHEADER>() as u32,
                    )
                };
                if read == size_of::<RAWINPUTHEADER>() as u32 && header.dwType == RIM_TYPEKEYBOARD {
                    self.raw_tick.store(message.time, Ordering::Relaxed);
                }
            }
            // Required for WM_INPUT cleanup, including when our app has focus.
            unsafe {
                DefWindowProcW(
                    message.hwnd,
                    message.message,
                    message.wParam,
                    message.lParam,
                );
            }
        }
    }

    impl Drop for KeyboardMonitor {
        fn drop(&mut self) {
            if let Ok(mut owner) = REGISTRATION_OWNER.lock() {
                if *owner == self.window as usize {
                    let device = RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: 6,
                        dwFlags: RIDEV_REMOVE,
                        hwndTarget: ptr::null_mut(),
                    };
                    unsafe {
                        RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32);
                    }
                    *owner = 0;
                }
            }
            unsafe {
                DestroyWindow(self.window);
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) use platform::KeyboardMonitor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_keyboard_is_detected_even_when_the_user_stops_after_one_key() {
        let now = Instant::now();
        let mut watchdog = KeyboardHookWatchdog::default();
        assert!(!watchdog.observe(10_000, 10_000, now));
        assert!(!watchdog.observe(10_000, 90_000, now + Duration::from_secs(80)));
        assert!(!watchdog.observe(10_000, 90_000, now + Duration::from_millis(80_249)));
        assert!(watchdog.observe(10_000, 90_000, now + Duration::from_millis(80_250)));
        // A mouse callback changes neither keyboard timestamp nor the result.
        assert!(watchdog.observe(10_000, 90_000, now + Duration::from_secs(81)));
    }

    #[test]
    fn healthy_keyboard_and_late_queue_servicing_do_not_trigger_repair() {
        let now = Instant::now();
        let mut watchdog = KeyboardHookWatchdog::default();
        assert!(!watchdog.observe(10_000, 10_000, now));
        // A hook services an old queued event after a long idle.
        assert!(!watchdog.observe(90_100, 90_000, now + Duration::from_secs(80)));
        assert!(!watchdog.observe(90_100, 90_000, now + Duration::from_secs(90)));
        // Idle with no keyboard input is also healthy.
        assert!(!KeyboardHookWatchdog::default().observe(0, 0, now));
    }

    #[test]
    fn a_fresh_keyboard_callback_cancels_pending_recovery() {
        let now = Instant::now();
        let mut watchdog = KeyboardHookWatchdog::default();
        assert!(!watchdog.observe(10_000, 20_000, now));
        assert!(!watchdog.observe(20_001, 20_000, now + Duration::from_millis(100)));
        assert!(!watchdog.observe(20_001, 20_000, now + Duration::from_secs(1)));
    }

    #[test]
    fn reset_ignores_evidence_from_before_the_repair() {
        let now = Instant::now();
        let mut watchdog = KeyboardHookWatchdog::default();
        assert!(!watchdog.observe(0, 20_000, now));
        assert!(watchdog.observe(0, 20_000, now + Duration::from_millis(250)));
        watchdog.reset(20_000);
        assert!(!watchdog.observe(0, 20_000, now + Duration::from_secs(1)));
        assert!(!watchdog.observe(0, 21_000, now + Duration::from_secs(2)));
        assert!(watchdog.observe(0, 21_000, now + Duration::from_millis(2_250)));
    }

    #[test]
    fn keyboard_health_handles_tick_wraparound() {
        let now = Instant::now();
        let mut watchdog = KeyboardHookWatchdog::default();
        watchdog.reset(u32::MAX - 100);
        assert!(!watchdog.observe(u32::MAX - 100, 200, now));
        assert!(watchdog.observe(u32::MAX - 100, 200, now + Duration::from_millis(250)));
        assert!(!watchdog.observe(210, 200, now + Duration::from_millis(300)));
    }

    #[test]
    fn first_missed_key_is_detected_after_more_than_twenty_five_days_of_uptime() {
        let now = Instant::now();
        let mut watchdog = KeyboardHookWatchdog::default();
        let tick = i32::MAX as u32 + 10_000;
        assert!(!watchdog.observe(0, tick, now));
        assert!(watchdog.observe(0, tick, now + Duration::from_millis(250)));
    }
}
