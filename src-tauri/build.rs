fn main() {
    expose_build_commit();
    ensure_windows_input_helper_sidecar_placeholder();
    tauri_build::build()
}

fn expose_build_commit() {
    println!("cargo:rerun-if-env-changed=MYKVM_BUILD_COMMIT");

    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| ".".into()),
    );
    watch_git_path(&manifest_dir, "HEAD");
    if let Some(reference) = git_output(&manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(&manifest_dir, &reference);
    }

    let commit = std::env::var("MYKVM_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_output(&manifest_dir, &["rev-parse", "--short=12", "HEAD"]));
    if let Some(commit) = commit {
        println!("cargo:rustc-env=MYKVM_BUILD_COMMIT={}", commit.trim());
    }
}

fn watch_git_path(manifest_dir: &std::path::Path, name: &str) {
    let Some(path) = git_output(manifest_dir, &["rev-parse", "--git-path", name]) else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        manifest_dir.join(path)
    };
    println!("cargo:rerun-if-changed={}", path.display());
}

fn git_output(manifest_dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
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
