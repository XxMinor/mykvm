use std::process::Command;

#[cfg(target_os = "windows")]
use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PerformanceSample {
    timestamp_ms: u64,
    app_cpu_percent: f64,
    app_memory_mb: f64,
    transport_packets: u64,
    input_events: u64,
    clipboard_packets: u64,
}

#[cfg(target_os = "windows")]
static WINDOWS_PROCESS_SAMPLE: OnceLock<Mutex<Option<WindowsProcessSample>>> = OnceLock::new();

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct WindowsProcessSample {
    instant: Instant,
    process_time_100ns: u64,
}

pub(crate) fn read_process_sample(
    transport_packets: u64,
    input_events: u64,
    clipboard_packets: u64,
) -> PerformanceSample {
    let (app_cpu_percent, app_memory_mb) =
        read_platform_process_performance().unwrap_or((0.0, 0.0));

    PerformanceSample {
        timestamp_ms: crate::now_ms(),
        app_cpu_percent: app_cpu_percent.clamp(0.0, 100.0),
        app_memory_mb: app_memory_mb.max(0.0),
        transport_packets,
        input_events,
        clipboard_packets,
    }
}

#[cfg(target_os = "windows")]
fn read_platform_process_performance() -> Result<(f64, f64), String> {
    read_windows_process_performance()
}

#[cfg(target_os = "macos")]
fn read_platform_process_performance() -> Result<(f64, f64), String> {
    let (cpu_percent, rss_memory_mb) = read_unix_process_performance()?;
    let memory_mb = read_macos_physical_footprint_mb().unwrap_or(rss_memory_mb);
    Ok((cpu_percent, memory_mb))
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn read_platform_process_performance() -> Result<(f64, f64), String> {
    read_unix_process_performance()
}

fn read_unix_process_performance() -> Result<(f64, f64), String> {
    let pid = std::process::id().to_string();
    let output = command_stdout(Command::new("ps").args(["-p", &pid, "-o", "%cpu=,rss="]))?;
    parse_process_metrics(&output)
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct RUsageInfoV0 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
}

#[cfg(target_os = "macos")]
fn read_macos_physical_footprint_mb() -> Result<f64, String> {
    use std::ffi::c_void;

    const RUSAGE_INFO_V0: i32 = 0;

    #[link(name = "proc")]
    extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut c_void) -> i32;
    }

    let mut info = RUsageInfoV0::default();
    let result = unsafe {
        proc_pid_rusage(
            std::process::id() as i32,
            RUSAGE_INFO_V0,
            &mut info as *mut RUsageInfoV0 as *mut c_void,
        )
    };
    if result == 0 {
        Ok(info.ri_phys_footprint as f64 / 1024.0 / 1024.0)
    } else {
        Err("failed to read macOS process physical footprint".into())
    }
}

#[cfg(target_os = "windows")]
fn read_windows_process_performance() -> Result<(f64, f64), String> {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
    };

    let process = unsafe { GetCurrentProcess() };
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    let memory_ok = unsafe { GetProcessMemoryInfo(process, &mut counters, counters.cb) };
    if memory_ok == 0 {
        return Err("failed to read process memory counters".into());
    }

    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();
    let time_ok = unsafe {
        GetProcessTimes(
            process,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
    };
    if time_ok == 0 {
        return Err("failed to read process cpu counters".into());
    }

    let now = Instant::now();
    let process_time_100ns = filetime_to_u64(&kernel_time) + filetime_to_u64(&user_time);
    let cpu_percent = {
        let sample = WINDOWS_PROCESS_SAMPLE.get_or_init(|| Mutex::new(None));
        let mut previous = sample
            .lock()
            .map_err(|_| "windows process sample lock poisoned".to_string())?;
        let cpu_percent = previous
            .map(|previous_sample| {
                let process_delta =
                    process_time_100ns.saturating_sub(previous_sample.process_time_100ns);
                let elapsed_100ns =
                    now.duration_since(previous_sample.instant).as_secs_f64() * 10_000_000.0;
                let cpu_count = std::thread::available_parallelism()
                    .map(|count| count.get())
                    .unwrap_or(1) as f64;

                if elapsed_100ns > 0.0 {
                    (process_delta as f64 / elapsed_100ns / cpu_count) * 100.0
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        *previous = Some(WindowsProcessSample {
            instant: now,
            process_time_100ns,
        });
        cpu_percent
    };

    Ok((
        cpu_percent,
        counters.WorkingSetSize as f64 / 1024.0 / 1024.0,
    ))
}

fn parse_process_metrics(output: &str) -> Result<(f64, f64), String> {
    let values = output
        .trim()
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().parse::<f64>().unwrap_or(0.0))
        .collect::<Vec<_>>();

    if values.len() >= 2 {
        Ok((
            values[0],
            values[1]
                / if cfg!(target_os = "windows") {
                    1.0
                } else {
                    1024.0
                },
        ))
    } else {
        Err("performance command did not return process cpu and memory".into())
    }
}

#[cfg(target_os = "windows")]
fn filetime_to_u64(filetime: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64
}

fn command_stdout(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("failed to run performance command: {error}"))?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("performance command returned invalid UTF-8: {error}"))
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unix_ps_cpu_and_rss_kb() {
        let (cpu, memory_mb) = parse_process_metrics(" 2.5 115664\n").expect("metrics");

        assert_eq!(cpu, 2.5);
        assert!((memory_mb - 112.953125).abs() < f64::EPSILON);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_macos_physical_footprint() {
        let memory_mb = read_macos_physical_footprint_mb().expect("physical footprint");

        assert!(memory_mb > 0.0);
    }
}
