//! The Live flyout and the Dashboard. Each runs as its own short-lived process, started by the tray
//! app, which sends live state down standard input and takes requests back on standard output.

mod dashboard;
mod live;
mod plot;

use super::ipc::{Snapshot, ToTray, ToWindow};
use super::win::message_box;
use chrono::{DateTime, Local, Utc};
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Margin,
    RichText, Stroke, TextStyle, Theme, Ui,
};
use speedtracker::domain::{DashboardSummary, RequestRecord};
use speedtracker::time::{Time, TICKS_PER_SECOND};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETHIGHCONTRAST};

#[derive(Default)]
struct LinkState {
    snapshot: Snapshot,
    // Requests from the tray that the window acts on in its next frame.
    focus: bool,
    close: bool,
    tab: Option<String>,
    notice: Option<String>,
    connected: bool,
    context: Option<egui::Context>,
}

/// The window's connection to the tray app.
#[derive(Clone, Default)]
pub struct Link(Arc<Mutex<LinkState>>);

impl Link {
    fn start() -> Link {
        let link = Link::default();
        let state = Arc::clone(&link.0);
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let Ok(message) = serde_json::from_str::<ToWindow>(&line) else {
                    continue;
                };
                let mut state = state.lock().unwrap();
                state.connected = true;
                match message {
                    ToWindow::Snapshot(snapshot) => {
                        if let Some(notice) = &snapshot.notice {
                            state.notice = Some(notice.clone());
                        }
                        state.snapshot = *snapshot;
                    }
                    ToWindow::Focus => state.focus = true,
                    ToWindow::Close => state.close = true,
                    ToWindow::ShowTab(tab) => state.tab = Some(tab),
                }
                if let Some(context) = &state.context {
                    context.request_repaint();
                }
            }
            // The tray app has gone; its windows go with it. A window started by hand has no tray and stays.
            let mut state = state.lock().unwrap();
            if state.connected {
                state.close = true;
                if let Some(context) = &state.context {
                    context.request_repaint();
                }
            }
        });
        link
    }

    pub fn send(&self, message: ToTray) {
        if let Ok(line) = serde_json::to_string(&message) {
            let mut output = std::io::stdout().lock();
            let _ = writeln!(output, "{line}").and_then(|_| output.flush());
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.0.lock().unwrap().snapshot.clone()
    }
    /// Applies what the tray asked for since the last frame. Returns a tab to switch to, if any.
    pub fn apply_requests(&self, context: &egui::Context) -> Option<String> {
        let mut state = self.0.lock().unwrap();
        if std::mem::take(&mut state.close) {
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if std::mem::take(&mut state.focus) {
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            context.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        state.tab.take()
    }
    pub fn take_notice(&self) -> Option<String> {
        self.0.lock().unwrap().notice.take()
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Color32,
    pub panel: Color32,
    pub panel_raised: Color32,
    pub foreground: Color32,
    pub muted: Color32,
    pub border: Color32,
    pub divider: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub secondary: Color32,
}

fn high_contrast() -> bool {
    let mut settings = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        dwFlags: 0,
        lpszDefaultScheme: std::ptr::null_mut(),
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            settings.cbSize,
            (&mut settings as *mut HIGHCONTRASTW).cast(),
            0,
        ) != 0
            && settings.dwFlags & HCF_HIGHCONTRASTON != 0
    }
}

pub fn palette(dark: bool) -> Palette {
    static HIGH_CONTRAST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let rgb = Color32::from_rgb;
    if *HIGH_CONTRAST.get_or_init(high_contrast) {
        // Maximum separation, no greys: every line and label is fully legible.
        let (back, fore) = if dark {
            (Color32::BLACK, Color32::WHITE)
        } else {
            (Color32::WHITE, Color32::BLACK)
        };
        return Palette {
            background: back,
            panel: back,
            panel_raised: back,
            foreground: fore,
            muted: fore,
            border: fore,
            divider: fore,
            accent: if dark {
                rgb(0x1A, 0xEB, 0xFF)
            } else {
                rgb(0x00, 0x37, 0xDA)
            },
            accent_soft: if dark {
                Color32::from_rgba_unmultiplied(0x1A, 0xEB, 0xFF, 40)
            } else {
                Color32::from_rgba_unmultiplied(0x00, 0x37, 0xDA, 40)
            },
            secondary: fore,
        };
    }
    if dark {
        Palette {
            background: rgb(0x18, 0x1A, 0x1E),
            panel: rgb(0x22, 0x25, 0x2B),
            panel_raised: rgb(0x2B, 0x2E, 0x36),
            foreground: rgb(0xF4, 0xF5, 0xF7),
            muted: rgb(0x8E, 0x95, 0xA3),
            border: rgb(0x34, 0x39, 0x44),
            divider: rgb(0x2B, 0x2F, 0x38),
            accent: rgb(0x21, 0xC6, 0x8F), // Signature Speed Tracker green accent
            accent_soft: Color32::from_rgba_unmultiplied(0x21, 0xC6, 0x8F, 38),
            secondary: rgb(0xF6, 0xBA, 0x73),
        }
    } else {
        Palette {
            background: rgb(0xEE, 0xF0, 0xF3),
            panel: rgb(0xFF, 0xFF, 0xFF),
            panel_raised: rgb(0xF4, 0xF5, 0xF8),
            foreground: rgb(0x18, 0x1A, 0x20),
            muted: rgb(0x6A, 0x72, 0x82),
            border: rgb(0xDB, 0xDF, 0xE7),
            divider: rgb(0xE6, 0xEA, 0xF0),
            accent: rgb(0x14, 0xA2, 0x72),
            accent_soft: Color32::from_rgba_unmultiplied(0x14, 0xA2, 0x72, 30),
            secondary: rgb(0xA7, 0x58, 0x05),
        }
    }
}

pub fn colors(ui: &Ui) -> Palette {
    palette(ui.visuals().dark_mode)
}

/// Brand colors for each supported harness matching the macOS version.
pub fn harness_color(harness: &str) -> Color32 {
    match harness {
        "Claude Code" | "Claude Desktop" => Color32::from_rgb(232, 122, 79), // Terracotta / warm orange
        "Codex" => Color32::from_rgb(51, 196, 143),                          // Emerald green
        "OMP" => Color32::from_rgb(171, 125, 245),                           // Violet purple
        "Pi" => Color32::from_rgb(105, 142, 250),                            // Soft blue
        "Gemini CLI" => Color32::from_rgb(74, 156, 250),                     // Gemini blue
        "Antigravity" => Color32::from_rgb(237, 92, 120),                    // Coral pink
        "DeepSeek CLI" => Color32::from_rgb(92, 128, 255),                  // Royal blue
        "opencode" => Color32::from_rgb(143, 161, 184),                      // Slate blue
        _ => Color32::from_rgb(51, 196, 143),
    }
}

// Segoe UI from the system, so nothing is bundled and text looks like the rest of Windows.
fn install_fonts(context: &egui::Context) {
    let folder = std::path::PathBuf::from(
        std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()),
    )
    .join("Fonts");
    let mut fonts = FontDefinitions::empty();
    // The first is the text face; the rest fill in symbols it lacks.
    for name in ["segoeui.ttf", "seguisym.ttf", "arial.ttf", "tahoma.ttf"] {
        if let Ok(bytes) = std::fs::read(folder.join(name)) {
            fonts
                .font_data
                .insert(name.to_string(), Arc::new(FontData::from_owned(bytes)));
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push(name.to_string());
            }
        }
    }
    if !fonts.font_data.is_empty() {
        context.set_fonts(fonts);
    }
}

