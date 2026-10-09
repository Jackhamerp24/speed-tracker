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
    GetMessageW, GetSystemMetrics, SM_CXSMICON, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, ICONINFO, MF_SEPARATOR,
    MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    WNDCLASSW,
};

const WM_TRAY: u32 = WM_APP + 1;
const WM_UPDATE: u32 = WM_APP + 2;
const WM_OPEN_DASHBOARD: u32 = WM_APP + 3;
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
    // The notification area's icon size in pixels, and the icon now shown there.
    icon_size: usize,
    icon: AtomicIsize,
    taskbar_created: u32,
    update_pending: AtomicBool,
    enhanced_pending: AtomicBool,
    history_revision: AtomicU64,
    notice: Mutex<Option<String>>,
    // What the icon currently shows, to avoid redundant shell calls: in flight, its number, its tooltip.
    shown: Mutex<(bool, Option<String>, String)>,
    exiting: AtomicBool,
    // Icons that have been replaced and not yet destroyed.
    retired: Mutex<Vec<isize>>,
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
                .recent(30, tracker.target().as_deref())
                .iter()
                .map(|record| (**record).clone())
                .collect(),
            all_recent: tracker
                .recent(60, None)
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
        // Like the flyout, the icon follows the Live target: another harness's call does not light it.
        let target = self.tracker.target();
        let calls: Vec<_> = self
            .tracker
            .active()
            .into_iter()
            .filter(|call| {
                target
                    .as_ref()
                    .is_none_or(|target| call.harness.eq_ignore_ascii_case(target))
            })
            .collect();
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
        if let Some(target) = &target {
            text.push_str(&format!(" · {target}"));
        }
        // The shell's tooltip is short; cut on a character boundary.
        if text.chars().count() > 63 {
            text = text.chars().take(62).chain(std::iter::once('…')).collect();
        }
        // The icon carries the same number the tooltip leads with: arriving now, or else the last reply's.
        let number = icon_text(
            calls
                .iter()
                .find_map(|call| call.rate)
                .or(self.tracker.held_rate()),
        );
        let mut shown = self.shown.lock().unwrap();
        if !add && *shown == (active, number.clone(), text.clone()) {
            return;
        }
        if add || (shown.0, &shown.1) != (active, &number) {
            let icon = create_icon(
                self.icon_size,
                &icon_pixels(self.icon_size, active, number.as_deref()),
            );
            let previous = self.icon.swap(icon, Ordering::Relaxed);
            if previous != 0 {
                // The shell copies an icon when it is set, so the old one can go once replaced below.
                self.retired.lock().unwrap().push(previous);
            }
        }
        let mut data = self.icon_data();
        data.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icon.load(Ordering::Relaxed) as HICON;
        for (slot, unit) in data.szTip.iter_mut().zip(text.encode_utf16().take(127)) {
            *slot = unit;
        }
        unsafe {
            Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &data);
            for icon in self.retired.lock().unwrap().drain(..) {
                DestroyIcon(icon as HICON);
            }
        }
        *shown = (active, number, text);
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
                app.toggle_live();
            }
            WM_LBUTTONDBLCLK => {
                app.open_dashboard();
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => app.show_menu(),
            _ => {}
        },
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

// What the icon says: the speed as a whole number, since three characters are all that fit.
// None until a speed has been measured.
pub fn icon_text(rate: Option<f64>) -> Option<String> {
    let rate = rate.filter(|rate| rate.is_finite() && *rate >= 0.0)?.round();
    Some(if rate < 1000.0 {
        format!("{rate:.0}")
    } else if rate < 99_500.0 {
        format!("{:.0}k", rate / 1000.0)
    } else {
        "99k".into()
    })
}

// Four-by-seven pixel digits and a "k", one byte per row, the leftmost pixel in bit 3. At the
// notification area's sixteen pixels a drawn font is a smudge; this stays legible.
fn glyph(character: char) -> [u8; 7] {
    match character {
        '0' => [0b0110, 0b1001, 0b1001, 0b1001, 0b1001, 0b1001, 0b0110],
        '1' => [0b0010, 0b0110, 0b0010, 0b0010, 0b0010, 0b0010, 0b0111],
        '2' => [0b0110, 0b1001, 0b0001, 0b0010, 0b0100, 0b1000, 0b1111],
        '3' => [0b1110, 0b0001, 0b0001, 0b0110, 0b0001, 0b0001, 0b1110],
        '4' => [0b0010, 0b0110, 0b1010, 0b1010, 0b1111, 0b0010, 0b0010],
        '5' => [0b1111, 0b1000, 0b1110, 0b0001, 0b0001, 0b1001, 0b0110],
        '6' => [0b0110, 0b1000, 0b1110, 0b1001, 0b1001, 0b1001, 0b0110],
        '7' => [0b1111, 0b0001, 0b0010, 0b0010, 0b0100, 0b0100, 0b0100],
        '8' => [0b0110, 0b1001, 0b1001, 0b0110, 0b1001, 0b1001, 0b0110],
        '9' => [0b0110, 0b1001, 0b1001, 0b1001, 0b0111, 0b0001, 0b0110],
        'k' => [0b1000, 0b1000, 0b1001, 0b1010, 0b1100, 0b1010, 0b1001],
        _ => [0; 7],
    }
}

