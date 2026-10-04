use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

#[cfg(target_os = "windows")]
#[path = "input_guard_windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "input_guard_macos.rs"]
mod platform;

fn enabled() -> bool {
    false
}
fn default_lock_hotkey() -> String {
    "disabled".into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InputProtection {
    #[serde(default)]
    pub local_only: bool,
    #[serde(default = "enabled")]
    pub protect_fullscreen: bool,
    #[serde(default)]
    pub blocked_applications: Vec<String>,
    #[serde(default = "default_lock_hotkey")]
    pub lock_hotkey: String,
}

impl Default for InputProtection {
    fn default() -> Self {
        Self {
            local_only: false,
            protect_fullscreen: false,
            blocked_applications: Vec::new(),
            lock_hotkey: default_lock_hotkey(),
        }
    }
}

#[cfg(test)]
impl InputProtection {
    pub fn normalize(&mut self) -> Result<(), String> {
        let mut names = Vec::new();
        for raw in &self.blocked_applications {
            let name = raw.trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            if name.len() > 256 || name.contains(['\n', '\r', '/', '\\', '*', '?']) {
                return Err("应用名单请填写程序名或 bundle ID，不使用路径或通配符。".into());
            }
            if !names.contains(&name) {
                names.push(name);
            }
        }
        if names.len() > 64 {
            return Err("禁止跨屏的应用最多可设置 64 个。".into());
        }
        self.blocked_applications = names;
        self.lock_hotkey = crate::canonical_runtime_toggle_shortcut(&self.lock_hotkey)?
            .unwrap_or_else(|| "disabled".into());
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ForegroundApplication {
    pub names: Vec<String>,
    pub fullscreen: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PauseReason {
    LocalOnly,
    Fullscreen,
    Application,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GuardStatus {
    pub reason: Option<PauseReason>,
    pub application: String,
    pub capture_available: bool,
}

impl Default for GuardStatus {
    fn default() -> Self {
        Self {
            reason: None,
            application: String::new(),
            capture_available: true,
        }
    }
}

pub(crate) struct GuardState {
    paused: AtomicBool,
    capture_available: AtomicBool,
    screenshot_active: AtomicBool,
    snipaste_running: AtomicBool,
    status: Mutex<GuardStatus>,
    notifier: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for GuardState {
    fn default() -> Self {
        Self {
            paused: AtomicBool::new(false),
            capture_available: AtomicBool::new(true),
            screenshot_active: AtomicBool::new(false),
            snipaste_running: AtomicBool::new(false),
            status: Mutex::new(GuardStatus::default()),
            notifier: Mutex::new(None),
        }
    }
}

pub(crate) fn set_notifier(guard: &SharedGuard, notify: Arc<dyn Fn() + Send + Sync>) {
    if let Ok(mut notifier) = guard.notifier.lock() {
        *notifier = Some(notify);
    }
}

fn notify(guard: &SharedGuard) {
    let callback = guard
        .notifier
        .lock()
        .ok()
        .and_then(|callback| callback.clone());
    if let Some(callback) = callback {
        callback();
    }
}

pub(crate) type SharedGuard = Arc<GuardState>;

pub(crate) fn snapshot(guard: &SharedGuard) -> GuardStatus {
    let mut status = guard
        .status
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default();
    status.capture_available = guard.capture_available.load(Ordering::Relaxed);
    status
}

pub(crate) fn spawn_monitor(
    layout: Arc<Mutex<crate::LayoutState>>,
    guard: SharedGuard,
    remote_active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut protection = InputProtection::default();
        #[cfg(target_os = "windows")]
        let mut last_snipaste_check = std::time::Instant::now() - Duration::from_secs(2);
        while !stop.load(Ordering::Relaxed) {
            if let Ok(layout) = layout.try_lock() {
                protection = layout.input_protection.clone();
            }
            let remote = remote_active.load(Ordering::Relaxed);
            // Window/AX queries may block on another app. Keep them on this
            // monitor thread, never on the native input callback/capture loop.
            let foreground = if cfg!(target_os = "windows") || (!protection.local_only
                && !remote
                && (protection.protect_fullscreen || !protection.blocked_applications.is_empty()))
            {
                foreground_application()
            } else {
                ForegroundApplication::default()
            };
            guard.screenshot_active.store(foreground.fullscreen && foreground.names.iter().any(|name| name.eq_ignore_ascii_case("snipaste.exe")), Ordering::Relaxed);
            #[cfg(target_os = "windows")]
            if last_snipaste_check.elapsed() >= Duration::from_secs(2) {
                last_snipaste_check = std::time::Instant::now();
                guard.snipaste_running.store(platform::snipaste_is_running(), Ordering::Relaxed);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            update_status(
                &guard,
                pause_reason(&protection, &foreground, remote),
                &foreground,
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    });
}

pub(crate) fn screenshot_is_active(guard: &SharedGuard) -> bool {
    guard.screenshot_active.load(Ordering::Relaxed)
}
pub(crate) fn snipaste_is_running(guard: &SharedGuard) -> bool {
    guard.snipaste_running.load(Ordering::Relaxed)
}

pub(crate) fn pause_reason(
    protection: &InputProtection,
    foreground: &ForegroundApplication,
    remote_active: bool,
) -> Option<PauseReason> {
    if protection.local_only {
        return Some(PauseReason::LocalOnly);
    }
    // Automatic rules prevent entering another screen; they never revoke an
    // existing remote session just because the controller's foreground changed.
    if remote_active {
        return None;
    }
    if foreground.names.iter().any(|name| {
        protection
            .blocked_applications
            .iter()
            .any(|blocked| blocked.eq_ignore_ascii_case(name))
    }) {
        return Some(PauseReason::Application);
    }
    if protection.protect_fullscreen && foreground.fullscreen {
        return Some(PauseReason::Fullscreen);
    }
    None
}

pub(crate) fn update_status(
    guard: &SharedGuard,
    reason: Option<PauseReason>,
    foreground: &ForegroundApplication,
) {
    guard.paused.store(reason.is_some(), Ordering::Relaxed);
    let mut changed = false;
    if let Ok(mut status) = guard.status.lock() {
        changed = status.reason != reason;
        if changed {
            log::info!("input protection: {reason:?}");
        }
        status.reason = reason;
        if let Some(name) = foreground.names.first() {
            status.application = name.clone();
        }
    }
    if changed {
        notify(guard);
    }
}

pub(crate) fn is_paused(guard: &SharedGuard) -> bool {
    should_suspend(guard) || !guard.capture_available.load(Ordering::Relaxed)
}

pub(crate) fn should_suspend(guard: &SharedGuard) -> bool {
    guard.paused.load(Ordering::Relaxed)
}
pub(crate) fn set_capture_available(guard: &SharedGuard, available: bool) {
    if guard.capture_available.swap(available, Ordering::Relaxed) != available {
        notify(guard);
    }
}

pub(crate) fn suspend_for_session(guard: &SharedGuard, remote_active: bool) -> bool {
    if !should_suspend(guard) {
        return false;
    }
    !remote_active || snapshot(guard).reason == Some(PauseReason::LocalOnly)
}

pub(crate) fn foreground_application() -> ForegroundApplication {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        platform::foreground_application()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        ForegroundApplication::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_config_defaults_to_manual_control_without_extra_protection_shortcuts() {
        let protection: InputProtection = serde_json::from_str("{}").unwrap();
        assert!(!protection.protect_fullscreen);
        assert!(!protection.local_only);
        assert_eq!(protection.lock_hotkey, "disabled");
    }

    #[test]
    fn automatic_rules_block_entry_without_pulling_an_existing_session_back() {
        let mut protection = InputProtection::default();
        protection.blocked_applications = vec!["game.exe".into()];
        let app = ForegroundApplication {
            names: vec!["GAME.EXE".into()],
            fullscreen: true,
        };
        assert_eq!(
            pause_reason(&protection, &app, false),
            Some(PauseReason::Application)
        );
        assert_eq!(pause_reason(&protection, &app, true), None);
        protection.local_only = true;
        assert_eq!(
            pause_reason(&protection, &app, true),
            Some(PauseReason::LocalOnly)
        );
    }

    #[test]
    fn leaving_a_fullscreen_app_restores_capture_and_disabling_the_rule_allows_entry() {
        let mut protection = InputProtection::default();
        protection.protect_fullscreen = true;
        let mut app = ForegroundApplication {
            names: vec!["player.exe".into()],
            fullscreen: true,
        };
        assert_eq!(
            pause_reason(&protection, &app, false),
            Some(PauseReason::Fullscreen)
        );
        app.fullscreen = false;
        assert_eq!(pause_reason(&protection, &app, false), None);
        app.fullscreen = true;
        protection.protect_fullscreen = false;
        assert_eq!(pause_reason(&protection, &app, false), None);
    }

    #[test]
    fn application_names_are_exact_and_paths_are_rejected() {
        let mut protection = InputProtection {
            blocked_applications: vec![" GAME.EXE ".into(), "game.exe".into()],
            ..Default::default()
        };
        protection.normalize().unwrap();
        assert_eq!(protection.blocked_applications, vec!["game.exe"]);
        let app = ForegroundApplication {
            names: vec!["other-game.exe".into()],
            fullscreen: false,
        };
        assert_eq!(pause_reason(&protection, &app, false), None);
        protection.blocked_applications = vec!["C:\\Games\\game.exe".into()];
        assert!(protection.normalize().is_err());
    }
}
