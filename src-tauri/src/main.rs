// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if mykvm_lib::handle_process_control_args() {
        return;
    }

    if !mykvm_lib::acquire_single_instance() {
        // A duplicate login launch must not raise the window of the instance
        // that won the lock.
        if !mykvm_lib::launched_from_autostart() {
            mykvm_lib::activate_existing_instance();
        }
        return;
    }

    // Installed Windows builds request elevation before initializing input,
    // networking or the UI. Keep development launches usable under cargo.
    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    match mykvm_lib::relaunch_as_admin_if_needed() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    }

    mykvm_lib::run();
}
