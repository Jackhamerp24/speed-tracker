//! The always-running part: the notification-area icon, the tracker, the optional proxy and
//! collector, and the Live and Dashboard windows, which run as short-lived child processes so that
//! nothing graphical stays in memory while they are closed.

use super::enhanced::EnhancedSampler;
use super::ipc::{Snapshot, ToTray, ToWindow};
use super::win::{message_box, wide, work_area_at_cursor, SingleInstance};
use speedtracker::domain::FlowSampler;
use speedtracker::history::HistoryStore;
use speedtracker::proxy::ProxyService;
use speedtracker::tracker::Tracker;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, KillTimer, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, TrackPopupMenu, TranslateMessage, HICON, ICONINFO, MF_SEPARATOR,
    MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSW,
};

const WM_TRAY: u32 = WM_APP + 1;
const WM_UPDATE: u32 = WM_APP + 2;
const WM_OPEN_DASHBOARD: u32 = WM_APP + 3;
const CLICK_TIMER: usize = 1;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Live,
    Dashboard,
}

struct WindowProcess {
    pid: u32,
    input: ChildStdin,
}

impl WindowProcess {
    fn send(&mut self, message: &ToWindow) {
        if let Ok(mut line) = serde_json::to_vec(message) {
            line.push(b'\n');
            // A window that has gone away is noticed by its reader thread; nothing to do here.
            let _ = self.input.write_all(&line).and_then(|_| self.input.flush());
        }
    }
}

#[derive(Default)]
struct Windows {
    live: Option<WindowProcess>,
    dashboard: Option<WindowProcess>,
    live_closed: Option<Instant>,
}

struct App {
    tracker: Arc<Tracker>,
    enhanced: Arc<EnhancedSampler>,
    windows: Mutex<Windows>,
    broadcast: Sender<()>,
    hwnd: AtomicIsize,
    idle_icon: isize,
    active_icon: isize,
    taskbar_created: u32,
    update_pending: AtomicBool,
    enhanced_pending: AtomicBool,
    history_revision: AtomicU64,
    notice: Mutex<Option<String>>,
    // What the icon currently shows, to avoid redundant shell calls.
    shown: Mutex<(bool, String)>,
    // The second button-up of a double-click must not also count as a click.
    double_clicked: AtomicBool,
    exiting: AtomicBool,
}

static APP: OnceLock<App> = OnceLock::new();

fn app() -> &'static App {
    APP.get()
        .expect("tray state is set before the message loop runs")
}

impl App {
    fn hwnd(&self) -> HWND {
        self.hwnd.load(Ordering::Relaxed) as HWND
    }

    fn snapshot(&self) -> Snapshot {
        let tracker = &self.tracker;
        Snapshot {
            active: tracker.active(),
            harnesses: tracker.harnesses(),
            held_record: tracker.held_record().map(|record| (*record).clone()),
            recent: tracker
                .recent(5, tracker.target().as_deref())
                .iter()
                .map(|record| (**record).clone())
                .collect(),
            held_rate: tracker.held_rate(),
            held_ttft: tracker.held_ttft(),
            held_rate_estimated: tracker.held_rate_estimated(),
            held_ttft_estimated: tracker.held_ttft_estimated(),
            target: tracker.target(),
            network_status: tracker.network_status(),
            enhanced_available: tracker.enhanced_network_available(),
            enhanced_enabled: tracker.enhanced_network_enabled(),
            enhanced_pending: self.enhanced_pending.load(Ordering::Relaxed),
            history_revision: self.history_revision.load(Ordering::Relaxed),
            notice: self.notice.lock().unwrap().take(),
        }
    }

    fn notify(&self) {
        let _ = self.broadcast.send(());
    }

    fn held_ttft(&self) -> String {
        match self.tracker.held_ttft() {
            Some(seconds) => format!(
                "{}{seconds:.2} s",
                if self.tracker.held_ttft_estimated() {
                    "~"
                } else {
                    ""
                }
            ),
            None => "—".into(),
        }
    }

