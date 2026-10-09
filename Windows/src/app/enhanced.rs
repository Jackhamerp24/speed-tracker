//! The app's end of the optional network collector. The app itself never elevates: it starts the
//! collector through a Windows approval prompt and listens on a one-way pipe that only this user
//! and administrators can open, checking that the process on the other end is the one it launched.

use super::ipc::Batch;
use super::win::{user_sid, wide};
use speedtracker::domain::{FlowSample, FlowSampler};
use std::ffi::c_void;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::windows::io::FromRawHandle;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_CANCELLED, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_LISTENING, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    SetNamedPipeHandleState, PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetProcessId, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

const OFF: &str = "Standard-user log detection. Enhanced network collection is off.";

struct State {
    samples: Vec<FlowSample>,
    received: Option<Instant>,
    available: bool,
    status: String,
    // The pipe the current collector writes to, as an integer so the state can cross threads.
    pipe: Option<isize>,
}

pub struct EnhancedSampler {
    state: Mutex<State>,
}

impl FlowSampler for EnhancedSampler {
    fn available(&self) -> bool {
        self.state.lock().unwrap().available
    }
    fn status(&self) -> String {
        self.state.lock().unwrap().status.clone()
    }
    fn sample(&self) -> Vec<FlowSample> {
        let state = self.state.lock().unwrap();
        // Samples older than a few seconds describe a collector that has stopped talking.
        if state
            .received
            .is_some_and(|received| received.elapsed() < Duration::from_secs(3))
        {
            state.samples.clone()
        } else {
            Vec::new()
        }
    }
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

impl EnhancedSampler {
    pub fn new() -> Arc<EnhancedSampler> {
        Arc::new(EnhancedSampler {
            state: Mutex::new(State {
                samples: Vec::new(),
                received: None,
                available: false,
                status: OFF.into(),
                pipe: None,
            }),
        })
    }

    fn set_status(&self, status: &str) {
        self.state.lock().unwrap().status = status.into();
    }

    /// Asks Windows to start the collector elevated and waits for it to connect. Blocks while the
    /// approval prompt is open, so it is called off the main thread.
    pub fn enable(self: &Arc<Self>) -> Result<(), String> {
        self.disable();
        let name = format!("SpeedTracker.Collector.{}", uuid::Uuid::new_v4().simple());
        let user = user_sid().ok_or("Windows user identity is unavailable.")?;
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        unsafe {
            // This user gets full control. Credential-based UAC can run the helper as a different
            // administrator account, so administrators may read and write. Nobody else gets in.
            let mut descriptor: *mut c_void = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(&format!("D:P(A;;GA;;;{user})(A;;GRGW;;;BA)")).as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err("The collector pipe's access list could not be built.".into());
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            // Non-blocking while waiting for the connection, so the wait can time out.
            let pipe = CreateNamedPipeW(
                wide(&format!(r"\\.\pipe\{name}")).as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
                1,
                0,
                64 * 1024,
                0,
                &attributes,
            );
            LocalFree(descriptor);
            if pipe == INVALID_HANDLE_VALUE {
                return Err("The collector pipe could not be created.".into());
            }
            let pipe = Handle(pipe);
            self.set_status("Waiting for permission to enable passive network counters…");

            let (verb, file, parameters) = (
                wide("runas"),
                wide(&executable.to_string_lossy()),
                wide(&format!("--collector {name} {}", GetCurrentProcessId())),
            );
            let mut launch: SHELLEXECUTEINFOW = std::mem::zeroed();
            launch.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            launch.fMask = SEE_MASK_NOCLOSEPROCESS;
            launch.lpVerb = verb.as_ptr();
            launch.lpFile = file.as_ptr();
            launch.lpParameters = parameters.as_ptr();
            if ShellExecuteExW(&mut launch) == 0 || launch.hProcess.is_null() {
                self.set_status(OFF);
                return Err(if GetLastError() == ERROR_CANCELLED {
                    "Administrator approval was declined.".into()
                } else {
                    "Network collector did not start.".to_string()
                });
            }
            let helper = Handle(launch.hProcess);
            let helper_pid = GetProcessId(helper.0);

            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if ConnectNamedPipe(pipe.0, std::ptr::null_mut()) != 0 {
                    break;
                }
                match GetLastError() {
                    ERROR_PIPE_CONNECTED | ERROR_NO_DATA => break,
                    ERROR_PIPE_LISTENING
                        if Instant::now() < deadline
                            && WaitForSingleObject(helper.0, 0) == WAIT_TIMEOUT =>
                    {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    _ => {
                        self.set_status(OFF);
                        return Err("Network collector did not connect.".into());
                    }
                }
            }
            let mut client = 0u32;
            if GetNamedPipeClientProcessId(pipe.0, &mut client) == 0 || client != helper_pid {
                self.set_status(OFF);
                return Err(
                    "Network collector identity did not match the process that was launched."
                        .into(),
                );
            }
            SetNamedPipeHandleState(
                pipe.0,
                &(PIPE_READMODE_BYTE | PIPE_WAIT),
                std::ptr::null(),
                std::ptr::null(),
            );

            let raw = pipe.0 as isize;
            std::mem::forget(pipe);
            {
                let mut state = self.state.lock().unwrap();
                state.pipe = Some(raw);
                state.available = true;
                state.status =
                    "Enhanced passive TCP collection enabled; traffic metrics are estimates."
                        .into();
            }
            let sampler = Arc::clone(self);
            let reader = File::from_raw_handle(raw as HANDLE);
            std::thread::spawn(move || sampler.read(reader, raw));
        }
        Ok(())
    }

    fn read(&self, pipe: File, raw: isize) {
        for line in BufReader::new(&pipe).lines() {
            let Ok(line) = line else { break };
            let Ok(batch) = serde_json::from_str::<Batch>(&line) else {
                continue;
            };
            let mut state = self.state.lock().unwrap();
            if state.pipe != Some(raw) {
                break;
            }
            state.samples = batch.samples;
            state.received = Some(Instant::now());
            state.status = batch.status;
        }
        // Forget the handle under the lock before closing it, so `disable` never touches a closed one.
        let mut state = self.state.lock().unwrap();
        if state.pipe == Some(raw) {
            state.pipe = None;
            state.available = false;
            state.samples.clear();
            state.status = "Enhanced collector disconnected; log detection continues.".into();
        }
        drop(state);
        drop(pipe);
    }

    /// Stops the collector by closing its pipe; it exits when its next write fails.
    pub fn disable(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(pipe) = state.pipe.take() {
            unsafe { DisconnectNamedPipe(pipe as HANDLE) };
        }
        state.samples.clear();
        state.received = None;
        state.available = false;
        state.status = OFF.into();
    }
}
