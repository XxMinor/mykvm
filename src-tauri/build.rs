fn main() {
    ensure_windows_input_helper_sidecar_placeholder();
    tauri_build::build()
}

fn ensure_windows_input_helper_sidecar_placeholder() {
    let Ok(target) = std::env::var("TARGET") else {
        return;
    };
    if !target.contains("windows") {
        return;
    }

    let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    let sidecar = std::path::PathBuf::from(manifest_dir)
        .join("binaries")
        .join(format!("mykvm-input-helper-{target}.exe"));
    if sidecar.exists() {
        return;
    }

    if let Some(parent) = sidecar.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(sidecar, []);
}
