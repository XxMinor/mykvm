#![cfg(target_os = "windows")]

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, SetWindowsHookExW,
        UnhookWindowsHookEx, HOOKPROC, MSG, PM_NOREMOVE, PM_REMOVE, QS_ALLINPUT, QS_SENDMESSAGE,
        WH_KEYBOARD_LL,
    },
};

/// Normally the keyboard hook has its own queue. Recovery can instead use the
/// known-live mouse queue after repeated installs produce no actual callbacks.
pub(crate) struct KeyboardCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    owner_hook: Option<(isize, Arc<AtomicU32>, u32)>,
}

impl KeyboardCapture {
    pub(crate) fn start(owner: Arc<AtomicU32>, callback: HOOKPROC) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("mykvm-keyboard-capture".into())
            .spawn(move || {
                let id = unsafe { GetCurrentThreadId() };
                let mut message = MSG::default();
                // Establish this thread's queue before installing a global hook.
                unsafe {
                    PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, 0);
                }
                owner.store(id, Ordering::Relaxed);
                let hook =
                    unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, callback, std::ptr::null_mut(), 0) };
                if hook.is_null() {
                    let error = format!(
                        "failed to install Windows keyboard hook: {}",
                        std::io::Error::last_os_error()
                    );
                    let _ = owner.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
                    let _ = ready_tx.send(Err(error));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                while !worker_stop.load(Ordering::Relaxed) {
                    unsafe {
                        while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0
                        {
                            DispatchMessageW(&message);
                        }
                        MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 20, QS_ALLINPUT);
                    }
                }
                unsafe {
                    UnhookWindowsHookEx(hook);
                }
                let _ = owner.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
            })
            .map_err(|error| error.to_string())?;
        let mut capture = Self {
            stop,
            thread: Some(thread),
            owner_hook: None,
        };
        match ready_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(error)) => {
                capture.stop();
                Err(error)
            }
            Err(error) => {
                capture.stop();
                Err(format!(
                    "keyboard capture worker did not become ready: {error}"
                ))
            }
        }
    }

    /// If a rebuilt dedicated queue still receives no physical callbacks,
    /// use the capture queue whose mouse hook is demonstrably alive.
    pub(crate) fn start_on_current_thread(
        owner: Arc<AtomicU32>,
        callback: HOOKPROC,
    ) -> Result<Self, String> {
        let id = unsafe { GetCurrentThreadId() };
        owner.store(id, Ordering::Relaxed);
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, callback, std::ptr::null_mut(), 0) };
        if hook.is_null() {
            let error = format!(
                "failed to install Windows keyboard hook on the capture thread: {}",
                std::io::Error::last_os_error()
            );
            let _ = owner.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
            return Err(error);
        }
        Ok(Self {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            owner_hook: Some((hook as isize, owner, id)),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some((hook, owner, id)) = self.owner_hook.take() {
            unsafe {
                UnhookWindowsHookEx(hook as _);
            }
            let _ = owner.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
        }
        if let Some(thread) = self.thread.take() {
            // A callback can synchronously ask the mouse thread to service a
            // cursor warp. Keep sent messages/hook callbacks moving while we
            // wait; blocking this thread in join() can remove the mouse hook.
            let mut message = MSG::default();
            while !thread.is_finished() {
                unsafe {
                    PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
                    MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 10, QS_SENDMESSAGE);
                }
            }
            let _ = thread.join();
        }
    }
}
impl Drop for KeyboardCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