fn install_theme(context: &egui::Context) {
    for (theme, dark) in [(Theme::Light, false), (Theme::Dark, true)] {
        let colors = palette(dark);
        let mut visuals = if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.panel_fill = colors.background;
        visuals.window_fill = colors.panel;
        visuals.extreme_bg_color = colors.panel;
        visuals.faint_bg_color = colors.background;
        visuals.hyperlink_color = colors.accent;
        visuals.selection.bg_fill = colors.accent.gamma_multiply(0.35);
        visuals.selection.stroke = Stroke::new(1.0_f32, colors.accent);
        visuals.window_stroke = Stroke::new(1.0_f32, colors.border);
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, colors.border);
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, colors.foreground);
        for widget in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.fg_stroke.color = colors.foreground;
            widget.corner_radius = CornerRadius::same(5);
        }
        visuals.widgets.inactive.bg_fill = colors.panel;
        visuals.widgets.inactive.weak_bg_fill = colors.panel;
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, colors.border);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, colors.accent);
        context.set_visuals_of(theme, visuals);
    }
    context.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Small, 11.0),
            (TextStyle::Body, 13.0),
            (TextStyle::Button, 13.0),
            (TextStyle::Monospace, 13.0),
            (TextStyle::Heading, 17.0),
        ]
        .into_iter()
        .map(|(style, size)| (style, FontId::proportional(size)))
        .collect();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.interact_size.y = 30.0;
    });
}

