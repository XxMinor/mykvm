use std::{
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
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
    let (app_cpu_percent, app_memory_mb) = if cfg!(target_os = "windows") {
        read_windows_process_performance().unwrap_or((0.0, 0.0))
    } else {
        read_unix_process_performance().unwrap_or((0.0, 0.0))
    };

    PerformanceSample {
        timestamp_ms: now_ms(),
        app_cpu_percent: app_cpu_percent.clamp(0.0, 100.0),
        app_memory_mb: app_memory_mb.max(0.0),
        transport_packets,
        input_events,
        clipboard_packets,
    }
}

fn read_unix_process_performance() -> Result<(f64, f64), String> {
    let pid = std::process::id().to_string();
    let output = command_stdout(Command::new("ps").args(["-p", &pid, "-o", "%cpu=,rss="]))?;
    parse_process_metrics(&output)
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

#[cfg(not(target_os = "windows"))]
fn read_windows_process_performance() -> Result<(f64, f64), String> {
    Err("windows process performance is unavailable on this platform".into())
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

#[allow(dead_code)]
pub(crate) fn read_system_overview_sample() -> PerformanceSample {
    let (app_cpu_percent, app_memory_mb, _memory_total_mb) = if cfg!(target_os = "macos") {
        read_macos_performance().unwrap_or((0.0, 0.0, 0.0))
    } else if cfg!(target_os = "windows") {
        read_windows_performance().unwrap_or((0.0, 0.0, 0.0))
    } else {
        read_linux_performance().unwrap_or((0.0, 0.0, 0.0))
    };

    PerformanceSample {
        timestamp_ms: now_ms(),
        app_cpu_percent: app_cpu_percent.clamp(0.0, 100.0),
        app_memory_mb,
        transport_packets: 0,
        input_events: 0,
        clipboard_packets: 0,
    }
}

fn read_macos_performance() -> Result<(f64, f64, f64), String> {
    let cpu_total = command_stdout(
        Command::new("sh").args(["-c", "ps -A -o %cpu= | awk '{s+=$1} END{print s+0}'"]),
    )?
    .trim()
    .parse::<f64>()
    .unwrap_or(0.0);
    let cpu_count = command_stdout(Command::new("sysctl").args(["-n", "hw.logicalcpu"]))?
        .trim()
        .parse::<f64>()
        .unwrap_or(1.0)
        .max(1.0);
    let total_bytes = command_stdout(Command::new("sysctl").args(["-n", "hw.memsize"]))?
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0);
    let vm_stat = command_stdout(&mut Command::new("vm_stat"))?;
    let page_size = vm_stat
        .lines()
        .next()
        .and_then(|line| line.split("page size of ").nth(1))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(4096.0);
    let free_pages = vm_stat_pages(&vm_stat, "Pages free")
        + vm_stat_pages(&vm_stat, "Pages inactive")
        + vm_stat_pages(&vm_stat, "Pages speculative");
    let total_mb = total_bytes / 1024.0 / 1024.0;
    let free_mb = free_pages * page_size / 1024.0 / 1024.0;
    let used_mb = (total_mb - free_mb).max(0.0);

    Ok((cpu_total / cpu_count, used_mb, total_mb))
}

fn read_windows_performance() -> Result<(f64, f64, f64), String> {
    let output = command_stdout(Command::new("powershell").args([
        "-NoProfile",
        "-Command",
        "$cpu=(Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average; $os=Get-CimInstance Win32_OperatingSystem; $total=[math]::Round($os.TotalVisibleMemorySize/1024,2); $free=[math]::Round($os.FreePhysicalMemory/1024,2); Write-Output \"$cpu,$($total-$free),$total\"",
    ]))?;
    parse_metric_triplet(&output)
}

fn read_linux_performance() -> Result<(f64, f64, f64), String> {
    let output = command_stdout(Command::new("sh").args([
        "-c",
        "cpu=$(top -bn1 | awk '/Cpu\\(s\\)/ {print 100-$8; exit}'); mem=$(awk '/MemTotal/ {t=$2} /MemAvailable/ {a=$2} END {printf \"%.2f,%.2f\", (t-a)/1024, t/1024}' /proc/meminfo); echo \"$cpu,$mem\"",
    ]))?;
    parse_metric_triplet(&output)
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

fn parse_metric_triplet(output: &str) -> Result<(f64, f64, f64), String> {
    let values = output
        .trim()
        .split(',')
        .map(|value| value.trim().parse::<f64>().unwrap_or(0.0))
        .collect::<Vec<_>>();
    if values.len() >= 3 {
        Ok((values[0], values[1], values[2]))
    } else {
        Err("performance command did not return cpu, memory used, memory total".into())
    }
}

fn vm_stat_pages(vm_stat: &str, label: &str) -> f64 {
    vm_stat
        .lines()
        .find(|line| line.trim_start().starts_with(label))
        .and_then(|line| line.split(':').nth(1))
        .map(|value| value.trim().trim_end_matches('.').replace('.', ""))
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
