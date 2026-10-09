//! The Live flyout: what is being generated right now, and which harness is being watched.
//! It opens above the notification area and closes when focus moves elsewhere.

use super::format;
use super::{card, colors, text, Link};
use crate::app::ipc::{Snapshot, ToTray};
use eframe::egui::{
    self, Align, Align2, Frame, Layout, Margin, RichText, Stroke, ViewportBuilder, ViewportCommand,
};
use speedtracker::time::Time;
use std::time::{Duration, Instant};

const WIDTH: f32 = 390.0;
const INITIAL_HEIGHT: f32 = 560.0;
const MARGIN: i8 = 16;
// Gap between the flyout and the edge of the work area, in pixels.
const EDGE: f32 = 12.0;

pub fn viewport(work: Option<[i32; 4]>, scale: f32) -> ViewportBuilder {
    let mut viewport = ViewportBuilder::default()
        .with_title("Speed Tracker — Live")
        .with_inner_size([WIDTH, INITIAL_HEIGHT])
        .with_decorations(false)
        .with_resizable(false)
        .with_taskbar(false)
        .with_always_on_top();
    if let Some(position) = work.map(|work| corner(work, INITIAL_HEIGHT, scale)) {
        viewport = viewport.with_position(position);
    }
    viewport
}

// Where the flyout's top-left goes so it sits in the bottom-right corner of the work area, in points.
fn corner(work: [i32; 4], height: f32, scale: f32) -> egui::Pos2 {
    let [left, top, right, bottom] = work.map(|edge| edge as f32);
    let x = (left + EDGE).max(right - WIDTH * scale - EDGE);
    let y = (top + EDGE).max(bottom - height * scale - EDGE);
    egui::pos2(x / scale, y / scale)
}

// What a call's state is called on screen. A session log can only say that a reply is due, so its
// "Waiting" lasts until the whole reply has arrived, not just until the first token.
fn phase_label(phase: &str) -> &str {
    match phase {
        "Waiting" => "reply in progress",
        other => other,
    }
}

// Cuts a long model name to fit its column, on a character boundary.
fn shorten(name: &str, limit: usize) -> String {
    if name.chars().count() <= limit {
        return name.to_string();
    }
    name.chars()
        .take(limit - 1)
        .chain(std::iter::once('…'))
        .collect()
}

#[derive(PartialEq)]
enum Tab {
    Live,
    Settings,
}

pub struct Live {
    link: Link,
    tab: Tab,
    work: Option<[i32; 4]>,
    was_focused: bool,
    // Rendering to a picture: do not close when focus is elsewhere.
    stay_open: bool,
    // The user asked for the flyout to stay while they work in another window.
    pinned: bool,
    // The choice just made, shown until the tray confirms it.
    chosen_target: Option<(Option<String>, Instant)>,
    confirm_enhanced: bool,
    notice: Option<String>,
}

impl Live {
    pub fn new(link: Link, tab: &str, work: Option<[i32; 4]>, stay_open: bool) -> Live {
        Live {
            stay_open,
            link,
            tab: if tab == "settings" {
                Tab::Settings
            } else {
                Tab::Live
            },
            work,
            pinned: false,
            was_focused: false,
            chosen_target: None,
            confirm_enhanced: false,
            notice: None,
        }
    }

    fn target(&mut self, snapshot: &Snapshot) -> Option<String> {
        if let Some((chosen, at)) = &self.chosen_target {
            if *chosen != snapshot.target && at.elapsed() < Duration::from_secs(2) {
                return chosen.clone();
            }
            self.chosen_target = None;
        }
        snapshot.target.clone()
    }