/// A line of wrapped text.
pub fn text(ui: &mut Ui, content: impl Into<String>, size: f32, muted: bool) -> egui::Response {
    let colors = colors(ui);
    ui.add(
        egui::Label::new(RichText::new(content).size(size).color(if muted {
            colors.muted
        } else {
            colors.foreground
        }))
        .wrap(),
    )
}

/// A bordered panel.
pub fn card<R>(ui: &mut Ui, padding: i8, contents: impl FnOnce(&mut Ui) -> R) -> R {
    let colors = colors(ui);
    Frame::new()
        .fill(colors.panel)
        .stroke(Stroke::new(1.0_f32, colors.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(padding))
        .show(ui, contents)
        .inner
}

/// A bordered panel with custom corner radius and fill.
pub fn card_styled<R>(
    ui: &mut Ui,
    padding: i8,
    radius: u8,
    fill: Color32,
    border: Color32,
    contents: impl FnOnce(&mut Ui) -> R,
) -> R {
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(radius))
        .inner_margin(Margin::same(padding))
        .show(ui, contents)
        .inner
}

/// Upper-cased bold section label with muted styling matching macOS SectionLabel.
pub fn section_label(ui: &mut Ui, content: &str) -> egui::Response {
    let colors = colors(ui);
    ui.label(
        RichText::new(content.to_uppercase())
            .size(10.0)
            .strong()
            .color(colors.muted),
    )
}

/// Paints the vector lightning bolt icon into the given rect.
pub fn paint_lightning_bolt(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let p = |rx: f32, ry: f32| egui::pos2(rect.left() + rect.width() * rx, rect.top() + rect.height() * ry);
    let points = vec![
        p(0.56, 0.08),
        p(0.20, 0.54),
        p(0.48, 0.54),
        p(0.38, 0.92),
        p(0.80, 0.44),
        p(0.52, 0.44),
    ];
    painter.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
}

/// Paints the radar scope target icon for Watching.
pub fn paint_scope_icon(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32) {
    painter.circle_stroke(center, radius, Stroke::new(1.3_f32, color));
    painter.circle_filled(center, radius * 0.32, color);
    let r_out = radius * 1.45;
    let r_in = radius * 0.85;
    painter.line_segment([center - egui::vec2(0.0, r_out), center - egui::vec2(0.0, r_in)], Stroke::new(1.3_f32, color));
    painter.line_segment([center + egui::vec2(0.0, r_in), center + egui::vec2(0.0, r_out)], Stroke::new(1.3_f32, color));
    painter.line_segment([center - egui::vec2(r_out, 0.0), center - egui::vec2(r_in, 0.0)], Stroke::new(1.3_f32, color));
    painter.line_segment([center + egui::vec2(r_in, 0.0), center + egui::vec2(r_out, 0.0)], Stroke::new(1.3_f32, color));
}

/// Paints a colored dot for a harness.
#[allow(dead_code)]
pub fn harness_dot(ui: &mut Ui, harness: &str, radius: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(radius * 2.0, radius * 2.0), egui::Sense::hover());
    let color = harness_color(harness);
    ui.painter().circle_filled(rect.center(), radius, color);
    response
}

pub mod format {
    use super::*;