/// The icon as blue, green, red, alpha bytes with colour premultiplied by alpha: blue while a call
/// is in flight and grey otherwise, carrying the speed once one is known and three rising bars until then.
pub fn icon_pixels(size: usize, active: bool, text: Option<&str>) -> Vec<u8> {
    let background = if active {
        [30u8, 111, 203]
    } else {
        [80, 88, 102]
    };
    let mut pixels = vec![0u8; size * size * 4];
    let mut put = |x: usize, y: usize, [red, green, blue]: [u8; 3], alpha: f32| {
        pixels[(y * size + x) * 4..][..4].copy_from_slice(&[
            (blue as f32 * alpha) as u8,
            (green as f32 * alpha) as u8,
            (red as f32 * alpha) as u8,
            (alpha * 255.0) as u8,
        ]);
    };
    let Some(text) = text else {
        // A filled circle with three rising bars, drawn on a 32-pixel grid and scaled to the icon.
        let scale = size as f32 / 32.0;
        for y in 0..size {
            for x in 0..size {
                let (gx, gy) = ((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale);
                let distance = ((gx - 16.0).powi(2) + (gy - 16.0).powi(2)).sqrt();
                let alpha = ((15.5 - distance) * scale).clamp(0.0, 1.0);
                let bar = [(7.0, 18.0), (14.0, 12.0), (21.0, 7.0)]
                    .iter()
                    .any(|&(left, top)| (left..left + 4.0).contains(&gx) && (top..25.0).contains(&gy));
                put(x, y, if bar { [255, 255, 255] } else { background }, alpha);
            }
        }
        return pixels;
    };
    // A full square with clipped corners gives the digits every pixel of width there is.
    let corner = (size / 8).max(1);
    for y in 0..size {
        for x in 0..size {
            let (edge_x, edge_y) = (x.min(size - 1 - x), y.min(size - 1 - y));
            if edge_x + edge_y >= corner {
                put(x, y, background, 1.0);
            }
        }
    }
    let count = text.chars().count().min(3);
    // Each digit is four units wide with one between, and seven tall. The units are as large as the
    // icon allows, and up to twice as tall as wide: at sixteen pixels that is what makes three digits readable.
    let wide = (size / (count * 5 - 1)).min((size - 2) / 7).max(1);
    let tall = ((size - 2) / 7).min(2 * wide).max(1);
    let (left, top) = (
        size.saturating_sub((count * 5 - 1) * wide) / 2,
        size.saturating_sub(7 * tall) / 2,
    );
    for (index, character) in text.chars().take(3).enumerate() {
        for (row, bits) in glyph(character).iter().enumerate() {
            for column in 0..4 {
                if bits & (0b1000 >> column) == 0 {
                    continue;
                }
                for dy in 0..tall {
                    for dx in 0..wide {
                        let (x, y) = (
                            left + (index * 5 + column) * wide + dx,
                            top + row * tall + dy,
                        );
                        if x < size && y < size {
                            put(x, y, [255, 255, 255], 1.0);
                        }
                    }
                }
            }
        }
    }
    pixels
}

fn create_icon(size: usize, pixels: &[u8]) -> isize {
    // One bit a pixel, each row padded to a whole number of 16-bit words. All zero: alpha decides.
    let mask = vec![0u8; size.div_ceil(16) * 2 * size];
    unsafe {
        let color = CreateBitmap(size as i32, size as i32, 1, 32, pixels.as_ptr().cast());
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, mask.as_ptr().cast());
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

/// `--icon-preview FILE.png`: every state of the tray icon at the three sizes Windows uses, each also
/// enlarged, so a change to it can be looked at without hunting for it in the notification area.
pub fn icon_preview(path: &str) -> i32 {
    let states: [(bool, Option<&str>); 6] = [
        (false, None),
        (true, None),
        (false, Some("87")),
        (true, Some("121")),
        (true, Some("5")),
        (false, Some("1k")),
    ];
    let sizes = [16usize, 24, 32];
    const ZOOM: usize = 6;
    let cell = 32 * ZOOM + 16;
    let mut sheet = image::RgbaImage::from_pixel(
        (cell * states.len()) as u32,
        (cell * sizes.len() * 2) as u32,
        image::Rgba([32, 34, 38, 255]),
    );
    for (row, size) in sizes.iter().enumerate() {
        for (column, (active, text)) in states.iter().enumerate() {
            let pixels = icon_pixels(*size, *active, *text);
            for zoom in [1, ZOOM] {
                let top = (row * 2 + usize::from(zoom == ZOOM)) * cell + 8;
                for y in 0..*size {
                    for x in 0..*size {
                        let [blue, green, red, alpha] =
                            <[u8; 4]>::try_from(&pixels[(y * size + x) * 4..][..4]).unwrap();
                        if alpha == 0 {
                            continue;
                        }
                        for dy in 0..zoom {
                            for dx in 0..zoom {
                                sheet.put_pixel(
                                    (column * cell + 8 + x * zoom + dx) as u32,
                                    (top + y * zoom + dy) as u32,
                                    image::Rgba([red, green, blue, 255]),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    i32::from(sheet.save(path).is_err())
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
        icon_size: (unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16)) as usize,
        icon: AtomicIsize::new(0),
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        update_pending: AtomicBool::new(false),
        enhanced_pending: AtomicBool::new(false),
        history_revision: AtomicU64::new(0),
        notice: Mutex::new(None),
        shown: Mutex::new((false, None, String::new())),
        exiting: AtomicBool::new(false),
        retired: Mutex::default(),
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
    unsafe { DestroyIcon(app.icon.load(Ordering::Relaxed) as HICON) };
    drop(instance);
    0
}

#[cfg(test)]
mod tests {
    use super::{icon_pixels, icon_text};

    #[test]
    fn the_icon_shows_the_speed_as_a_whole_number_of_at_most_three_characters() {
        assert_eq!(icon_text(Some(87.4)).as_deref(), Some("87"));
        assert_eq!(icon_text(Some(120.6)).as_deref(), Some("121"));
        assert_eq!(icon_text(Some(0.2)).as_deref(), Some("0"), "a measured crawl is a number, not a blank");
        assert_eq!(icon_text(Some(999.4)).as_deref(), Some("999"));
        assert_eq!(icon_text(Some(999.6)).as_deref(), Some("1k"), "what rounds to a thousand no longer fits as digits");
        assert_eq!(icon_text(Some(2400.0)).as_deref(), Some("2k"));
        assert_eq!(icon_text(Some(12_345.0)).as_deref(), Some("12k"));
        assert_eq!(icon_text(Some(5_000_000.0)).as_deref(), Some("99k"), "never more than three characters");
    }

    #[test]
    fn with_no_usable_speed_the_icon_shows_no_number() {
        for rate in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.0)] {
            assert_eq!(icon_text(rate), None, "{rate:?}");
        }
    }

    // Blue, green, red, alpha at one pixel.
    fn pixel(pixels: &[u8], size: usize, x: usize, y: usize) -> [u8; 4] {
        pixels[(y * size + x) * 4..][..4].try_into().unwrap()
    }

    #[test]
    fn at_sixteen_pixels_three_digits_fill_the_icon_two_pixels_tall_per_row() {
        // "111" at 16 px: units 1 wide and 2 tall, so the text is 14 x 14 starting at (1, 1). The top
        // row of a "1" is 0010: its only lit pixel is the third column, x = 1 + 2 for the first digit.
        let pixels = icon_pixels(16, true, Some("111"));
        assert_eq!(pixel(&pixels, 16, 3, 1), [255, 255, 255, 255], "the digit is white");
        assert_eq!(pixel(&pixels, 16, 3, 2), [255, 255, 255, 255], "and each row is two pixels tall");
        assert_eq!(pixel(&pixels, 16, 1, 1), [203, 111, 30, 255], "beside it is the in-flight blue");
        assert_eq!(pixel(&pixels, 16, 0, 0), [0, 0, 0, 0], "the corner is clipped");
        assert_eq!(pixel(&icon_pixels(16, false, Some("111")), 16, 1, 1), [102, 88, 80, 255], "grey when no call is in flight");
    }

    #[test]
    fn every_size_and_length_draws_inside_the_icon() {
        for size in 16..=64 {
            for text in [None, Some("5"), Some("87"), Some("121"), Some("12k"), Some("1234")] {
                assert_eq!(icon_pixels(size, size % 2 == 0, text).len(), size * size * 4, "{size} px, {text:?}");
            }
        }
    }
}