    // One line of what Live would show: the calls in flight and the held figures. Timings only.
    fn trace_line(&self) -> String {
        let calls: Vec<String> = self
            .tracker
            .active()
            .iter()
            .map(|call| {
                format!(
                    "{}/{} rate={} ttft={}",
                    call.harness,
                    call.phase,
                    format_rate(call.rate, call.estimated),
                    call.ttft
                        .map_or("—".into(), |seconds| format!("{seconds:.2}"))
                )
            })
            .collect();
        format!(
            "active=[{}] held={} ttft={} recorded={}",
            calls.join("; "),
            format_rate(self.tracker.held_rate(), self.tracker.held_rate_estimated()),
            self.held_ttft(),
            self.history_revision.load(Ordering::Relaxed)
        )
    }

    fn slot(windows: &mut Windows, kind: Kind) -> &mut Option<WindowProcess> {
        match kind {
            Kind::Live => &mut windows.live,
            Kind::Dashboard => &mut windows.dashboard,
        }
    }

    // Shows a window, starting its process if it is not already open.
    fn open(&'static self, kind: Kind, tab: &str) {
        if self.exiting.load(Ordering::Relaxed) {
            return;
        }
        let mut windows = self.windows.lock().unwrap();
        if let Some(window) = Self::slot(&mut windows, kind) {
            // This process received the click, so it may hand the foreground to the window.
            unsafe { AllowSetForegroundWindow(window.pid) };
            window.send(&ToWindow::ShowTab(tab.into()));
            window.send(&ToWindow::Focus);
            return;
        }
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        let mut command = Command::new(executable);
        command.args([
            "--ui",
            if kind == Kind::Live {
                "live"
            } else {
                "dashboard"
            },
            "--tab",
            tab,
        ]);
        if let (Kind::Live, Some([left, top, right, bottom])) = (kind, work_area_at_cursor()) {
            command.args(["--work", &format!("{left},{top},{right},{bottom}")]);
        }
        // Extra window arguments for checking a change, such as `--screenshot|out.png`.
        if let Ok(extra) = std::env::var("SPEEDTRACKER_WINDOW_ARGS") {
            command.args(extra.split('|').filter(|argument| !argument.is_empty()));
        }
        let Ok(mut child) = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        else {
            return;
        };
        let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            return;
        };
        let pid = child.id();
        unsafe { AllowSetForegroundWindow(pid) };
        let mut window = WindowProcess { pid, input };
        window.send(&ToWindow::Snapshot(Box::new(self.snapshot())));
        *Self::slot(&mut windows, kind) = Some(window);
        drop(windows);
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if let Ok(message) = serde_json::from_str::<ToTray>(&line) {
                    self.handle(message);
                }
            }
            let _ = child.wait();
            let mut windows = self.windows.lock().unwrap();
            let slot = Self::slot(&mut windows, kind);
            if slot.as_ref().is_some_and(|window| window.pid == pid) {
                *slot = None;
                if kind == Kind::Live {
                    windows.live_closed = Some(Instant::now());
                }
            }
        });
    }

    fn close(&self, kind: Kind) {
        if let Some(window) = Self::slot(&mut self.windows.lock().unwrap(), kind) {
            window.send(&ToWindow::Close);
        }
    }

    fn open_dashboard(&'static self) {
        self.close(Kind::Live);
        self.open(Kind::Dashboard, "overview");
    }

    // A click on the icon: opens Live, or closes it if it is open.
    fn toggle_live(&'static self) {
        {
            let windows = self.windows.lock().unwrap();
            if windows.live.is_some() {
                drop(windows);
                self.close(Kind::Live);
                return;
            }
            // Clicking the icon takes focus from the flyout, which closes itself; that click meant "close".
            let grace = Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }) + 400);
            if windows
                .live_closed
                .is_some_and(|closed| closed.elapsed() < grace)
            {
                return;
            }
        }
        self.open(Kind::Live, "live");
    }

    fn handle(&'static self, message: ToTray) {
        match message {
            ToTray::SetTarget(target) => {
                if let Err(error) = self.tracker.set_target(target.as_deref()) {
                    *self.notice.lock().unwrap() =
                        Some(format!("Live target could not be saved: {error}"));
                }
                self.notify();
            }
            ToTray::SetEnhanced(enabled) => self.set_enhanced(enabled),
            ToTray::OpenDashboard => self.open_dashboard(),
        }
    }

    fn set_enhanced(&'static self, enabled: bool) {
        if !enabled {
            self.tracker.set_enhanced_network(false);
            self.enhanced.disable();
            self.notify();
            return;
        }
        if self.enhanced_pending.swap(true, Ordering::Relaxed) {
            return;
        }
        self.notify();
        // The approval prompt can stay open for a long time; wait for it away from the message loop.
        std::thread::spawn(move || {
            match self.enhanced.enable() {
                Ok(()) => self.tracker.set_enhanced_network(true),
                Err(error) => {
                    self.tracker.set_enhanced_network(false);
                    *self.notice.lock().unwrap() = Some(format!("Enhanced collection was not enabled. Passive log detection continues.\n\n{error}"));
                }
            }
            self.enhanced_pending.store(false, Ordering::Relaxed);
            self.notify();
        });
    }

    fn icon_data(&self) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd();
        data.uID = 1;
        data
    }

    fn update_tray(&self, add: bool) {
        let calls = self.tracker.active();
        let active = !calls.is_empty();
        let held = format_rate(self.tracker.held_rate(), self.tracker.held_rate_estimated());
        // The speed comes first: it is what the icon is hovered for.
        let mut text = match calls
            .iter()
            .find_map(|call| call.rate.map(|rate| format_rate(Some(rate), call.estimated)))
        {
            Some(arriving) => format!("Speed Tracker · {arriving} now"),
            None if active => format!("Speed Tracker · reply in progress · last {held}"),
            None => format!("Speed Tracker · {held} · TTFT {}", self.held_ttft()),
        };
        if let Some(target) = self.tracker.target() {
            text.push_str(&format!(" · {target}"));
        }
        // The shell's tooltip is short; cut on a character boundary.
        if text.chars().count() > 63 {
            text = text.chars().take(62).chain(std::iter::once('…')).collect();
        }
        let mut shown = self.shown.lock().unwrap();
        if !add && *shown == (active, text.clone()) {
            return;
        }
        let mut data = self.icon_data();
        data.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = (if active {
            self.active_icon
        } else {
            self.idle_icon
        }) as HICON;
        for (slot, unit) in data.szTip.iter_mut().zip(text.encode_utf16().take(127)) {
            *slot = unit;
        }
        unsafe { Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &data) };
        *shown = (active, text);
    }

    fn show_menu(&'static self) {
        unsafe {
            let menu = CreatePopupMenu();
            for (id, label) in [
                (1usize, "Open Dashboard"),
                (2, "Open Live"),
                (3, "Settings — optional network collector"),
                (0, ""),
                (4, "Quit Speed Tracker"),
            ] {
                if id == 0 {
                    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                } else {
                    AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
                }
            }
            let mut point = POINT { x: 0, y: 0 };
            GetCursorPos(&mut point);
            // Without this the menu does not close when the user clicks elsewhere.
            SetForegroundWindow(self.hwnd());
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                point.x,
                point.y,
                0,
                self.hwnd(),
                std::ptr::null(),
            );
            PostMessageW(self.hwnd(), WM_NULL, 0, 0);
            DestroyMenu(menu);
            match chosen {
                1 => self.open_dashboard(),
                2 => self.open(Kind::Live, "live"),
                3 => self.open(Kind::Live, "settings"),
                4 => {
                    DestroyWindow(self.hwnd());
                }
                _ => {}
            }
        }
    }
}