    /// A measurement to two decimals below ten and one above; "—" when unavailable; "~" when estimated.
    pub fn number(value: Option<f64>, suffix: &str, estimated: bool) -> String {
        match value.filter(|number| number.is_finite() && *number >= 0.0) {
            Some(number) => format!(
                "{}{number:.*}{suffix}",
                if estimated { "~" } else { "" },
                if number < 10.0 { 2 } else { 1 }
            ),
            None => "—".into(),
        }
    }
    /// A whole number with thousands separators.
    pub fn count(value: i64) -> String {
        let digits = value.unsigned_abs().to_string();
        let mut grouped = String::new();
        for (index, digit) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index) % 3 == 0 {
                grouped.push(',');
            }
            grouped.push(digit);
        }
        if value < 0 {
            format!("-{grouped}")
        } else {
            grouped
        }
    }
    pub fn latency(record: &RequestRecord) -> Option<f64> {
        record
            .ttft
            .filter(|_| DashboardSummary::valid_latency(record))
    }
    pub fn speed(record: &RequestRecord) -> Option<f64> {
        record.tps.filter(|_| DashboardSummary::valid_speed(record))
    }
    pub fn source(record: &RequestRecord) -> &'static str {
        match record.source.as_deref() {
            Some("log") => "Session log",
            Some("network") => "Passive network estimate",
            Some("proxy") => "Observed proxy",
            _ => "Unknown / legacy source",
        }
    }
    pub fn outcome(record: &RequestRecord) -> String {
        if record.aborted {
            "Interrupted".into()
        } else if record.status >= 400 {
            format!("Error {}", record.status)
        } else if record.status > 0 {
            record.status.to_string()
        } else {
            "Status unavailable".into()
        }
    }
    /// The instant in the user's time zone, in a chrono format.
    pub fn local(time: Time, pattern: &str) -> String {
        let seconds = time.0.div_euclid(TICKS_PER_SECOND);
        let nanos = (time.0.rem_euclid(TICKS_PER_SECOND) * 100) as u32;
        match DateTime::<Utc>::from_timestamp(seconds, nanos) {
            Some(utc) => utc.with_timezone(&Local).format(pattern).to_string(),
            None => "—".into(),
        }
    }
    pub const SHORT: &str = "%Y-%m-%d %H:%M";

    /// How long something has been going on: seconds, then minutes and seconds.
    #[allow(dead_code)]
    pub fn elapsed(seconds: f64) -> String {
        let seconds = seconds.max(0.0) as u64;
        if seconds < 60 {
            format!("{seconds} s")
        } else {
            format!("{} min {:02} s", seconds / 60, seconds % 60)
        }
    }

    /// How long ago something ended, in the largest unit that fits.
    pub fn ago(seconds: f64) -> String {
        let seconds = seconds.max(0.0) as u64;
        match seconds {
            0..=1 => "just now".into(),
            2..=59 => format!("{seconds} s ago"),
            60..=3599 => format!("{} min ago", seconds / 60),
            3600..=86_399 => format!("{} h ago", seconds / 3600),
            _ => format!("{} d ago", seconds / 86_400),
        }
    }

    /// Short relative time for compact rows: 40s, 3m, 25m, 1h, 3d.
    pub fn ago_compact(seconds: f64) -> String {
        let seconds = seconds.max(0.0) as u64;
        match seconds {
            0..=1 => "just now".into(),
            2..=59 => format!("{seconds}s"),
            60..=3599 => format!("{}m", seconds / 60),
            3600..=86_399 => format!("{}h", seconds / 3600),
            _ => format!("{}d", seconds / 86_400),
        }
    }

    /// Latency formatted as ms below 1s and s above, or "—".
    pub fn latency_compact(seconds: Option<f64>) -> String {
        match seconds.filter(|s| s.is_finite() && *s >= 0.0) {
            Some(s) if s < 1.0 => format!("{:.0} ms", (s * 1000.0).max(0.0)),
            Some(s) => format!("{:.2} s", s),
            None => "—".into(),
        }
    }

    /// Returns value and unit separated for metric tiles, e.g. ("930", "ms") or ("1.05", "s").
    pub fn duration_parts(seconds: f64) -> (String, &'static str) {
        if seconds < 1.0 {
            (format!("{:.0}", (seconds * 1000.0).max(0.0)), "ms")
        } else {
            (format!("{:.2}", seconds), "s")
        }
    }

    /// Whole rate e.g. "68" or "~68".
    pub fn rate_whole(value: Option<f64>, estimated: bool) -> String {
        match value.filter(|number| number.is_finite() && *number >= 0.0) {
            Some(number) => format!("{}{:.0}", if estimated { "~" } else { "" }, number.round()),
            None => "—".into(),
        }
    }
}

// Renders a window to a PNG and exits: how a change to the windows is checked without clicking through them.
struct Screenshot {
    window: Box<dyn eframe::App>,
    path: std::path::PathBuf,
    frames: u32,
}

impl eframe::App for Screenshot {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        self.window.clear_color(visuals)
    }

    fn update(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
        self.window.update(context, frame);
        self.frames += 1;
        // Give history time to load and the layout time to settle before capturing.
        if self.frames == 40 {
            context.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let captured = context.input(|input| {
            input.events.iter().find_map(|event| {
                if let egui::Event::Screenshot { image, .. } = event {
                    Some(Arc::clone(image))
                } else {
                    None
                }
            })
        });
        if let Some(captured) = captured {
            let bytes: Vec<u8> = captured
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_array())
                .collect();
            if let Some(picture) =
                image::RgbaImage::from_raw(captured.size[0] as u32, captured.size[1] as u32, bytes)
            {
                let _ = picture.save(&self.path);
            }
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        context.request_repaint_after(std::time::Duration::from_millis(50));
    }
}

