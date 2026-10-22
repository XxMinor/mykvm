use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub(crate) fn validate_helper_binary(path: &Path) -> Result<(), String> {
    let invalid = || {
        format!("input helper is missing or is not a valid Windows executable: {}. Reinstall MyKVM using the complete installer.", path.display())
    };
    let mut file = File::open(path).map_err(|error| format!("{} ({error})", invalid()))?;
    let length = file.metadata().map_err(|_| invalid())?.len();
    let mut dos = [0_u8; 64];
    file.read_exact(&mut dos).map_err(|_| invalid())?;
    if &dos[..2] != b"MZ" {
        return Err(invalid());
    }
    let pe_offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if pe_offset < 64 || pe_offset.saturating_add(24) > length {
        return Err(invalid());
    }
    file.seek(SeekFrom::Start(pe_offset))
        .map_err(|_| invalid())?;
    let mut pe = [0_u8; 6];
    file.read_exact(&mut pe).map_err(|_| invalid())?;
    let machine = u16::from_le_bytes([pe[4], pe[5]]);
    if &pe[..4] != b"PE\0\0" || !matches!(machine, 0x014c | 0x8664 | 0xaa64) {
        return Err(invalid());
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct OperationResult {
    process_id: u32,
    error: Option<String>,
}

#[cfg(target_os = "windows")]
fn result_path() -> Result<std::path::PathBuf, String> {
    let helper = crate::protected_input_helper_path()?;
    Ok(helper.with_file_name("input-service-install.json"))
}

#[cfg(target_os = "windows")]
pub(crate) fn record_result(result: &Result<(), String>) {
    let recorded = (|| -> Result<(), String> {
        let path = result_path()?;
        let directory = path
            .parent()
            .ok_or("input service result directory is missing")?;
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
        crate::secure_protected_input_helper_path(directory)?;
        let result = OperationResult {
            process_id: std::process::id(),
            error: result.as_ref().err().cloned(),
        };
        std::fs::write(
            &path,
            serde_json::to_vec(&result).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        // The unelevated parent must read the result, while only administrators
        // and SYSTEM may change it. Do not grant write access to the helper.
        crate::set_windows_path_security(&path, "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GR;;;AU)")
    })();
    if let Err(error) = recorded {
        eprintln!("failed to record input service installation result: {error}");
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn run_elevated(args: &[String]) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::Threading::{GetExitCodeProcess, GetProcessId, WaitForSingleObject},
    };
    let process = crate::launch_current_process_as_admin_with_handle(args)?;
    let handle = process.as_raw_handle().cast();
    let process_id = unsafe { GetProcessId(handle) };
    if unsafe { WaitForSingleObject(handle, 120_000) } != WAIT_OBJECT_0 {
        return Err("Timed out waiting for the administrator input-service operation. Check MyKVM/input-service-install.json in Program Files.".into());
    }
    let mut exit_code = 0;
    if unsafe { GetExitCodeProcess(handle, &mut exit_code) } == 0 {
        return Err(crate::windows_last_error(
            "GetExitCodeProcess(input service installer)",
        ));
    }
    if exit_code == 0 {
        return Ok(());
    }
    let detail = result_path()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<OperationResult>(&bytes).ok())
        .filter(|result| result.process_id == process_id)
        .and_then(|result| result.error)
        .unwrap_or_else(|| format!("administrator process exited with code {exit_code}"));
    Err(format!("Input service operation failed: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_validation_rejects_missing_empty_and_truncated_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper.exe");
        assert!(validate_helper_binary(&path).is_err());
        for bytes in [Vec::new(), b"MZ".to_vec(), vec![0; 128]] {
            std::fs::write(&path, bytes).unwrap();
            assert!(validate_helper_binary(&path).is_err());
        }
    }

    #[test]
    fn helper_validation_checks_pe_offset_and_signature() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper.exe");
        let mut bytes = vec![0_u8; 128];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64_u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664_u16.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(validate_helper_binary(&path).is_ok());
        bytes[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(validate_helper_binary(&path).is_err());
    }

    #[test]
    fn operation_result_preserves_native_error_and_child_identity() {
        let result = OperationResult {
            process_id: 42,
            error: Some("StartServiceW failed (error 193)".into()),
        };
        let decoded: OperationResult =
            serde_json::from_slice(&serde_json::to_vec(&result).unwrap()).unwrap();
        assert_eq!(decoded.process_id, 42);
        assert_eq!(decoded.error, result.error);
    }
}