fn format_rate(value: Option<f64>, estimated: bool) -> String {
    match value.filter(|number| number.is_finite() && *number >= 0.0) {
        Some(number) => format!(
            "{}{number:.*} tok/s",
            if estimated { "~" } else { "" },
            if number < 10.0 { 2 } else { 1 }
        ),
        None => "—".into(),
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(app) = APP.get() else {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    };
    match message {
        WM_TRAY => match lparam as u32 {
            WM_LBUTTONUP => {
                if !app.double_clicked.swap(false, Ordering::Relaxed) {
                    // Wait out the double-click interval: a double-click opens the dashboard instead.
                    SetTimer(hwnd, CLICK_TIMER, GetDoubleClickTime(), None);
                }
            }
            WM_LBUTTONDBLCLK => {
                KillTimer(hwnd, CLICK_TIMER);
                app.double_clicked.store(true, Ordering::Relaxed);
                app.open_dashboard();
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => app.show_menu(),
            _ => {}
        },
        WM_TIMER if wparam == CLICK_TIMER => {
            KillTimer(hwnd, CLICK_TIMER);
            app.toggle_live();
        }
        WM_UPDATE => {
            app.update_pending.store(false, Ordering::Relaxed);
            app.update_tray(false);
        }
        WM_OPEN_DASHBOARD => app.open_dashboard(),
        WM_DESTROY => {
            Shell_NotifyIconW(NIM_DELETE, &app.icon_data());
            PostQuitMessage(0);
        }
        // Explorer restarted and lost every icon; put this one back.
        _ if message == app.taskbar_created && message != 0 => app.update_tray(true),
        _ => return DefWindowProcW(hwnd, message, wparam, lparam),
    }
    0
}

// A filled circle with three rising bars, blue while a call is in flight and grey otherwise.
fn create_icon(active: bool) -> isize {
    const SIZE: usize = 32;
    let background = if active {
        [30u8, 111, 203]
    } else {
        [80, 88, 102]
    };
    let mut pixels = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let distance =
                ((x as f32 + 0.5 - 16.0).powi(2) + (y as f32 + 0.5 - 16.0).powi(2)).sqrt();
            let alpha = (15.5 - distance).clamp(0.0, 1.0);
            let bar = [(7, 18), (14, 12), (21, 7)]
                .iter()
                .any(|&(left, top)| (left..left + 4).contains(&x) && (top..25).contains(&y));
            let [red, green, blue] = if bar { [255, 255, 255] } else { background };
            // Blue, green, red, alpha, with colour premultiplied by alpha.
            pixels[(y * SIZE + x) * 4..][..4].copy_from_slice(&[
                (blue as f32 * alpha) as u8,
                (green as f32 * alpha) as u8,
                (red as f32 * alpha) as u8,
                (alpha * 255.0) as u8,
            ]);
        }
    }
    let mask = vec![0u8; SIZE * SIZE / 8];
    unsafe {
        let color = CreateBitmap(SIZE as i32, SIZE as i32, 1, 32, pixels.as_ptr().cast());
        let mask = CreateBitmap(SIZE as i32, SIZE as i32, 1, 1, mask.as_ptr().cast());
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        });
        DeleteObject(color);
        DeleteObject(mask);
        icon as isize
    }
}