// Made-up live state for pictures of the flyout, so a screenshot never shows anyone's real history.
fn demo(kind: &str) -> Snapshot {
    use speedtracker::domain::{HarnessStatus, LiveCall};
    let now = Time::now();
    let reply = |harness: &str, model: &str, ago: f64, ttft: f64, generation: f64, tokens: i32| {
        RequestRecord {
            started_at: now.add_seconds(-ago - ttft - generation),
            harness: harness.into(),
            model: model.into(),
            streamed: true,
            status: 200,
            ttft: Some(ttft),
            generation: Some(generation),
            total: ttft + generation,
            output_tokens: tokens,
            tps: Some(f64::from(tokens) / generation),
            source: Some("log".into()),
            ..RequestRecord::default()
        }
    };
    let all_recent = vec![
        reply("Claude Code", "claude-opus-5-5", 40.0, 1.42, 12.6, 862),
        reply("Codex", "gpt-6.1-sol", 180.0, 0.88, 5.4, 767),
        reply("DeepSeek CLI", "deepseek-reasoner", 780.0, 2.31, 21.0, 729),
        reply("Claude Code", "claude-haiku-4-5-20251001", 1500.0, 0.41, 2.2, 376),
        reply("OMP", "gpt-6-astra", 3600.0, 0.93, 8.2, 1082),
    ];
    let recent = all_recent.clone();
    let call = |phase: &str, rate: Option<f64>| LiveCall {
        id: "demo".into(),
        harness: "Claude Code".into(),
        model: "claude-opus-5-5".into(),
        provider: "anthropic".into(),
        phase: phase.into(),
        started_at: now.add_seconds(-11.0),
        last_activity: now,
        ttft: Some(1.31),
        rate,
        output_tokens: rate.map(|_| 638),
        estimated: true,
    };
    let streaming = kind == "streaming";
    let waiting = kind == "waiting";
    Snapshot {
        active: if streaming {
            vec![call("Streaming · network estimate", Some(68.0))]
        } else if waiting {
            vec![call("Waiting for first token", None)]
        } else {
            Vec::new()
        },
        harnesses: [
            ("Claude Code", true, true),
            ("Codex", true, true),
            ("OMP", true, false),
            ("DeepSeek CLI", true, false),
        ]
        .iter()
        .map(|&(name, has_logs, is_running)| HarnessStatus {
            name: name.to_string(),
            has_logs,
            is_running,
            limitation: None,
            is_installed: true,
        })
        .collect(),
        held_rate: Some(68.4),
        held_ttft: Some(1.31),
        held_record: Some(all_recent[0].clone()),
        recent,
        all_recent,
        network_status: if streaming {
            "Passive TCP byte counters".into()
        } else {
            "Standard-user log-first tracking. Enhanced network collection is off.".into()
        },
        enhanced_available: true,
        enhanced_enabled: streaming,
        ..Snapshot::default()
    }
}