    fn live_tab(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let colors = colors(ui);
        let target = self.target(snapshot);
        text(ui, "Watching", 12.0, true);
        let mut names: Vec<String> = snapshot
            .harnesses
            .iter()
            .map(|status| status.name.clone())
            .chain(target.clone())
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        names.dedup_by_key(|name| name.to_lowercase());
        let mut choice = target.clone();
        egui::ComboBox::from_id_salt("target")
            .width(240.0)
            .selected_text(
                target
                    .clone()
                    .unwrap_or_else(|| "Auto · all harnesses".into()),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut choice, None, "Auto · all harnesses");
                for name in &names {
                    ui.selectable_value(&mut choice, Some(name.clone()), name);
                }
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::ComboBox,
                    true,
                    "Live target, independent of dashboard filters",
                )
            });
        if choice != target {
            self.link.send(ToTray::SetTarget(choice.clone()));
            self.chosen_target = Some((choice, Instant::now()));
        }

        let mut calls: Vec<_> = snapshot
            .active
            .iter()
            .filter(|call| {
                target
                    .as_ref()
                    .is_none_or(|target| call.harness.eq_ignore_ascii_case(target))
            })
            .collect();
        calls.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        let call = calls.first().copied();
        let now = Time::now();
        let status = match (call, &target) {
            (Some(call), _) => format!(
                "● {} · {} · {}",
                call.harness,
                phase_label(&call.phase),
                format::elapsed(now.since(call.started_at))
            ),
            (None, None) => "○ Idle · no model call in progress".into(),
            (None, Some(target)) => format!("○ Idle · no model call in {target}"),
        };
        ui.label(RichText::new(status).size(13.0).color(if call.is_some() {
            colors.accent
        } else {
            colors.muted
        }));

        // The one number: how fast the reply is arriving where that can be seen, and otherwise how
        // fast the last one was. It is held between replies and never falls back to zero.
        let latest = snapshot.held_record.as_ref();
        let streaming = call.filter(|call| {
            call.rate
                .is_some_and(|rate| rate.is_finite() && rate >= 0.0)
        });
        card(ui, 14, |ui| {
            ui.set_width(ui.available_width());
            let model = call
                .map(|call| call.model.as_str())
                .or(latest.map(|record| record.model.as_str()));
            text(ui, model.unwrap_or("No reply measured yet"), 13.0, true);
            let (speed, detail, footnote) = match streaming {
                Some(call) => (
                    format::number(call.rate, " tok/s", call.estimated),
                    format!(
                        "TTFT {} · {} tokens so far",
                        format::number(call.ttft, " s", call.estimated),
                        call.output_tokens
                            .map_or("—".into(), |tokens| format!(
                                "{}{}",
                                if call.estimated { "~" } else { "" },
                                format::count(tokens.into())
                            ))
                    ),
                    "Arriving now".to_string(),
                ),
                None => (
                    format::number(snapshot.held_rate, " tok/s", snapshot.held_rate_estimated),
                    format!(
                        "TTFT {}{}",
                        format::number(snapshot.held_ttft, " s", snapshot.held_ttft_estimated),
                        latest
                            .filter(|record| format::speed(record).is_some())
                            .map_or(String::new(), |record| format!(
                                " · {}{} tokens in {}",
                                if record.tokens_estimated { "~" } else { "" },
                                format::count(record.output_tokens.into()),
                                format::number(record.generation, " s", false)
                            ))
                    ),
                    match latest {
                        Some(record) => format!(
                            "Last reply · {} · {}",
                            record.harness,
                            format::ago(now.since(record.started_at.add_seconds(record.total)))
                        ),
                        None => "Speed appears here when a reply completes".into(),
                    },
                ),
            };
            text(ui, speed, 34.0, false);
            text(ui, detail, 13.0, false);
            text(ui, footnote, 12.0, true);
        });

        if !snapshot.recent.is_empty() {
            text(ui, "Recent replies", 12.0, true);
            egui::Grid::new("recent")
                .num_columns(4)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    // A grid sizes its columns to their text; a wrapping label would be squeezed instead.
                    let cell = |ui: &mut egui::Ui, content: String, color| {
                        ui.add(
                            egui::Label::new(RichText::new(content).size(12.0).color(color))
                                .extend(),
                        );
                    };
                    for record in &snapshot.recent {
                        cell(
                            ui,
                            format::local(record.started_at, "%H:%M:%S"),
                            colors.muted,
                        );
                        cell(ui, shorten(&record.model, 20), colors.foreground);
                        cell(
                            ui,
                            format::number(record.tps, " tok/s", record.tokens_estimated),
                            colors.foreground,
                        );
                        cell(
                            ui,
                            format!(
                                "TTFT {}",
                                format::number(format::latency(record), " s", false)
                            ),
                            colors.muted,
                        );
                        ui.end_row();
                    }
                });
        }
        text(
            ui,
            if snapshot.enhanced_enabled {
                "“~” is an estimate from network byte counts while a reply streams; the figure settles on the session log's own count when the reply completes."
            } else {
                "Measured from each harness's session log when a reply completes. Harnesses write nothing while a reply is streaming; to watch speed during a reply, turn on the network collector in Settings."
            },
            11.0,
            true,
        );
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        text(ui, "Standard-user collection", 15.0, false);
        text(ui, "Readable local session telemetry is detected automatically. No credentials are read, no harness settings are changed, and no proxy is required. A source may expose completed usage but no live speed or TTFT.", 12.0, true);
        let mut enabled = snapshot.enhanced_enabled || snapshot.enhanced_pending;
        if ui
            .add_enabled(
                !snapshot.enhanced_pending,
                egui::Checkbox::new(&mut enabled, "Enable optional elevated network collector"),
            )
            .changed()
        {
            if enabled {
                self.confirm_enhanced = true;
            } else {
                self.link.send(ToTray::SetEnhanced(false));
            }
        }
        text(ui, "Shows speed while a reply is still streaming, and measures harnesses that keep no session logs. Windows gives per-connection byte counts only to administrators, so it asks for approval; Speed Tracker itself stays unprivileged and needs no restart. Byte-flow speeds are estimates, marked “~”, not exact token counts or verified endpoint attribution.", 12.0, true);
        let mut status = snapshot.network_status.clone();
        if !snapshot.enhanced_available {
            status.push_str(
                "\nNo enhanced collector is connected; standard-user session logs remain enabled.",
            );
        }
        text(ui, status, 12.0, true);
        text(ui, "Detected harnesses", 15.0, false);
        for status in &snapshot.harnesses {
            let mut line = format!(
                "{} · {}{}",
                status.name,
                if status.has_logs {
                    "readable telemetry"
                } else {
                    "no supported readable telemetry"
                },
                if status.is_running {
                    " · process running (not proof of activity)"
                } else {
                    ""
                }
            );
            if let Some(limitation) = status
                .limitation
                .as_deref()
                .filter(|limitation| !limitation.trim().is_empty())
            {
                line.push('\n');
                line.push_str(limitation);
            }
            text(ui, line, 11.0, true);
        }
        text(ui, "Windows controls whether this notification-area icon is shown or placed in the overflow menu. Closing these windows leaves collection running; use Quit in the tray menu to exit.", 11.0, true);
    }

    // A question or message shown over the flyout. Returns the button pressed.
    fn dialog(
        context: &egui::Context,
        title: &str,
        message: &str,
        buttons: &[&'static str],
    ) -> Option<&'static str> {
        let mut pressed = None;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .max_width(WIDTH - 70.0)
            .show(context, |ui| {
                text(ui, message, 13.0, false);
                ui.horizontal(|ui| {
                    for button in buttons {
                        if ui.button(*button).clicked() {
                            pressed = Some(*button);
                        }
                    }
                });
            });
        pressed
    }
}