// config.json: the proxy's port and any extra routes. Property names match in any letter case.
fn proxy_options(home: &std::path::Path) -> (u16, Option<HashMap<String, String>>) {
    let path = home.join("config.json");
    let config = std::fs::metadata(&path)
        .ok()
        .filter(|metadata| metadata.len() <= 1024 * 1024)
        .and_then(|_| std::fs::read_to_string(&path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let field = |name: &str| {
        config
            .as_ref()
            .and_then(|config| config.as_object())
            .and_then(|object| {
                object
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
            })
            .map(|(_, value)| value.clone())
    };
    let Some(port) = field("port").map_or(Some(4141), |port| {
        port.as_u64().filter(|port| (1..=65_535).contains(port))
    }) else {
        return (4141, None);
    };
    (
        port as u16,
        field("routes").and_then(|routes| serde_json::from_value(routes).ok()),
    )
}

/// Runs until the user quits from the tray menu. Returns the process exit code.
pub fn run(open_dashboard: bool, open_live: bool) -> i32 {
    let instance = SingleInstance::acquire();
    if !instance.first {
        instance.signal_first();
        return 0;
    }
    let history = Arc::new(HistoryStore::new(None));
    let enhanced = EnhancedSampler::new();
    let sampler: Arc<dyn FlowSampler> = enhanced.clone();
    let tracker = match Tracker::new(Arc::clone(&history), Some(sampler), None, None, None) {
        Ok(tracker) => tracker,
        Err(error) => {
            message_box(&format!("Speed Tracker could not start.\n\n{error}"), true);
            return 1;
        }
    };
    let (port, routes) = proxy_options(history.home());
    let proxy = ProxyService::new(Arc::clone(&tracker), port, routes);
    // A port conflict must not disable passive detection.
    let _ = proxy.start();

    let (broadcast, changes) = mpsc::channel::<()>();
    let hwnd = unsafe {
        let class = wide("SpeedTrackerTray");
        let instance = GetModuleHandleW(std::ptr::null());
        let mut definition: WNDCLASSW = std::mem::zeroed();
        definition.lpfnWndProc = Some(window_proc);
        definition.hInstance = instance;
        definition.lpszClassName = class.as_ptr();
        RegisterClassW(&definition);
        // A hidden top-level window, not a message-only one: only top-level windows hear that Explorer restarted.
        CreateWindowExW(
            0,
            class.as_ptr(),
            wide("Speed Tracker").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null::<c_void>(),
        )
    };
    if hwnd.is_null() {
        message_box(
            "Speed Tracker could not start: the notification-area window could not be created.",
            true,
        );
        return 1;
    }
    let state = App {
        tracker: Arc::clone(&tracker),
        enhanced,
        windows: Mutex::default(),
        broadcast,
        hwnd: AtomicIsize::new(hwnd as isize),
        idle_icon: create_icon(false),
        active_icon: create_icon(true),
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        update_pending: AtomicBool::new(false),
        enhanced_pending: AtomicBool::new(false),
        history_revision: AtomicU64::new(0),
        notice: Mutex::new(None),
        shown: Mutex::new((false, String::new())),
        double_clicked: AtomicBool::new(false),
        exiting: AtomicBool::new(false),
    };
    if APP.set(state).is_err() {
        return 1;
    }
    let app = app();
    history.on_recorded(|_| {
        app.history_revision.fetch_add(1, Ordering::Relaxed);
        // The tracker announced this record before the count moved; say so again, or an open
        // dashboard would not read the newest call until something else changed.
        app.notify();
    });
    tracker.on_changed(|| {
        app.notify();
        if !app.update_pending.swap(true, Ordering::Relaxed) {
            unsafe { PostMessageW(app.hwnd(), WM_UPDATE, 0, 0) };
        }
    });
    // SPEEDTRACKER_TRACE=<file> appends a line whenever the live state changes: how a change to
    // detection is checked against real harness activity without watching the flyout.
    let mut trace = std::env::var_os("SPEEDTRACKER_TRACE")
        .and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        })
        .map(|file| (file, String::new()));
    // Sends live state to the open windows. Bursts of changes become one message.
    std::thread::spawn(move || {
        while changes.recv().is_ok() {
            while changes.try_recv().is_ok() {}
            if let Some((file, last)) = &mut trace {
                let line = app.trace_line();
                if line != *last {
                    let _ = writeln!(file, "{} {line}", chrono::Local::now().format("%H:%M:%S%.3f"));
                    *last = line;
                }
            }
            let mut windows = app.windows.lock().unwrap();
            if windows.live.is_some() || windows.dashboard.is_some() {
                let message = ToWindow::Snapshot(Box::new(app.snapshot()));
                let Windows {
                    live, dashboard, ..
                } = &mut *windows;
                for window in [live, dashboard].into_iter().flatten() {
                    window.send(&message);
                }
            }
            drop(windows);
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    // A second launch signals this event instead of starting another instance.
    let reopen = instance.reopen as isize;
    std::thread::spawn(move || unsafe {
        while WaitForSingleObject(reopen as _, INFINITE) == 0 {
            PostMessageW(app.hwnd(), WM_OPEN_DASHBOARD, 0, 0);
        }
    });
    app.update_tray(true);
    tracker.start();
    if open_dashboard {
        app.open_dashboard();
    } else if open_live {
        app.open(Kind::Live, "live");
    }

    unsafe {
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    app.exiting.store(true, Ordering::Relaxed);
    app.close(Kind::Live);
    app.close(Kind::Dashboard);
    tracker.set_enhanced_network(false);
    app.enhanced.disable();
    tracker.dispose();
    proxy.stop();
    unsafe {
        DestroyIcon(app.idle_icon as HICON);
        DestroyIcon(app.active_icon as HICON);
    }
    drop(instance);
    0
}
