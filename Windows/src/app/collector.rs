//! The optional elevated collector: this same program started with `--collector`, after the user
//! approves it. It reads per-connection TCP byte counters, which Windows only gives administrators,
//! and writes them to the unprivileged app over a pipe. No payload capture, no process environment.

use super::ipc::Batch;
use super::win::wide;
use speedtracker::domain::FlowSample;
use speedtracker::{harness, processes};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::os::windows::io::FromRawHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};
use windows_sys::Win32::UI::Shell::IsUserAnAdmin;

#[link(name = "iphlpapi")]
extern "system" {
    fn GetExtendedTcpTable(
        table: *mut c_void,
        size: *mut u32,
        order: i32,
        family: u32,
        class: i32,
        reserved: u32,
    ) -> u32;
    fn SetPerTcpConnectionEStats(
        row: *const c_void,
        kind: i32,
        rw: *const u8,
        version: u32,
        size: u32,
        offset: u32,
    ) -> u32;
    fn SetPerTcp6ConnectionEStats(
        row: *const c_void,
        kind: i32,
        rw: *const u8,
        version: u32,
        size: u32,
        offset: u32,
    ) -> u32;
    fn GetPerTcpConnectionEStats(
        row: *const c_void,
        kind: i32,
        rw: *mut u8,
        rw_version: u32,
        rw_size: u32,
        ros: *mut u8,
        ros_version: u32,
        ros_size: u32,
        rod: *mut u8,
        rod_version: u32,
        rod_size: u32,
    ) -> u32;
    fn GetPerTcp6ConnectionEStats(
        row: *const c_void,
        kind: i32,
        rw: *mut u8,
        rw_version: u32,
        rw_size: u32,
        ros: *mut u8,
        ros_version: u32,
        ros_size: u32,
        rod: *mut u8,
        rod_version: u32,
        rod_size: u32,
    ) -> u32;
}

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;
const TCP_TABLE_OWNER_PID_ALL: i32 = 5;
const TCP_ESTATS_DATA: i32 = 1;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const STATE_ESTABLISHED: u32 = 5;

// TCP_ESTATS_DATA_ROD_v0.
#[repr(C)]
#[derive(Default)]
struct DataCounters {
    data_bytes_out: u64,
    data_segs_out: u64,
    data_bytes_in: u64,
    data_segs_in: u64,
    segs_out: u64,
    segs_in: u64,
    soft_errors: u32,
    soft_error_reason: u32,
    snd_una: u32,
    snd_nxt: u32,
    snd_max: u32,
    thru_bytes_acked: u64,
    rcv_nxt: u32,
    thru_bytes_received: u64,
}

struct Process {
    name: String,
    harness: Option<&'static str>,
    // A desktop app's own process: never sampled, even though it talks to a model provider.
    host_app: bool,
    started: u64,
}

struct Sampler {
    processes: HashMap<u32, Process>,
    // Address -> the provider host it was resolved from.
    providers: HashMap<String, String>,
    // Connections whose counters have been switched on.
    enabled: HashSet<String>,
    next_processes: Instant,
    next_dns: Instant,
    status: String,
}