/// `--ui live|dashboard [--tab NAME] [--work left,top,right,bottom] [--screenshot FILE.png] [--theme dark|light] [--demo idle|waiting|streaming]`
pub fn run(arguments: &[String]) -> i32 {
    let value = |flag: &str| {
        arguments
            .iter()
            .position(|argument| argument == flag)
            .and_then(|index| arguments.get(index + 1))
            .map(String::as_str)
    };
    let dashboard = arguments.first().map(String::as_str) == Some("dashboard");
    let tab = value("--tab").unwrap_or("").to_string();
    let work: Option<[i32; 4]> = value("--work").and_then(|text| {
        let parts: Vec<i32> = text
            .split(',')
            .filter_map(|part| part.parse().ok())
            .collect();
        parts.try_into().ok()
    });
    let screenshot = value("--screenshot").map(std::path::PathBuf::from);
    let theme = value("--theme").map(|theme| {
        if theme == "dark" {
            Theme::Dark
        } else {
            Theme::Light
        }
    });
    let link = Link::start();
    if let Some(kind) = value("--demo") {
        link.0.lock().unwrap().snapshot = demo(kind);
    }
    // Window positions are given in points of the primary display's scale.
    let scale = unsafe { GetDpiForSystem() }.max(96) as f32 / 96.0;
    let viewport = if dashboard {
        dashboard::viewport()
    } else {
        live::viewport(work, scale)
    };
    let options = eframe::NativeOptions {
        viewport,
        centered: dashboard,
        ..Default::default()
    };
    let result = eframe::run_native(
        "Speed Tracker",
        options,
        Box::new(move |creation| {
            install_fonts(&creation.egui_ctx);
            install_theme(&creation.egui_ctx);
            if let Some(theme) = theme {
                creation.egui_ctx.set_theme(theme);
            }
            link.0.lock().unwrap().context = Some(creation.egui_ctx.clone());
            let window = if dashboard {
                Box::new(dashboard::Dashboard::new(link, &tab)) as Box<dyn eframe::App>
            } else {
                Box::new(live::Live::new(link, &tab, work, screenshot.is_some()))
            };
            Ok(match screenshot {
                Some(path) => Box::new(Screenshot {
                    window,
                    path,
                    frames: 0,
                }),
                None => window,
            })
        }),
    );
    match result {
        Ok(()) => 0,
        Err(error) => {
            message_box(
                &format!("The Speed Tracker window could not be opened.\n\n{error}"),
                true,
            );
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format::{ago, count, elapsed, number, outcome, source};
    use speedtracker::domain::RequestRecord;

    #[test]
    fn measurements_show_two_decimals_below_ten_and_one_from_ten_up() {
        assert_eq!(number(Some(9.994), "", false), "9.99");
        assert_eq!(number(Some(10.0), "", false), "10.0");
        assert_eq!(number(Some(123.456), " tok/s", false), "123.5 tok/s");
        assert_eq!(number(Some(0.0), " s", false), "0.00 s", "a measured zero is a number, not a dash");
    }

    #[test]
    fn an_estimate_is_marked_with_a_tilde() {
        assert_eq!(number(Some(42.0), " tok/s", true), "~42.0 tok/s");
    }

    #[test]
    fn a_missing_or_impossible_measurement_is_a_dash_never_a_zero() {
        for value in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.0)] {
            assert_eq!(number(value, " tok/s", true), "—", "{value:?}");
        }
    }

    #[test]
    fn a_call_in_flight_counts_seconds_then_minutes_and_seconds() {
        assert_eq!(elapsed(0.4), "0 s");
        assert_eq!(elapsed(59.9), "59 s");
        assert_eq!(elapsed(60.0), "1 min 00 s");
        assert_eq!(elapsed(125.0), "2 min 05 s");
        assert_eq!(elapsed(-3.0), "0 s", "a clock that stepped back never shows a negative time");
    }

    #[test]
    fn how_long_ago_uses_the_largest_unit_that_fits() {
        assert_eq!(ago(1.9), "just now");
        assert_eq!(ago(2.0), "2 s ago");
        assert_eq!(ago(59.0), "59 s ago");
        assert_eq!(ago(60.0), "1 min ago");
        assert_eq!(ago(3599.0), "59 min ago");
        assert_eq!(ago(3600.0), "1 h ago");
        assert_eq!(ago(86_400.0 * 3.0), "3 d ago");
    }

    #[test]
    fn counts_are_grouped_in_thousands() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(count(-1234), "-1,234");
    }

    #[test]
    fn outcome_names_interruptions_and_errors_before_the_status_code() {
        let record = |aborted: bool, status: i32| RequestRecord { aborted, status, ..RequestRecord::default() };
        assert_eq!(outcome(&record(true, 200)), "Interrupted");
        assert_eq!(outcome(&record(false, 500)), "Error 500");
        assert_eq!(outcome(&record(false, 200)), "200");
        assert_eq!(outcome(&record(false, 0)), "Status unavailable");
    }

    #[test]
    fn an_unrecognised_source_is_called_unknown_not_guessed() {
        let record = |name: Option<&str>| RequestRecord { source: name.map(str::to_string), ..RequestRecord::default() };
        assert_eq!(source(&record(Some("log"))), "Session log");
        assert_eq!(source(&record(Some("network"))), "Passive network estimate");
        assert_eq!(source(&record(Some("proxy"))), "Observed proxy");
        assert_eq!(source(&record(Some("import"))), "Unknown / legacy source");
        assert_eq!(source(&record(None)), "Unknown / legacy source");
    }
}
