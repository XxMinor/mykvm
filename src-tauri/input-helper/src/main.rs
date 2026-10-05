#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("mykvm-input-helper is only supported on Windows.");
}

#[cfg(target_os = "windows")]
fn main() {
    if let Err(error) = windows_helper::main() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(any(target_os = "windows", test))]
fn worker_status_due(
    previous: Option<&(std::time::Instant, String, String)>,
    now: std::time::Instant,
    desktop: &str,
    error: &str,
) -> bool {
    previous.map_or(true, |(written_at, old_desktop, old_error)| {
        desktop != old_desktop
            || error != old_error
            || now.saturating_duration_since(*written_at) >= std::time::Duration::from_secs(1)
    })
}

#[cfg(test)]
mod status_tests {
    #[test]
    fn unavailable_desktop_does_not_receive_new_keys() {
        use mykvm_lib::shared_input::InputCommand;
        let key = InputCommand::Key { key_code: 0x41, down: true };
        assert!(!super::worker_can_inject(false, &key));
        assert!(super::worker_can_inject(true, &key));
        assert!(super::worker_can_inject(false, &InputCommand::ReleaseAll));
        assert!(super::worker_can_inject(false, &InputCommand::SecureAttention));
    }

    #[test]
    fn status_writes_are_bounded_but_desktop_and_error_changes_are_immediate() {
        let now = std::time::Instant::now();
        let previous = (now, "Default".into(), String::new());
        assert!(super::worker_status_due(None, now, "Default", ""));
        assert!(!super::worker_status_due(
            Some(&previous),
            now,
            "Default",
            ""
        ));
        assert!(super::worker_status_due(
            Some(&previous),
            now,
            "Winlogon",
            ""
        ));
        assert!(super::worker_status_due(
            Some(&previous),
            now,
            "Default",
            "denied"
        ));
        assert!(super::worker_status_due(
            Some(&previous),
            now + std::time::Duration::from_secs(1),
            "Default",
            "",
        ));
    }
}

#[cfg(any(target_os = "windows", test))]
fn worker_can_inject(desktop_ready: bool, command: &mykvm_lib::shared_input::InputCommand) -> bool {
    desktop_ready || matches!(command,
        mykvm_lib::shared_input::InputCommand::ReleaseAll
        | mykvm_lib::shared_input::InputCommand::SecureAttention)
}