fn ignored(name: &str) -> bool {
    let name = name.to_lowercase();
    [
        "speedtracker",
        "chrome",
        "msedge",
        "firefox",
        "brave",
        "opera",
        "claude helper",
        "chatgpt",
        "codex desktop",
        "svchost",
        "system",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

// Ports arrive in network byte order in the low sixteen bits.
fn port(raw: u32) -> u16 {
    (((raw & 255) << 8) | ((raw >> 8) & 255)) as u16
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

impl Sampler {
    fn new() -> Sampler {
        let now = Instant::now();
        Sampler {
            processes: HashMap::new(),
            providers: HashMap::new(),
            enabled: HashSet::new(),
            next_processes: now,
            next_dns: now,
            status: "Passive TCP byte counters".into(),
        }
    }

    fn sample(&mut self) -> Vec<FlowSample> {
        self.refresh_processes();
        self.refresh_providers();
        let mut samples = Vec::new();
        let mut present = HashSet::new();
        for family in [AF_INET, AF_INET6] {
            let v4 = family == AF_INET;
            let mut size = 0u32;
            let result = unsafe {
                GetExtendedTcpTable(
                    std::ptr::null_mut(),
                    &mut size,
                    0,
                    family,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                )
            };
            if result != ERROR_INSUFFICIENT_BUFFER || size > 32 * 1024 * 1024 {
                continue;
            }
            // Four-byte units keep the table aligned.
            let mut table = vec![0u32; (size as usize).div_ceil(4)];
            let result = unsafe {
                GetExtendedTcpTable(
                    table.as_mut_ptr().cast(),
                    &mut size,
                    0,
                    family,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                )
            };
            if result != 0 {
                self.status = format!("TCP endpoint discovery unavailable ({result})");
                continue;
            }
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(table.as_ptr().cast(), table.len() * 4) };
            let count = read_u32(bytes, 0) as usize;
            // MIB_TCPROW_OWNER_PID is 24 bytes; MIB_TCP6ROW_OWNER_PID is 56.
            let row_size = if v4 { 24 } else { 56 };
            if 4 + count * row_size > (size as usize).min(bytes.len()) {
                continue;
            }
            for index in 0..count {
                let row = &bytes[4 + index * row_size..4 + (index + 1) * row_size];
                let state = read_u32(row, if v4 { 0 } else { 48 });
                let pid = read_u32(row, if v4 { 20 } else { 52 });
                let Some(process) = self
                    .processes
                    .get(&pid)
                    .filter(|_| state == STATE_ESTABLISHED)
                else {
                    continue;
                };
                let remote = if v4 {
                    IpAddr::V4(Ipv4Addr::new(row[12], row[13], row[14], row[15]))
                } else {
                    IpAddr::V6(Ipv6Addr::from(
                        <[u8; 16]>::try_from(&row[24..40]).expect("sixteen bytes"),
                    ))
                };
                let link_local = matches!(remote, IpAddr::V6(address) if address.segments()[0] & 0xffc0 == 0xfe80);
                if remote.is_loopback() || link_local {
                    continue;
                }
                let remote_port = port(read_u32(row, if v4 { 16 } else { 44 }));
                let local_port = port(read_u32(row, if v4 { 8 } else { 20 }));
                let address = remote.to_string();
                let provider = self.providers.get(&address);
                // Either a known harness, or any process talking to a known model provider.
                if (process.harness.is_none() && provider.is_none())
                    || process.host_app
                    || ignored(&process.name)
                {
                    continue;
                }
                let key = format!(
                    "{pid}:{}:{family}:{local_port}:{address}:{remote_port}",
                    process.started
                );
                present.insert(key.clone());
                // MIB_TCPROW matches the owner row's prefix; MIB_TCP6ROW puts State first.
                let mut tcp_row = [0u32; 13];
                let tcp_bytes: &mut [u8] =
                    unsafe { std::slice::from_raw_parts_mut(tcp_row.as_mut_ptr().cast(), 52) };
                if v4 {
                    tcp_bytes[..20].copy_from_slice(&row[..20]);
                } else {
                    tcp_bytes[..4].copy_from_slice(&state.to_ne_bytes());
                    tcp_bytes[4..52].copy_from_slice(&row[..48]);
                }
                let tcp_row = tcp_row.as_ptr().cast::<c_void>();
                if !self.enabled.contains(&key) {
                    let enable = 1u8;
                    let error = unsafe {
                        if v4 {
                            SetPerTcpConnectionEStats(tcp_row, TCP_ESTATS_DATA, &enable, 0, 1, 0)
                        } else {
                            SetPerTcp6ConnectionEStats(tcp_row, TCP_ESTATS_DATA, &enable, 0, 1, 0)
                        }
                    };
                    if error != 0 {
                        self.status =
                            format!("TCP counters unavailable ({error}); log detection continues");
                        continue;
                    }
                    self.enabled.insert(key.clone());
                }
                let mut active = 0u8;
                let mut counters = DataCounters::default();
                let size = std::mem::size_of::<DataCounters>() as u32;
                let rod = (&mut counters as *mut DataCounters).cast::<u8>();
                let code = unsafe {
                    if v4 {
                        GetPerTcpConnectionEStats(
                            tcp_row,
                            TCP_ESTATS_DATA,
                            &mut active,
                            0,
                            1,
                            std::ptr::null_mut(),
                            0,
                            0,
                            rod,
                            0,
                            size,
                        )
                    } else {
                        GetPerTcp6ConnectionEStats(
                            tcp_row,
                            TCP_ESTATS_DATA,
                            &mut active,
                            0,
                            1,
                            std::ptr::null_mut(),
                            0,
                            0,
                            rod,
                            0,
                            size,
                        )
                    }
                };
                if code == 0 && active != 0 {
                    samples.push(FlowSample {
                        pid,
                        harness: process
                            .harness
                            .map_or_else(|| process.name.clone(), str::to_string),
                        host: provider.cloned().unwrap_or(address),
                        received: counters.data_bytes_in,
                        sent: counters.data_bytes_out,
                        connection_id: Some(key),
                    });
                }
            }
        }
        self.enabled.retain(|key| present.contains(key));
        samples
    }

    fn refresh_processes(&mut self) {
        if Instant::now() < self.next_processes {
            return;
        }
        self.next_processes = Instant::now() + Duration::from_secs(3);
        let mut present = HashSet::new();
        for process in processes::list() {
            present.insert(process.pid);
            let Some(started) = processes::start_time(process.pid) else {
                continue;
            };
            if self
                .processes
                .get(&process.pid)
                .is_some_and(|cached| cached.started == started)
            {
                continue;
            }
            let executable =
                processes::image_path(process.pid).unwrap_or_else(|| process.name.clone());
            let mut harness = harness::classify(&executable, None);
            // A script runtime is only a harness if the script it runs is one.
            if matches!(
                process.name.as_str(),
                "node" | "bun" | "python" | "python3" | "deno"
            ) {
                harness = harness::classify(
                    &process.name,
                    processes::command_line(process.pid).as_deref(),
                );
            }
            self.processes.insert(
                process.pid,
                Process {
                    name: process.name,
                    harness,
                    host_app: harness::is_host_app(&executable),
                    started,
                },
            );
        }
        self.processes.retain(|pid, _| present.contains(pid));
    }

    fn refresh_providers(&mut self) {
        if Instant::now() < self.next_dns {
            return;
        }
        self.next_dns = Instant::now() + Duration::from_secs(300);
        for host in [
            "api.anthropic.com",
            "api.openai.com",
            "chatgpt.com",
            "api.deepseek.com",
            "openrouter.ai",
            "api.x.ai",
            "api.groq.com",
            "api.cerebras.ai",
            "api.mistral.ai",
            "api.moonshot.ai",
            "api.z.ai",
        ] {
            // These are public endpoint DNS lookups, not requests to the model APIs.
            if let Ok(addresses) = (host, 443).to_socket_addrs() {
                for address in addresses {
                    self.providers
                        .insert(address.ip().to_string(), host.to_string());
                }
            }
        }
    }
}

/// `--collector <pipe name> <parent pid>`. The exit code says why it stopped; nothing is shown.
pub fn run(arguments: &[String]) -> i32 {
    let [name, parent] = arguments else { return 2 };
    let valid_name = name
        .strip_prefix("SpeedTracker.Collector.")
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let Some(parent_pid) = parent.parse::<u32>().ok().filter(|_| valid_name) else {
        return 2;
    };
    unsafe {
        if IsUserAnAdmin() == 0 {
            return 3;
        }
        let parent: HANDLE = OpenProcess(PROCESS_SYNCHRONIZE, 0, parent_pid);
        if parent.is_null() {
            return 1;
        }
        let path = wide(&format!(r"\\.\pipe\{name}"));
        let deadline = Instant::now() + Duration::from_secs(15);
        let pipe = loop {
            let pipe = CreateFileW(
                path.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if pipe != INVALID_HANDLE_VALUE {
                break pipe;
            }
            if Instant::now() >= deadline {
                CloseHandle(parent);
                return 1;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let mut pipe = File::from_raw_handle(pipe);
        // Only the app that launched this helper may receive its samples.
        let mut server = 0u32;
        if GetNamedPipeServerProcessId(
            std::os::windows::io::AsRawHandle::as_raw_handle(&pipe),
            &mut server,
        ) == 0
            || server != parent_pid
        {
            CloseHandle(parent);
            return 4;
        }
        let mut sampler = Sampler::new();
        while WaitForSingleObject(parent, 0) == WAIT_TIMEOUT {
            let samples = sampler.sample();
            let Ok(mut line) = serde_json::to_vec(&Batch {
                status: sampler.status.clone(),
                samples,
            }) else {
                break;
            };
            line.push(b'\n');
            // The app closing its end is how it switches the collector off.
            if pipe.write_all(&line).and_then(|_| pipe.flush()).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        CloseHandle(parent);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{ignored, port, run};

    #[test]
    fn ports_are_read_from_network_byte_order() {
        // 443 is 0x01BB: on the wire 01 BB, which a little-endian read of the field sees as 0xBB01.
        assert_eq!(port(0xBB01), 443);
        assert_eq!(port(0x5000), 80);
        assert_eq!(port(0xFFFF_BB01), 443, "the unused upper half of the field is ignored");
    }

    #[test]
    fn browsers_and_host_apps_are_never_sampled() {
        for name in ["chrome", "msedge", "Claude Helper (Renderer)", "ChatGPT", "SpeedTracker", "svchost"] {
            assert!(ignored(name), "{name}");
        }
        for name in ["claude", "node", "codex", "opencode"] {
            assert!(!ignored(name), "{name}");
        }
    }

    #[test]
    fn the_collector_refuses_to_start_without_a_well_formed_pipe_name_and_parent() {
        let pipe = format!("SpeedTracker.Collector.{}", "0123456789abcdef0123456789abcdef");
        let arguments = |values: &[&str]| values.iter().map(|value| value.to_string()).collect::<Vec<_>>();
        assert_eq!(run(&arguments(&[])), 2);
        assert_eq!(run(&arguments(&[&pipe])), 2, "the parent process id is required");
        assert_eq!(run(&arguments(&[&pipe, "not-a-number"])), 2);
        assert_eq!(run(&arguments(&["SomeOther.Pipe", "1234"])), 2, "only this app's collector pipes are accepted");
        assert_eq!(run(&arguments(&["SpeedTracker.Collector.short", "1234"])), 2);
        assert_eq!(run(&arguments(&["SpeedTracker.Collector.0123456789abcdef0123456789abcdeg", "1234"])), 2, "the id must be hexadecimal");
    }
}