impl eframe::App for Live {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.window_fill.to_normalized_gamma_f32()
    }

    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        match self.link.apply_requests(context).as_deref() {
            Some("settings") => self.tab = Tab::Settings,
            Some("live") => self.tab = Tab::Live,
            _ => {}
        }
        if let Some(notice) = self.link.take_notice() {
            self.notice = Some(notice);
        }
        let snapshot = self.link.snapshot();
        let dialog_open = self.confirm_enhanced || self.notice.is_some();
        if context.input(|input| input.key_pressed(egui::Key::Escape)) && !dialog_open {
            context.send_viewport_cmd(ViewportCommand::Close);
        }
        // A flyout goes away when the user turns to something else, except while Windows is asking
        // for approval or a message is waiting to be read.
        if context.input(|input| input.focused) {
            self.was_focused = true;
        } else if self.was_focused
            && !self.stay_open
            && !self.pinned
            && !snapshot.enhanced_pending
            && !dialog_open
        {
            context.send_viewport_cmd(ViewportCommand::Close);
        }

        let colors = super::palette(context.style().visuals.dark_mode);
        let frame = Frame::new()
            .fill(colors.panel)
            .stroke(Stroke::new(1.0_f32, colors.border))
            .inner_margin(Margin::same(MARGIN));
        let content = egui::CentralPanel::default()
            .frame(frame)
            .show(context, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            text(ui, "Speed Tracker", 17.0, false);
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(egui::Button::new("×").min_size(egui::vec2(30.0, 26.0)))
                                    .on_hover_text("Dismiss Live (Esc)")
                                    .clicked()
                                {
                                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                                }
                                ui.toggle_value(&mut self.pinned, "Keep open").on_hover_text(
                                    "Stay on screen while you work in another window",
                                );
                            });
                        });
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.tab, Tab::Live, "Live");
                            ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                        });
                        ui.separator();
                        match self.tab {
                            Tab::Live => self.live_tab(ui, &snapshot),
                            Tab::Settings => self.settings_tab(ui, &snapshot),
                        }
                        ui.add_space(4.0);
                        if ui
                            .add_sized(
                                [ui.available_width(), 30.0],
                                egui::Button::new("Open Dashboard"),
                            )
                            .clicked()
                        {
                            self.link.send(ToTray::OpenDashboard);
                            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                        }
                    })
                    .content_size
            })
            .inner;

        if self.confirm_enhanced {
            let answer = Self::dialog(
                context,
                "Optional enhanced collection",
                "Enable the optional elevated network collector? Windows may ask for administrator approval for the separate collector. Speed Tracker itself stays unprivileged, and session logs already work without elevation. Byte-flow speeds and provider attribution remain estimates; no proxy or harness configuration changes will be made.",
                &["Yes", "No"],
            );
            if let Some(answer) = answer {
                self.confirm_enhanced = false;
                if answer == "Yes" {
                    self.link.send(ToTray::SetEnhanced(true));
                }
            }
        } else if let Some(notice) = self.notice.clone() {
            if Self::dialog(context, "Speed Tracker", &notice, &["OK"]).is_some() {
                self.notice = None;
            }
        }

        // The elapsed time of a call in flight, and how long ago the last one ended, move on their own.
        context.request_repaint_after(Duration::from_millis(if snapshot.active.is_empty() {
            1000
        } else {
            250
        }));

        // The window is as tall as its content, up to the work area, and stays in the corner as it changes.
        let scale = context.pixels_per_point();
        let available = self.work.map_or(780.0, |[_, top, _, bottom]| {
            (bottom - top) as f32 / scale - 2.0 * EDGE
        });
        let wanted = (content.y + 2.0 * f32::from(MARGIN) + 2.0)
            .max(if dialog_open { 340.0 } else { 100.0 })
            .min(available.clamp(100.0, 780.0))
            .round();
        let current = context.input(|input| input.screen_rect().height());
        if (wanted - current).abs() > 1.0 {
            context.send_viewport_cmd(ViewportCommand::InnerSize(egui::vec2(WIDTH, wanted)));
            if let Some(work) = self.work {
                context
                    .send_viewport_cmd(ViewportCommand::OuterPosition(corner(work, wanted, scale)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{corner, phase_label, shorten};
    use eframe::egui::pos2;

    // A 1920 x 1032 work area: a full-HD display less a 48-pixel taskbar.
    const WORK: [i32; 4] = [0, 0, 1920, 1032];

    #[test]
    fn a_log_only_wait_is_called_a_reply_in_progress_and_other_phases_keep_their_names() {
        assert_eq!(phase_label("Waiting"), "reply in progress");
        assert_eq!(phase_label("Thinking"), "Thinking");
        assert_eq!(phase_label("Streaming · network estimate"), "Streaming · network estimate");
    }

    #[test]
    fn a_long_model_name_is_cut_to_its_column_with_an_ellipsis() {
        assert_eq!(shorten("claude-opus-5-5", 20), "claude-opus-5-5");
        assert_eq!(shorten("accounts/fireworks/models/llama", 20), "accounts/fireworks/…");
        assert_eq!(shorten("mô-hình-tiếng-việt-rất-dài", 10).chars().count(), 10, "cut on a character, not a byte");
    }

    #[test]
    fn the_flyout_sits_twelve_pixels_inside_the_bottom_right_corner() {
        // 390 wide and 500 tall at 100 %: left edge 1920 - 390 - 12, top edge 1032 - 500 - 12.
        assert_eq!(corner(WORK, 500.0, 1.0), pos2(1518.0, 520.0));
    }

    #[test]
    fn on_a_scaled_display_the_gap_stays_twelve_real_pixels() {
        // At 150 % the flyout is 585 x 750 pixels: left 1920 - 585 - 12 = 1323, top 1032 - 750 - 12 = 270,
        // which are 882 and 180 in points.
        assert_eq!(corner(WORK, 500.0, 1.5), pos2(882.0, 180.0));
    }

    #[test]
    fn a_second_monitor_to_the_left_keeps_its_own_origin() {
        // A work area from x = -1920 to 0: left edge 0 - 390 - 12.
        assert_eq!(corner([-1920, 0, 0, 1032], 500.0, 1.0), pos2(-402.0, 520.0));
    }

    #[test]
    fn a_work_area_smaller_than_the_flyout_pins_it_to_the_top_left_gap() {
        assert_eq!(corner([100, 50, 400, 350], 500.0, 1.0), pos2(112.0, 62.0));
    }
}