#[cfg(target_os = "windows")]
mod windows_helper {
    use std::{
        env,
        ffi::{OsStr, OsString},
        fs::{self, OpenOptions},
        io::Write as _,
        mem,
        path::PathBuf,
        ptr,
        sync::{mpsc, OnceLock},
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use mykvm_lib::{
        headless_client::{self, HeadlessClientHandle},
        shared_input::{
            decode_input_command, input_helper_status_path, input_pipe_name, InputCommand,
            INPUT_SERVICE_NAME, SERVICE_CONFIG_PATH_ARG, SERVICE_CONTROL_PIPE,
            SERVICE_OWNER_SID_ARG,
        },
        windows_input::{self, DesktopAttachment},
    };
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
            INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, SetTokenInformation,
            TokenPrimary, TokenSessionId, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
            TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
            TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
        },
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
                PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
            },
            RemoteDesktop::{
                ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
            },
            Threading::{
                CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
                TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
                CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
    };

    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    const INVALID_SESSION_ID: u32 = u32::MAX;
    const PIPE_BUFFER_SIZE: u32 = 64 * 1024;
    static SERVICE_ARGS: OnceLock<ServiceArgs> = OnceLock::new();

    pub fn main() -> Result<(), String> {
        let mut args = env::args_os().skip(1);
        match args.next().as_deref().and_then(OsStr::to_str) {
            Some("--service") => {
                let service_args = ServiceArgs::parse(args).map_err(|error| {
                    log_service_error("parse service arguments", &error);
                    error
                })?;
                if SERVICE_ARGS.set(service_args).is_err() {
                    return Err("service arguments already initialized".into());
                }
                service_dispatcher::start(INPUT_SERVICE_NAME, ffi_service_main)
                    .map_err(|error| format!("start service dispatcher: {error}"))
            }
            Some("--worker") => run_worker().map_err(|error| {
                log_service_error("input worker stopped with error", &error);
                error
            }),
            _ => Err("expected --service or --worker".into()),
        }
    }

    #[derive(Debug)]
    struct ServiceArgs {
        config_path: Option<PathBuf>,
        owner_sid: Option<String>,
    }

    impl ServiceArgs {
        fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Self, String> {
            let mut config_path = None;
            let mut owner_sid = None;
            while let Some(arg) = args.next() {
                if arg == OsStr::new(SERVICE_CONFIG_PATH_ARG) {
                    config_path =
                        Some(PathBuf::from(args.next().ok_or_else(|| {
                            format!("missing value for {SERVICE_CONFIG_PATH_ARG}")
                        })?));
                } else if arg == OsStr::new(SERVICE_OWNER_SID_ARG) {
                    owner_sid = Some(
                        args.next()
                            .ok_or_else(|| format!("missing value for {SERVICE_OWNER_SID_ARG}"))?
                            .into_string()
                            .map_err(|_| "owner SID is not valid Unicode".to_string())?,
                    );
                } else {
                    return Err(format!(
                        "unexpected service argument: {}",
                        arg.to_string_lossy()
                    ));
                }
            }

            match (&config_path, &owner_sid) {
                (Some(_), None) => return Err(format!("missing {SERVICE_OWNER_SID_ARG}")),
                (None, Some(_)) => return Err(format!("missing {SERVICE_CONFIG_PATH_ARG}")),
                _ => {}
            }
            Ok(Self {
                config_path,
                owner_sid,
            })
        }
    }

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            log_service_error("service stopped with error", &error.to_string());
        }
    }

    fn run_service() -> windows_service::Result<()> {
        let service_args = SERVICE_ARGS
            .get()
            .expect("service arguments are initialized before dispatch");
        let (event_tx, event_rx) = mpsc::channel::<ServiceEvent>();
        let handler_tx = event_tx.clone();
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                ServiceControl::Stop => {
                    let _ = handler_tx.send(ServiceEvent::Stop);
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::SessionChange(_) => {
                    let _ = handler_tx.send(ServiceEvent::SessionChange);
                    ServiceControlHandlerResult::NoError
                }
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(INPUT_SERVICE_NAME, event_handler)?;
        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SESSION_CHANGE,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        let mut worker = WorkerProcess::default();
        restart_worker(&mut worker);
        let mut receiver = service_args.config_path.as_ref().and_then(start_receiver);
        let mut network_leased = false;
        let mut network_shutdown_failed = false;
        if let Some(owner_sid) = service_args.owner_sid.clone() {
            let control_tx = event_tx.clone();
            if let Err(error) = thread::Builder::new()
                .name("mykvm-service-control".into())
                .spawn(move || run_control_pipe(control_tx, owner_sid))
            {
                log_service_error("start service control pipe", &error.to_string());
            }
        }

        loop {
            match event_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(ServiceEvent::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(ServiceEvent::Takeover(ack)) => {
                    if network_shutdown_failed {
                        let _ = ack.send(false);
                        continue;
                    }
                    if let Err(error) = stop_receiver(&mut receiver) {
                        log_service_error("stop headless receiver for GUI takeover", &error);
                        network_shutdown_failed = true;
                        network_leased = true;
                        let _ = ack.send(false);
                        continue;
                    }
                    network_leased = true;
                    let _ = ack.send(true);
                }
                Ok(ServiceEvent::LeaseReleased) => {
                    if let Err(error) = stop_receiver(&mut receiver) {
                        log_service_error("stop headless receiver after GUI lease", &error);
                    }
                    network_leased = false;
                    receiver = service_args.config_path.as_ref().and_then(start_receiver);
                }
                Ok(ServiceEvent::SessionChange) | Err(mpsc::RecvTimeoutError::Timeout) => {
                    if worker.needs_restart() {
                        restart_worker(&mut worker);
                    }
                    if !network_leased && receiver.is_none() {
                        receiver = service_args.config_path.as_ref().and_then(start_receiver);
                    }
                }
            }
        }

        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::StopPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 1,
            wait_hint: Duration::from_secs(7),
            process_id: None,
        })?;
        if let Err(error) = stop_receiver(&mut receiver) {
            log_service_error("stop headless receiver with service", &error);
        }
        worker.stop();
        status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        Ok(())
    }

    enum ServiceEvent {
        Stop,
        SessionChange,
        Takeover(mpsc::Sender<bool>),
        LeaseReleased,
    }

    fn restart_worker(worker: &mut WorkerProcess) {
        if let Err(error) = worker.restart() {
            log_service_error("start session input worker", &error);
        }
    }

    fn start_receiver(config_path: &PathBuf) -> Option<HeadlessClientHandle> {
        match headless_client::start(config_path.clone()) {
            Ok(receiver) => receiver,
            Err(error) => {
                log_service_error("start headless receiver", &error);
                None
            }
        }
    }

    fn stop_receiver(receiver: &mut Option<HeadlessClientHandle>) -> Result<(), String> {
        if let Some(mut handle) = receiver.take() {
            handle.stop_and_wait()?;
        }
        Ok(())
    }

    fn run_control_pipe(event_tx: mpsc::Sender<ServiceEvent>, owner_sid: String) {
        loop {
            let pipe = match create_control_pipe(&owner_sid) {
                Ok(pipe) => pipe,
                Err(error) => {
                    log_service_error("create service control pipe", &error);
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            let pipe = HandleGuard(pipe);
            if !connect_pipe(pipe.0) {
                continue;
            }

            let mut request = [0_u8; 1];
            if !matches!(read_exact_pipe(pipe.0, &mut request), Ok(true)) || request[0] != 1 {
                unsafe {
                    let _ = DisconnectNamedPipe(pipe.0);
                }
                continue;
            }

            let (ack_tx, ack_rx) = mpsc::channel();
            if event_tx.send(ServiceEvent::Takeover(ack_tx)).is_err() {
                return;
            }
            if !matches!(ack_rx.recv(), Ok(true)) {
                unsafe {
                    let _ = DisconnectNamedPipe(pipe.0);
                }
                continue;
            }

            let acknowledged = write_control_ack(pipe.0);
            if acknowledged {
                let mut held = [0_u8; 1];
                let _ = read_exact_pipe(pipe.0, &mut held);
            }
            unsafe {
                let _ = DisconnectNamedPipe(pipe.0);
            }
            drop(pipe);

            if event_tx.send(ServiceEvent::LeaseReleased).is_err() {
                return;
            }
        }
    }

    fn create_control_pipe(owner_sid: &str) -> Result<HANDLE, String> {
        let pipe_name_w = wide_null(SERVICE_CONTROL_PIPE);
        let security = PipeSecurity::for_owner_sid(owner_sid)?;
        let pipe = unsafe {
            CreateNamedPipeW(
                pipe_name_w.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                1,
                1,
                0,
                &security.attributes,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            return Err(last_error("CreateNamedPipeW(service control)"));
        }
        Ok(pipe)
    }

    fn write_control_ack(pipe: HANDLE) -> bool {
        let ack = [1_u8];
        let mut written = 0_u32;
        unsafe {
            WriteFile(
                pipe,
                ack.as_ptr(),
                ack.len() as u32,
                &mut written,
                ptr::null_mut(),
            ) != 0
                && written == ack.len() as u32
        }
    }

    #[derive(Default)]
    struct WorkerProcess {
        handle: HANDLE,
        session_id: u32,
    }

    impl WorkerProcess {
        /// True when the worker died, or the physical console moved to a
        /// different session while the worker stayed behind. An RDP attach or
        /// detach swaps the console to a LogonUI session (and back on unlock)
        /// without a session-change reason that names the console (issue #21),
        /// so the 2s service tick polls for drift instead of trusting reasons.
        fn needs_restart(&mut self) -> bool {
            if self.has_exited() {
                return true;
            }
            let console_session = unsafe { WTSGetActiveConsoleSessionId() };
            console_session != INVALID_SESSION_ID && console_session != self.session_id
        }

        fn restart(&mut self) -> Result<(), String> {
            self.stop();
            let session_id = unsafe { WTSGetActiveConsoleSessionId() };
            if session_id == INVALID_SESSION_ID {
                return Err("no active console session".into());
            }

            let handle = spawn_worker_in_session(session_id)?;
            self.handle = handle;
            self.session_id = session_id;
            Ok(())
        }

        fn stop(&mut self) {
            if self.handle.is_null() {
                return;
            }

            unsafe {
                let _ = TerminateProcess(self.handle, 0);
                let _ = CloseHandle(self.handle);
            }
            self.handle = ptr::null_mut();
            self.session_id = 0;
        }

        fn has_exited(&mut self) -> bool {
            if self.handle.is_null() {
                return true;
            }

            unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for WorkerProcess {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn spawn_worker_in_session(session_id: u32) -> Result<HANDLE, String> {
        unsafe {
            let mut service_token = ptr::null_mut();
            let token_access = TOKEN_DUPLICATE
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_QUERY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID;
            if OpenProcessToken(GetCurrentProcess(), token_access, &mut service_token) == 0 {
                return Err(last_error("OpenProcessToken"));
            }
            let _service_token = HandleGuard(service_token);

            let mut primary_token = ptr::null_mut();
            if DuplicateTokenEx(
                service_token,
                token_access,
                ptr::null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut primary_token,
            ) == 0
            {
                return Err(last_error("DuplicateTokenEx"));
            }
            let _primary_token = HandleGuard(primary_token);

            if SetTokenInformation(
                primary_token,
                TokenSessionId,
                &session_id as *const u32 as *const _,
                mem::size_of::<u32>() as u32,
            ) == 0
            {
                return Err(last_error("SetTokenInformation(TokenSessionId)"));
            }

            let exe =
                env::current_exe().map_err(|error| format!("resolve helper exe path: {error}"))?;
            let current_dir = exe
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));

            let default_error = match spawn_worker_on_desktop(
                primary_token,
                &exe,
                &current_dir,
                "WinSta0\\Default",
            ) {
                Ok(handle) => return Ok(handle),
                Err(error) => {
                    log_service_error("start session input worker on WinSta0\\Default", &error);
                    error
                }
            };
            match spawn_worker_on_desktop(primary_token, &exe, &current_dir, "WinSta0\\Winlogon") {
                Ok(handle) => Ok(handle),
                Err(error) => {
                    log_service_error("start session input worker on WinSta0\\Winlogon", &error);
                    Err(format!(
                        "worker launch failed on Default ({default_error}) and Winlogon ({error})"
                    ))
                }
            }
        }
    }

    unsafe fn spawn_worker_on_desktop(
        primary_token: HANDLE,
        exe: &PathBuf,
        current_dir: &PathBuf,
        desktop: &str,
    ) -> Result<HANDLE, String> {
        let command = format!("\"{}\" --worker", exe.display());
        let mut command_w = wide_null(&command);
        let mut desktop_w = wide_null(desktop);
        let current_dir_w = wide_null(&current_dir.to_string_lossy());
        let mut startup_info = STARTUPINFOW {
            cb: mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: desktop_w.as_mut_ptr(),
            ..Default::default()
        };
        let mut process_info = PROCESS_INFORMATION::default();
        if CreateProcessAsUserW(
            primary_token,
            ptr::null(),
            command_w.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            ptr::null(),
            current_dir_w.as_ptr(),
            &mut startup_info,
            &mut process_info,
        ) == 0
        {
            let code = GetLastError();
            return Err(format!(
                "CreateProcessAsUserW({desktop}) failed with Windows error {code}"
            ));
        }

        if !process_info.hThread.is_null() {
            let _ = CloseHandle(process_info.hThread);
        }
        Ok(process_info.hProcess)
    }

    fn run_worker() -> Result<(), String> {
        let session_id = current_session_id()?;
        let mut desktop = DesktopAttachment::new();
        let mut pressed_keys = Vec::new();
        let mut button_mask = 0_u64;

        loop {
            let pipe = create_input_pipe(session_id)?;
            let connected = connect_pipe(pipe);
            if !connected {
                unsafe {
                    let _ = CloseHandle(pipe);
                }
                continue;
            }

            loop {
                let payload = match read_framed_message(pipe) {
                    Ok(Some(payload)) => payload,
                    Ok(None) => break,
                    Err(error) => {
                        // A bad length prefix means the byte stream is desynced.
                        // Drop this connection and recreate the pipe instead of
                        // returning the error — that used to propagate out of
                        // run_worker and kill the worker process, blanking
                        // lock-screen input until the service restarted it.
                        write_worker_desktop_status(session_id, Err(error.as_str()));
                        break;
                    }
                };
                let command = match decode_input_command(&payload) {
                    Ok(command) => command,
                    Err(error) => {
                        // The frame length was valid, so the stream stays aligned:
                        // skip just this command (e.g. one a newer app build sent
                        // that this helper binary doesn't understand) and keep the
                        // connection alive rather than killing the worker.
                        write_worker_desktop_status(session_id, Err(error.as_str()));
                        continue;
                    }
                };
                inject_worker_command(
                    session_id,
                    &command,
                    &mut desktop,
                    &mut pressed_keys,
                    &mut button_mask,
                );
            }

            let release_result = windows_input::release_pressed_inputs_on_fresh_input_desktop(
                &mut pressed_keys,
                &mut button_mask,
            );
            write_worker_desktop_status(
                session_id,
                release_result
                    .as_ref()
                    .map(String::as_str)
                    .map_err(String::as_str),
            );
            unsafe {
                let _ = DisconnectNamedPipe(pipe);
                let _ = CloseHandle(pipe);
            }
        }
    }

    fn inject_worker_command(
        session_id: u32,
        command: &InputCommand,
        desktop: &mut DesktopAttachment,
        pressed_keys: &mut Vec<u16>,
        button_mask: &mut u64,
    ) {
        // Every command is injected on the worker's main thread against the
        // cached DesktopAttachment. attach_current_input_desktop() caches by
        // desktop name, so on a stable desktop this is cheap.
        let desktop_result = desktop.attach_current_input_desktop();
        write_worker_desktop_status(
            session_id,
            desktop_result
                .as_ref()
                .map(String::as_str)
                .map_err(String::as_str),
        );
        if !super::worker_can_inject(desktop_result.is_ok(), command) {
            return;
        }

        match command {
            InputCommand::ReleaseAll => {
                windows_input::release_pressed_inputs(pressed_keys, button_mask);
            }
            _ => {
                windows_input::inject_command(command, pressed_keys, button_mask);
            }
        }
    }

    fn write_worker_desktop_status(session_id: u32, desktop_result: Result<&str, &str>) {
        static LAST_STATUS: std::sync::Mutex<Option<(std::time::Instant, String, String)>> =
            std::sync::Mutex::new(None);
        let (desktop, error) = match desktop_result {
            Ok(name) => (name, ""),
            Err(error) => ("", error),
        };
        let Ok(mut previous) = LAST_STATUS.lock() else {
            return;
        };
        let now = std::time::Instant::now();
        if !super::worker_status_due(previous.as_ref(), now, desktop, error) {
            return;
        }
        let path = input_helper_status_path(session_id);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body = format!("{}\n{}\n{}\n", now_ms(), desktop, error);
        if fs::write(path, body).is_ok() {
            *previous = Some((now, desktop.into(), error.into()));
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    fn log_service_error(context: &str, error: &str) {
        let base = env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        let directory = base.join("MyKVM");
        if fs::create_dir_all(&directory).is_err() {
            return;
        }
        let path = directory.join("service.log");
        if fs::metadata(&path)
            .map(|metadata| metadata.len() > 1024 * 1024)
            .unwrap_or(false)
        {
            let _ = fs::write(&path, []);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{} {context}: {error}", now_ms());
        }
    }

    fn create_input_pipe(session_id: u32) -> Result<HANDLE, String> {
        let pipe_name = input_pipe_name(session_id);
        let pipe_name_w = wide_null(&pipe_name);
        let security = PipeSecurity::for_session(session_id)?;
        let pipe = unsafe {
            CreateNamedPipeW(
                pipe_name_w.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0,
                &security.attributes,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            return Err(last_error("CreateNamedPipeW"));
        }
        Ok(pipe)
    }

    fn connect_pipe(pipe: HANDLE) -> bool {
        let ok = unsafe { ConnectNamedPipe(pipe, ptr::null_mut()) } != 0;
        ok || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED
    }

    fn read_framed_message(pipe: HANDLE) -> Result<Option<Vec<u8>>, String> {
        let mut len_bytes = [0_u8; 4];
        if !read_exact_pipe(pipe, &mut len_bytes)? {
            return Ok(None);
        }
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len == 0 || len > PIPE_BUFFER_SIZE as usize {
            return Err(format!("invalid input command length: {len}"));
        }

        let mut payload = vec![0_u8; len];
        if !read_exact_pipe(pipe, &mut payload)? {
            return Ok(None);
        }
        Ok(Some(payload))
    }

    fn read_exact_pipe(pipe: HANDLE, buffer: &mut [u8]) -> Result<bool, String> {
        let mut offset = 0_usize;
        while offset < buffer.len() {
            let mut read = 0_u32;
            let ok = unsafe {
                ReadFile(
                    pipe,
                    buffer[offset..].as_mut_ptr(),
                    (buffer.len() - offset) as u32,
                    &mut read,
                    ptr::null_mut(),
                )
            } != 0;
            if !ok || read == 0 {
                return Ok(false);
            }
            offset += read as usize;
        }
        Ok(true)
    }

    struct PipeSecurity {
        attributes: SECURITY_ATTRIBUTES,
        security_descriptor: PSECURITY_DESCRIPTOR,
    }

    impl PipeSecurity {
        fn for_session(session_id: u32) -> Result<Self, String> {
            let user_sid = session_user_sid(session_id).unwrap_or_else(|| "IU".into());
            let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})");
            Self::from_sddl(&sddl)
        }

        fn for_owner_sid(owner_sid: &str) -> Result<Self, String> {
            if owner_sid.is_empty() {
                return Err("service owner SID is empty".into());
            }
            let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})");
            Self::from_sddl(&sddl)
        }

        fn from_sddl(sddl: &str) -> Result<Self, String> {
            let sddl_w = wide_null(&sddl);
            let mut security_descriptor = ptr::null_mut();
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl_w.as_ptr(),
                    SDDL_REVISION_1,
                    &mut security_descriptor,
                    ptr::null_mut(),
                )
            } != 0;
            if !ok {
                return Err(last_error(
                    "ConvertStringSecurityDescriptorToSecurityDescriptorW",
                ));
            }

            let attributes = SECURITY_ATTRIBUTES {
                nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: security_descriptor,
                bInheritHandle: 0,
            };
            Ok(Self {
                attributes,
                security_descriptor,
            })
        }
    }

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            if !self.security_descriptor.is_null() {
                unsafe {
                    let _ = LocalFree(self.security_descriptor as HLOCAL);
                }
            }
        }
    }

    fn session_user_sid(session_id: u32) -> Option<String> {
        unsafe {
            let mut token = ptr::null_mut();
            if WTSQueryUserToken(session_id, &mut token) == 0 {
                return None;
            }
            let _token = HandleGuard(token);

            let mut needed = 0_u32;
            let _ = GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
            if needed == 0 {
                return None;
            }
            let mut buffer = vec![0_u8; needed as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut _,
                needed,
                &mut needed,
            ) == 0
            {
                return None;
            }

            let token_user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut sid_w = ptr::null_mut();
            if ConvertSidToStringSidW(token_user.User.Sid, &mut sid_w) == 0 {
                return None;
            }
            let sid = wide_ptr_to_string(sid_w);
            let _ = LocalFree(sid_w as HLOCAL);
            Some(sid)
        }
    }

    fn current_session_id() -> Result<u32, String> {
        let mut session_id = 0_u32;
        let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) } != 0;
        if ok {
            Ok(session_id)
        } else {
            Err(last_error("ProcessIdToSessionId"))
        }
    }

    struct HandleGuard(HANDLE);

    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    fn last_error(context: &str) -> String {
        let code = unsafe { GetLastError() };
        format!("{context} failed with Windows error {code}")
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe fn wide_ptr_to_string(ptr: *const u16) -> String {
        if ptr.is_null() {
            return String::new();
        }
        let mut len = 0_usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_service_config_and_owner_sid() {
            let args = [
                SERVICE_CONFIG_PATH_ARG,
                r"C:\Users\client\AppData\Roaming\com.xzhpl.mykvm\layout.json",
                SERVICE_OWNER_SID_ARG,
                "S-1-5-21-1-2-3-1001",
            ]
            .map(OsString::from)
            .into_iter();

            let parsed = ServiceArgs::parse(args).expect("parse service arguments");
            assert_eq!(
                parsed.config_path,
                Some(PathBuf::from(
                    r"C:\Users\client\AppData\Roaming\com.xzhpl.mykvm\layout.json"
                ))
            );
            assert_eq!(parsed.owner_sid.as_deref(), Some("S-1-5-21-1-2-3-1001"));
        }

        #[test]
        fn accepts_legacy_worker_only_service_arguments() {
            let parsed =
                ServiceArgs::parse(std::iter::empty()).expect("parse legacy service arguments");
            assert!(parsed.config_path.is_none());
            assert!(parsed.owner_sid.is_none());
        }

        #[test]
        fn rejects_partial_headless_service_arguments() {
            let args = [
                SERVICE_CONFIG_PATH_ARG,
                r"C:\Users\client\AppData\Roaming\com.xzhpl.mykvm\layout.json",
            ]
            .map(OsString::from)
            .into_iter();
            assert_eq!(
                ServiceArgs::parse(args).unwrap_err(),
                format!("missing {SERVICE_OWNER_SID_ARG}")
            );
        }
    }
}
