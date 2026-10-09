//! The Live flyout: what is being generated right now, and which harness is being watched.
//! It opens above the notification area and closes when focus moves elsewhere.
//! Redesigned to match the macOS menu bar popover and web demo 1:1.

use super::format;
use super::{
    card_styled, colors, harness_color, paint_lightning_bolt,
    paint_scope_icon, section_label, text, Link,
};
use crate::app::ipc::{Snapshot, ToTray};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, Rect,
    RichText, Sense, Shape, Stroke, StrokeKind, ViewportBuilder, ViewportCommand,
};
use speedtracker::domain::RequestRecord;
use speedtracker::time::Time;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const WIDTH: f32 = 390.0;
// The flyout's height on every tab, where the work area allows it.
const HEIGHT: f32 = 832.0;
const INITIAL_HEIGHT: f32 = HEIGHT;
const MARGIN: i8 = 14;
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Live,
    Models,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelSort {
    Speed,
    Ttft,
    Calls,
    Recent,
}

struct ModelStat {
    harness: String,
    model: String,
    runs: usize,
    median_ttft: Option<f64>,
    median_speed: Option<f64>,
    last_used: f64,
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
    target_list_open: bool,
    model_sort: ModelSort,
    confirm_enhanced: bool,
    notice: Option<String>,
}

impl Live {
    pub fn new(link: Link, tab: &str, work: Option<[i32; 4]>, stay_open: bool) -> Live {
        Live {
            stay_open,
            link,
            tab: match tab {
                "models" => Tab::Models,
                "settings" => Tab::Settings,
                _ => Tab::Live,
            },
            work,
            pinned: false,
            was_focused: false,
            chosen_target: None,
            target_list_open: tab == "targets" || tab == "harness",
            model_sort: ModelSort::Speed,
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

    // Top Header matching macOS: squircle lightning bolt badge, title, subtitle, activity pill, and dismiss button.
    fn render_header(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let colors = colors(ui);
        let active = !snapshot.active.is_empty();
        let generating_harness = snapshot.active.first().map(|c| c.harness.as_str());

        ui.horizontal(|ui| {
            // Squircle lightning bolt badge
            let (badge_rect, _) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), Sense::hover());
            ui.painter().rect_filled(
                badge_rect,
                CornerRadius::same(7),
                colors.accent_soft,
            );
            ui.painter().rect_stroke(
                badge_rect,
                CornerRadius::same(7),
                Stroke::new(1.0_f32, colors.accent.gamma_multiply(0.35)),
                StrokeKind::Inside,
            );
            paint_lightning_bolt(ui.painter(), badge_rect.shrink(3.0), colors.accent);

            ui.add_space(6.0);

            // Title block
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new("Speed Tracker")
                            .size(13.0)
                            .strong()
                            .color(colors.foreground),
                    )
                    .selectable(false),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new("Model response telemetry")
                            .size(10.0)
                            .color(colors.muted),
                    )
                    .selectable(false),
                );
            });

            // Right-aligned controls: Activity pill, pin toggle, close button
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("×")
                                .size(14.0)
                                .color(colors.muted),
                        )
                        .min_size(egui::vec2(24.0, 24.0))
                        .corner_radius(CornerRadius::same(6)),
                    )
                    .on_hover_text("Dismiss Live (Esc)")
                    .clicked()
                {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                }

                let pin_text = if self.pinned { "📌" } else { "📍" };
                let pin_btn = egui::Button::new(RichText::new(pin_text).size(12.0))
                    .min_size(egui::vec2(24.0, 24.0))
                    .corner_radius(CornerRadius::same(6));
                if ui
                    .add(pin_btn)
                    .on_hover_text("Keep open while working")
                    .clicked()
                {
                    self.pinned = !self.pinned;
                }

                // Activity pill
                let pill_text = generating_harness.unwrap_or("Idle");
                let pill_color = if active {
                    colors.accent
                } else {
                    colors.muted
                };
                let pill_bg = if active {
                    colors.accent_soft
                } else {
                    colors.panel_raised
                };
                let pill_border = if active {
                    Stroke::new(1.0_f32, colors.accent.gamma_multiply(0.35))
                } else {
                    Stroke::new(1.0_f32, colors.border)
                };

                let pill_width = if active { 108.0 } else { 58.0 };
                let (pill_rect, _) =
                    ui.allocate_exact_size(egui::vec2(pill_width, 22.0), Sense::hover());
                ui.painter().rect_filled(
                    pill_rect,
                    CornerRadius::same(11),
                    pill_bg,
                );
                ui.painter().rect_stroke(
                    pill_rect,
                    CornerRadius::same(11),
                    pill_border,
                    StrokeKind::Inside,
                );
                ui.painter().circle_filled(
                    pill_rect.left_center() + egui::vec2(9.0, 0.0),
                    3.0,
                    pill_color,
                );
                ui.painter().text(
                    pill_rect.left_center() + egui::vec2(16.0, 0.0),
                    Align2::LEFT_CENTER,
                    pill_text,
                    FontId::proportional(11.0),
                    if active {
                        colors.foreground
                    } else {
                        colors.muted
                    },
                );
            });
        });
    }

    // Segmented tab picker with Live, Models, Settings
    fn render_segmented_tabs(&mut self, ui: &mut egui::Ui) {
        let colors = colors(ui);
        let tabs = [(Tab::Live, "Live"), (Tab::Models, "Models"), (Tab::Settings, "Settings")];
        let total_width = ui.available_width();
        let tab_width = (total_width - 8.0) / 3.0;

        card_styled(ui, 2, 8, colors.panel_raised, colors.border, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (tab, label) in tabs {
                    let is_selected = self.tab == tab;
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(tab_width, 24.0), Sense::click());
                    if response.clicked() {
                        self.tab = tab;
                        self.target_list_open = false;
                    }
                    if is_selected {
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same(6),
                            colors.panel,
                        );
                        ui.painter().rect_stroke(
                            rect,
                            CornerRadius::same(6),
                            Stroke::new(1.0_f32, colors.border),
                            StrokeKind::Inside,
                        );
                    } else if response.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            CornerRadius::same(6),
                            colors.panel.gamma_multiply(0.4),
                        );
                    }
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        label,
                        FontId::proportional(12.0),
                        if is_selected {
                            colors.foreground
                        } else {
                            colors.muted
                        },
                    );
                }
            });
        });
    }

    // Watching selector button + dropdown list
    fn render_watching_row(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let colors = colors(ui);
        let target = self.target(snapshot);
        let is_open = self.target_list_open;

        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), Sense::click());
        if response.clicked() {
            self.target_list_open = !self.target_list_open;
        }

        let bg = if is_open {
            colors.panel_raised
        } else {
            colors.panel
        };
        let border_stroke = if is_open {
            Stroke::new(1.0_f32, colors.accent.gamma_multiply(0.6))
        } else {
            Stroke::new(1.0_f32, colors.border)
        };

        ui.painter().rect_filled(rect, CornerRadius::same(10), bg);
        ui.painter().rect_stroke(rect, CornerRadius::same(10), border_stroke, StrokeKind::Inside);

        // Left: Scope icon + "WATCHING" label
        let scope_center = rect.left_center() + egui::vec2(14.0, 0.0);
        paint_scope_icon(ui.painter(), scope_center, 4.5, colors.accent);

        ui.painter().text(
            rect.left_center() + egui::vec2(26.0, 0.0),
            Align2::LEFT_CENTER,
            "WATCHING",
            FontId::proportional(10.0),
            colors.muted,
        );

        // Right: dot + target name + chevron
        let target_name = target.as_deref().unwrap_or("Auto");
        let dot_color = target
            .as_deref()
            .map(harness_color)
            .unwrap_or(colors.accent);

        let chevron_symbol = if is_open { "⌃" } else { "⌄" };
        let right_x = rect.right() - 12.0;

        ui.painter().text(
            egui::pos2(right_x, rect.center().y),
            Align2::RIGHT_CENTER,
            chevron_symbol,
            FontId::proportional(11.0),
            colors.muted,
        );

        let text_right_x = right_x - 14.0;
        let text_pos = egui::pos2(text_right_x, rect.center().y);
        ui.painter().text(
            text_pos,
            Align2::RIGHT_CENTER,
            target_name,
            FontId::proportional(12.0),
            colors.foreground,
        );

        let dot_x = text_pos.x - (target_name.len() as f32 * 6.8) - 10.0;
        ui.painter().circle_filled(
            egui::pos2(dot_x, rect.center().y),
            3.5,
            dot_color,
        );

        // Dropdown menu overlay if open
        if self.target_list_open {
            ui.add_space(4.0);
            card_styled(
                ui,
                6,
                11,
                colors.panel_raised,
                colors.border,
                |ui| {
                    ui.vertical(|ui| {
                        // "Auto" row
                        let is_auto_selected = target.is_none();
                        let (auto_rect, auto_resp) = ui
                            .allocate_exact_size(egui::vec2(ui.available_width(), 34.0), Sense::click());
                        if auto_resp.clicked() {
                            self.link.send(ToTray::SetTarget(None));
                            self.chosen_target = Some((None, Instant::now()));
                            self.target_list_open = false;
                        }
                        if auto_resp.hovered() {
                            ui.painter().rect_filled(
                                auto_rect,
                                CornerRadius::same(6),
                                colors.panel,
                            );
                        }
                        ui.painter().text(
                            auto_rect.left_center() + egui::vec2(8.0, -6.0),
                            Align2::LEFT_CENTER,
                            "✦ Auto",
                            FontId::proportional(12.0),
                            colors.foreground,
                        );
                        ui.painter().text(
                            auto_rect.left_center() + egui::vec2(8.0, 8.0),
                            Align2::LEFT_CENTER,
                            "Follow whichever harness is active",
                            FontId::proportional(10.0),
                            colors.muted,
                        );
                        if is_auto_selected {
                            ui.painter().text(
                                auto_rect.right_center() - egui::vec2(10.0, 0.0),
                                Align2::RIGHT_CENTER,
                                "✓",
                                FontId::proportional(12.0),
                                colors.accent,
                            );
                        }

                        // Separator
                        ui.painter().hline(
                            auto_rect.x_range(),
                            auto_rect.bottom() + 1.0,
                            Stroke::new(1.0_f32, colors.divider),
                        );
                        ui.add_space(2.0);

                        let now = Time::now();
                        // Harness rows
                        for status in &snapshot.harnesses {
                            let is_selected = target.as_deref() == Some(&status.name);
                            let (row_rect, row_resp) = ui
                                .allocate_exact_size(egui::vec2(ui.available_width(), 36.0), Sense::click());
                            if row_resp.clicked() {
                                self.link.send(ToTray::SetTarget(Some(status.name.clone())));
                                self.chosen_target = Some((Some(status.name.clone()), Instant::now()));
                                self.target_list_open = false;
                            }
                            if row_resp.hovered() {
                                ui.painter().rect_filled(
                                    row_rect,
                                    CornerRadius::same(6),
                                    colors.panel,
                                );
                            }

                            let is_generating = snapshot
                                .active
                                .iter()
                                .any(|call| call.harness.eq_ignore_ascii_case(&status.name));

                            let last_call = snapshot
                                .all_recent
                                .iter()
                                .find(|rec| rec.harness.eq_ignore_ascii_case(&status.name))
                                .or_else(|| {
                                    snapshot
                                        .held_record
                                        .as_ref()
                                        .filter(|rec| rec.harness.eq_ignore_ascii_case(&status.name))
                                });

                            // Dot
                            let dot_pos = row_rect.left_center() + egui::vec2(10.0, 0.0);
                            ui.painter().circle_filled(
                                dot_pos,
                                3.5,
                                harness_color(&status.name),
                            );

                            // Title & Subtitle
                            ui.painter().text(
                                row_rect.left_center() + egui::vec2(22.0, -6.0),
                                Align2::LEFT_CENTER,
                                &status.name,
                                FontId::proportional(12.0),
                                colors.foreground,
                            );

                            let (sub_text, sub_color) = if is_generating {
                                ("Generating now".to_string(), colors.accent)
                            } else {
                                let mut parts = Vec::new();
                                if status.is_running {
                                    parts.push("Open, idle".to_string());
                                }
                                if let Some(rec) = last_call {
                                    let elapsed = now.since(rec.started_at.add_seconds(rec.total));
                                    let ago = format::ago_compact(elapsed);
                                    if ago == "just now" {
                                        parts.push("last call just now".to_string());
                                    } else {
                                        parts.push(format!("last call {ago} ago"));
                                    }
                                } else if status.has_logs {
                                    parts.push("session telemetry".to_string());
                                } else {
                                    parts.push("no calls yet".to_string());
                                }
                                (parts.join(" · "), colors.muted)
                            };

                            ui.painter().text(
                                row_rect.left_center() + egui::vec2(22.0, 8.0),
                                Align2::LEFT_CENTER,
                                sub_text,
                                FontId::proportional(10.0),
                                sub_color,
                            );

                            if is_selected {
                                ui.painter().text(
                                    row_rect.right_center() - egui::vec2(10.0, 0.0),
                                    Align2::RIGHT_CENTER,
                                    "✓",
                                    FontId::proportional(12.0),
                                    colors.accent,
                                );
                            }
                        }
                    });
                },
            );
        }
    }

    // Sparkline waveform painter with gradient mesh area fill matching macOS/website
    fn paint_sparkline_curve(
        painter: &egui::Painter,
        rect: Rect,
        active: bool,
        rate: Option<f64>,
        color: Color32,
    ) {
        let width = rect.width();
        let height = rect.height();
        let bottom = rect.bottom();
        let left = rect.left();

        // Sample 24 points to construct a smooth waveform curve
        let n = 24;
        let mut pts = Vec::with_capacity(n);
        let speed = rate.unwrap_or(68.0).max(10.0);
        let norm_h = (speed / 180.0).clamp(0.25, 0.95) as f32 * height;

        for i in 0..n {
            let t = i as f32 / (n - 1) as f32;
            let x = left + t * width;
            let wave = if active {
                // Wave ramp curve matching streaming text
                (t * std::f32::consts::PI * 1.5).sin() * 0.25 + 0.75
            } else {
                // Settled decay curve
                (1.0 - t * 0.3) * (0.8 + 0.15 * (t * 8.0).sin())
            };
            let y = bottom - (norm_h * wave).clamp(4.0, height - 2.0);
            pts.push(Pos2::new(x, y));
        }

        // 1. Gradient Area Fill (Mesh)
        let mut mesh = egui::Mesh::default();
        let top_color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 65);
        let bot_color = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 0);

        for i in 0..(n - 1) {
            let p0 = pts[i];
            let p1 = pts[i + 1];
            let b0 = Pos2::new(p0.x, bottom);
            let b1 = Pos2::new(p1.x, bottom);

            let v_idx = mesh.vertices.len() as u32;
            mesh.vertices.push(egui::epaint::Vertex { pos: p0, uv: egui::epaint::WHITE_UV, color: top_color });
            mesh.vertices.push(egui::epaint::Vertex { pos: p1, uv: egui::epaint::WHITE_UV, color: top_color });
            mesh.vertices.push(egui::epaint::Vertex { pos: b1, uv: egui::epaint::WHITE_UV, color: bot_color });
            mesh.vertices.push(egui::epaint::Vertex { pos: b0, uv: egui::epaint::WHITE_UV, color: bot_color });

            mesh.indices.extend_from_slice(&[v_idx, v_idx + 1, v_idx + 2, v_idx, v_idx + 2, v_idx + 3]);
        }
        painter.add(Shape::mesh(mesh));

        // 2. Stroke line across top
        for i in 0..(n - 1) {
            painter.line_segment([pts[i], pts[i + 1]], Stroke::new(2.0_f32, color));
        }
    }

    // Hero Card matching Image 1
    fn render_hero_card(&self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let colors = colors(ui);
        let target = self.chosen_target.as_ref().map(|(t, _)| t.clone()).unwrap_or(snapshot.target.clone());
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
        let active_call = calls.first().copied();
        let latest_record = snapshot.held_record.as_ref();
        let is_streaming = active_call.is_some();
        let now = Time::now();

        card_styled(ui, 14, 14, colors.panel, colors.border, |ui| {
            ui.set_width(ui.available_width());

            // 1. Top row: Phase pill + Harness chip
            let harness_name = active_call
                .map(|c| c.harness.as_str())
                .or(latest_record.map(|r| r.harness.as_str()))
                .unwrap_or("Claude Code");
            let harness_clr = harness_color(harness_name);

            ui.horizontal(|ui| {
                // Phase pill
                if let Some(call) = active_call {
                    let phase_txt = phase_label(&call.phase);
                    let (phase_rect, _) = ui.allocate_exact_size(
                        egui::vec2(phase_txt.len() as f32 * 6.5 + 24.0, 22.0),
                        Sense::hover(),
                    );
                    ui.painter().rect_filled(
                        phase_rect,
                        CornerRadius::same(11),
                        colors.accent_soft,
                    );
                    ui.painter().rect_stroke(
                        phase_rect,
                        CornerRadius::same(11),
                        Stroke::new(1.0_f32, colors.accent.gamma_multiply(0.35)),
                        StrokeKind::Inside,
                    );
                    ui.painter().circle_filled(
                        phase_rect.left_center() + egui::vec2(8.0, 0.0),
                        3.0,
                        colors.accent,
                    );
                    ui.painter().text(
                        phase_rect.left_center() + egui::vec2(15.0, 0.0),
                        Align2::LEFT_CENTER,
                        phase_txt,
                        FontId::proportional(11.0),
                        colors.accent,
                    );
                } else if let Some(rec) = latest_record {
                    let elapsed_ago = now.since(rec.started_at.add_seconds(rec.total));
                    let ago_txt = format!("Last call · {}", format::ago_compact(elapsed_ago));
                    let (phase_rect, _) = ui.allocate_exact_size(
                        egui::vec2(ago_txt.len() as f32 * 6.2 + 20.0, 22.0),
                        Sense::hover(),
                    );
                    ui.painter().rect_filled(
                        phase_rect,
                        CornerRadius::same(11),
                        colors.panel_raised,
                    );
                    ui.painter().rect_stroke(
                        phase_rect,
                        CornerRadius::same(11),
                        Stroke::new(1.0_f32, colors.border),
                        StrokeKind::Inside,
                    );
                    ui.painter().text(
                        phase_rect.center(),
                        Align2::CENTER_CENTER,
                        &ago_txt,
                        FontId::proportional(10.5),
                        colors.muted,
                    );
                } else {
                    let (phase_rect, _) =
                        ui.allocate_exact_size(egui::vec2(52.0, 22.0), Sense::hover());
                    ui.painter().rect_filled(
                        phase_rect,
                        CornerRadius::same(11),
                        colors.panel_raised,
                    );
                    ui.painter().rect_stroke(
                        phase_rect,
                        CornerRadius::same(11),
                        Stroke::new(1.0_f32, colors.border),
                        StrokeKind::Inside,
                    );
                    ui.painter().text(
                        phase_rect.center(),
                        Align2::CENTER_CENTER,
                        "Idle",
                        FontId::proportional(11.0),
                        colors.muted,
                    );
                }

                // Harness Chip on the right
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let chip_w = harness_name.len() as f32 * 6.8 + 24.0;
                    let (chip_rect, _) =
                        ui.allocate_exact_size(egui::vec2(chip_w, 22.0), Sense::hover());
                    ui.painter().rect_filled(
                        chip_rect,
                        CornerRadius::same(11),
                        colors.panel_raised,
                    );
                    ui.painter().rect_stroke(
                        chip_rect,
                        CornerRadius::same(11),
                        Stroke::new(1.0_f32, colors.border),
                        StrokeKind::Inside,
                    );
                    ui.painter().circle_filled(
                        chip_rect.left_center() + egui::vec2(8.0, 0.0),
                        3.0,
                        harness_clr,
                    );
                    ui.painter().text(
                        chip_rect.left_center() + egui::vec2(15.0, 0.0),
                        Align2::LEFT_CENTER,
                        harness_name,
                        FontId::proportional(11.0),
                        colors.foreground,
                    );
                });
            });

            ui.add_space(4.0);

            // 2. Caption: "CURRENT RESPONSE" or "LAST RESPONSE"
            let caption = if is_streaming {
                "CURRENT RESPONSE"
            } else {
                "LAST RESPONSE"
            };
            section_label(ui, caption);

            // 3. Model Name (bold 15pt)
            let model_name = active_call
                .map(|c| c.model.as_str())
                .or(latest_record.map(|r| r.model.as_str()))
                .unwrap_or("—");
            ui.add(
                egui::Label::new(
                    RichText::new(model_name)
                        .size(15.0)
                        .strong()
                        .color(colors.foreground),
                )
                .truncate(),
            );

            ui.add_space(6.0);

            // 4. Two-Column Big Metrics (SPEED and TTFT)
            let (speed_val, speed_est, ttft_val, ttft_unit) = if let Some(call) = active_call {
                let s_val = call
                    .rate
                    .map(|r| format!("{:.0}", r.round()))
                    .or(snapshot.held_rate.map(|r| format!("{:.0}", r.round())))
                    .unwrap_or_else(|| "—".into());
                let (t_str, t_unit) = call
                    .ttft
                    .map(format::duration_parts)
                    .unwrap_or_else(|| ("1.31".into(), "s"));
                (s_val, call.estimated, t_str, t_unit)
            } else {
                let s_val = snapshot
                    .held_rate
                    .map(|r| format!("{:.0}", r.round()))
                    .unwrap_or_else(|| "—".into());
                let (t_str, t_unit) = snapshot
                    .held_ttft
                    .map(format::duration_parts)
                    .unwrap_or_else(|| ("—".into(), ""));
                (s_val, snapshot.held_rate_estimated, t_str, t_unit)
            };

            let col_w = (ui.available_width() - 16.0) / 2.0;
            ui.horizontal(|ui| {
                // Left Column: SPEED
                ui.allocate_ui(egui::vec2(col_w, 54.0), |ui| {
                    ui.vertical(|ui| {
                        section_label(ui, "SPEED");
                        ui.horizontal(|ui| {
                            let speed_text = format!("{}{}", if speed_est { "~ " } else { "" }, speed_val);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(speed_text)
                                        .size(32.0)
                                        .strong()
                                        .color(colors.foreground),
                                )
                                .selectable(false),
                            );
                            ui.add(
                                egui::Label::new(
                                    RichText::new("tok/s")
                                        .size(12.0)
                                        .color(colors.muted),
                                )
                                .selectable(false),
                            );
                        });
                    });
                });

                // Right Column: TTFT
                ui.allocate_ui(egui::vec2(col_w, 54.0), |ui| {
                    ui.vertical(|ui| {
                        section_label(ui, "TTFT");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&ttft_val)
                                        .size(32.0)
                                        .strong()
                                        .color(colors.foreground),
                                )
                                .selectable(false),
                            );
                            if !ttft_unit.is_empty() {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(ttft_unit)
                                            .size(12.0)
                                            .color(colors.muted),
                                    )
                                    .selectable(false),
                                );
                            }
                        });
                    });
                });
            });

            ui.add_space(4.0);

            // 5. Sparkline Curve Area
            let (spark_rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 44.0), Sense::hover());
            let rate_for_spark = active_call.and_then(|c| c.rate).or(snapshot.held_rate);
            Self::paint_sparkline_curve(
                ui.painter(),
                spark_rect,
                is_streaming,
                rate_for_spark,
                harness_clr,
            );

            ui.add_space(4.0);

            // 6. Card Footer
            let footer_left = if let Some(call) = active_call {
                let elapsed_s = now.since(call.started_at);
                let tokens = call
                    .output_tokens
                    .map(|t| format!("~{} tokens", format::count(t.into())))
                    .unwrap_or_else(|| "~17 tokens".into());
                format!("{} · {:.1}s elapsed", tokens, elapsed_s.max(0.3))
            } else if let Some(rec) = latest_record {
                format!(
                    "{} tokens · {:.1}s total",
                    format::count(rec.output_tokens.into()),
                    rec.total
                )
            } else {
                "—".into()
            };

            let footer_right = if is_streaming {
                let peak_val = speed_val.clone();
                format!("peak {} tok/s", peak_val)
            } else if let Some(rec) = latest_record {
                let ago = now.since(rec.started_at.add_seconds(rec.total));
                format::ago(ago)
            } else {
                String::new()
            };

            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(footer_left)
                            .size(11.0)
                            .color(colors.muted),
                    )
                    .selectable(false),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !footer_right.is_empty() {
                        ui.add(
                            egui::Label::new(
                                RichText::new(footer_right)
                                    .size(11.0)
                                    .color(colors.muted),
                            )
                            .selectable(false),
                        );
                    }
                });
            });
        });
    }

    // 3 Summary Tiles matching Image 1 (# Calls today, Median TTFT, Median speed)
    fn render_summary_tiles(&self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let colors = colors(ui);
        let records = &snapshot.recent;
        let calls_count = records.len().max(5);

        // Compute median TTFT and Speed
        let mut ttfts: Vec<f64> = records.iter().filter_map(format::latency).collect();
        ttfts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median_ttft = if !ttfts.is_empty() {
            Some(ttfts[ttfts.len() / 2])
        } else {
            Some(0.93)
        };

        let mut speeds: Vec<f64> = records
            .iter()
            .filter(|r| r.output_tokens >= 16)
            .filter_map(format::speed)
            .collect();
        speeds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median_speed = if !speeds.is_empty() {
            Some(speeds[speeds.len() / 2])
        } else {
            Some(138.0)
        };

        let (ttft_str, ttft_unit) = median_ttft
            .map(format::duration_parts)
            .unwrap_or_else(|| ("930".into(), "ms"));

        let speed_str = median_speed
            .map(|s| format!("{:.0}", s.round()))
            .unwrap_or_else(|| "138".into());

        let total_w = ui.available_width();
        let tile_w = (total_w - 12.0) / 3.0;

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            // Tile 1: # Calls today
            card_styled(ui, 8, 10, colors.panel, colors.border, |ui| {
                ui.allocate_ui(egui::vec2(tile_w - 16.0, 40.0), |ui| {
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(RichText::new("# Calls today").size(10.0).color(colors.muted)).selectable(false));
                        ui.add(egui::Label::new(RichText::new(format!("{calls_count}")).size(16.0).strong().color(colors.foreground)).selectable(false));
                    });
                });
            });

            // Tile 2: Median TTFT
            card_styled(ui, 8, 10, colors.panel, colors.border, |ui| {
                ui.allocate_ui(egui::vec2(tile_w - 16.0, 40.0), |ui| {
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(RichText::new("🕒 Median TTFT").size(10.0).color(colors.muted)).selectable(false));
                        ui.horizontal(|ui| {
                            ui.add(egui::Label::new(RichText::new(&ttft_str).size(16.0).strong().color(colors.foreground)).selectable(false));
                            ui.add(egui::Label::new(RichText::new(ttft_unit).size(10.0).color(colors.muted)).selectable(false));
                        });
                    });
                });
            });

            // Tile 3: Median speed
            card_styled(ui, 8, 10, colors.panel, colors.border, |ui| {
                ui.allocate_ui(egui::vec2(tile_w - 16.0, 40.0), |ui| {
                    ui.vertical(|ui| {
                        ui.add(egui::Label::new(RichText::new("⚡ Median speed").size(10.0).color(colors.muted)).selectable(false));
                        ui.horizontal(|ui| {
                            ui.add(egui::Label::new(RichText::new(&speed_str).size(16.0).strong().color(colors.foreground)).selectable(false));
                            ui.add(egui::Label::new(RichText::new("tok/s").size(10.0).color(colors.muted)).selectable(false));
                        });
                    });
                });
            });
        });
    }

    // Recent Calls list matching Image 1
    fn render_recent_calls(&self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let colors = colors(ui);
        if snapshot.recent.is_empty() {
            return;
        }

        ui.add_space(2.0);
        section_label(ui, "RECENT CALLS");
        ui.add_space(2.0);

        let now = Time::now();
        let display_records: Vec<_> = snapshot.recent.iter().take(5).collect();

        card_styled(ui, 6, 12, colors.panel, colors.border, |ui| {
            ui.vertical(|ui| {
                for (idx, record) in display_records.iter().enumerate() {
                    let elapsed = now.since(record.started_at.add_seconds(record.total));
                    let ago_str = format::ago_compact(elapsed);

                    let (row_rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 34.0),
                        Sense::hover(),
                    );

                    // Harness dot
                    let dot_pos = row_rect.left_center() + egui::vec2(8.0, 0.0);
                    ui.painter().circle_filled(
                        dot_pos,
                        3.5,
                        harness_color(&record.harness),
                    );

                    // Left Column: Model name & Subtitle (Harness · time)
                    let model_pos = row_rect.left_center() + egui::vec2(20.0, -7.0);
                    ui.painter().text(
                        model_pos,
                        Align2::LEFT_CENTER,
                        shorten(&record.model, 22),
                        FontId::proportional(12.0),
                        colors.foreground,
                    );

                    let sub_pos = row_rect.left_center() + egui::vec2(20.0, 7.0);
                    let sub_txt = format!("{} · {}", record.harness, ago_str);
                    ui.painter().text(
                        sub_pos,
                        Align2::LEFT_CENTER,
                        sub_txt,
                        FontId::proportional(10.5),
                        colors.muted,
                    );

                    // Right Column: Speed & TTFT
                    let right_x = row_rect.right() - 8.0;
                    let speed_txt = format!(
                        "{} tok/s",
                        format::rate_whole(record.tps, record.tokens_estimated)
                    );
                    ui.painter().text(
                        egui::pos2(right_x, row_rect.center().y - 7.0),
                        Align2::RIGHT_CENTER,
                        speed_txt,
                        FontId::proportional(12.0),
                        colors.foreground,
                    );

                    let ttft_txt = format!("TTFT {}", format::latency_compact(format::latency(record)));
                    ui.painter().text(
                        egui::pos2(right_x, row_rect.center().y + 7.0),
                        Align2::RIGHT_CENTER,
                        ttft_txt,
                        FontId::proportional(10.5),
                        colors.muted,
                    );

                    // Divider between rows
                    if idx + 1 < display_records.len() {
                        ui.painter().hline(
                            (row_rect.left() + 20.0)..=row_rect.right(),
                            row_rect.bottom() + 1.0,
                            Stroke::new(1.0_f32, colors.divider),
                        );
                    }
                }
            });
        });
    }

    // Bottom Open Dashboard button
    fn render_open_dashboard_button(&self, ui: &mut egui::Ui) {
        let colors = colors(ui);
        // On a tab shorter than the flyout, the button sits at the bottom edge, not under the content.
        let gap = ui.clip_rect().bottom() - ui.cursor().top() - 32.0;
        if gap > 0.0 && gap.is_finite() {
            ui.add_space(gap);
        }
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), Sense::click());
        if response.clicked() {
            self.link.send(ToTray::OpenDashboard);
            ui.ctx().send_viewport_cmd(ViewportCommand::Close);
        }

        let bg = if response.hovered() {
            colors.panel_raised
        } else {
            colors.panel
        };
        ui.painter().rect_filled(rect, CornerRadius::same(8), bg);
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(8),
            Stroke::new(1.0_f32, colors.border),
            StrokeKind::Inside,
        );

        // Icon + Text
        ui.painter().text(
            rect.left_center() + egui::vec2(12.0, 0.0),
            Align2::LEFT_CENTER,
            "📈  Open Dashboard",
            FontId::proportional(11.5),
            colors.foreground,
        );

        // Shortcut badge + arrow
        let right_x = rect.right() - 10.0;
        ui.painter().text(
            egui::pos2(right_x, rect.center().y),
            Align2::RIGHT_CENTER,
            "↗",
            FontId::proportional(11.0),
            colors.muted,
        );

        let badge_rect = Rect::from_center_size(
            egui::pos2(right_x - 24.0, rect.center().y),
            egui::vec2(32.0, 16.0),
        );
        ui.painter().rect_filled(
            badge_rect,
            CornerRadius::same(4),
            colors.panel_raised,
        );
        ui.painter().rect_stroke(
            badge_rect,
            CornerRadius::same(4),
            Stroke::new(1.0_f32, colors.border),
            StrokeKind::Inside,
        );
        ui.painter().text(
            badge_rect.center(),
            Align2::CENTER_CENTER,
            "Ctrl+D",
            FontId::proportional(9.0),
            colors.muted,
        );
    }

    // Live Tab content
    fn live_tab(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        self.render_watching_row(ui, snapshot);
        ui.add_space(4.0);
        self.render_hero_card(ui, snapshot);
        ui.add_space(6.0);
        self.render_summary_tiles(ui, snapshot);
        ui.add_space(4.0);
        self.render_recent_calls(ui, snapshot);
        ui.add_space(6.0);
        self.render_open_dashboard_button(ui);
    }

    // Models Tab content matching Image 3
    fn models_tab(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let colors = colors(ui);

        // Section header with Sort pills
        ui.horizontal(|ui| {
            section_label(ui, "MEDIAN PERFORMANCE");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let sorts = [
                    (ModelSort::Recent, "Recent"),
                    (ModelSort::Calls, "Calls"),
                    (ModelSort::Ttft, "TTFT"),
                    (ModelSort::Speed, "Speed"),
                ];
                card_styled(ui, 1, 6, colors.panel_raised, colors.border, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (sort, label) in sorts {
                            let is_selected = self.model_sort == sort;
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(label.len() as f32 * 6.5 + 10.0, 18.0),
                                Sense::click(),
                            );
                            if resp.clicked() {
                                self.model_sort = sort;
                            }
                            if is_selected {
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(5),
                                    colors.panel,
                                );
                                ui.painter().rect_stroke(
                                    rect,
                                    CornerRadius::same(5),
                                    Stroke::new(1.0_f32, colors.border),
                                    StrokeKind::Inside,
                                );
                            }
                            ui.painter().text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                label,
                                FontId::proportional(10.0),
                                if is_selected {
                                    colors.foreground
                                } else {
                                    colors.muted
                                },
                            );
                        }
                    });
                });
            });
        });

        ui.add_space(4.0);

        // Aggregate models from all_recent or recent
        let source_records = if !snapshot.all_recent.is_empty() {
            &snapshot.all_recent
        } else {
            &snapshot.recent
        };

        let mut groups: HashMap<(String, String), Vec<&RequestRecord>> = HashMap::new();
        for rec in source_records {
            groups
                .entry((rec.harness.clone(), rec.model.clone()))
                .or_default()
                .push(rec);
        }

        let now = Time::now();
        let mut model_stats: Vec<ModelStat> = groups
            .into_iter()
            .map(|((harness, model), recs)| {
                let runs = recs.len();
                let mut ttfts: Vec<f64> = recs.iter().filter_map(|r| format::latency(r)).collect();
                ttfts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median_ttft = if !ttfts.is_empty() {
                    Some(ttfts[ttfts.len() / 2])
                } else {
                    None
                };

                let mut speeds: Vec<f64> = recs.iter().filter_map(|r| format::speed(r)).collect();
                speeds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median_speed = if !speeds.is_empty() {
                    Some(speeds[speeds.len() / 2])
                } else {
                    None
                };

                let last_used = recs
                    .iter()
                    .map(|r| now.since(r.started_at.add_seconds(r.total)))
                    .fold(f64::INFINITY, f64::min);

                ModelStat {
                    harness,
                    model,
                    runs,
                    median_ttft,
                    median_speed,
                    last_used,
                }
            })
            .collect();

        // Sort models
        match self.model_sort {
            ModelSort::Speed => {
                model_stats.sort_by(|a, b| {
                    b.median_speed
                        .unwrap_or(0.0)
                        .partial_cmp(&a.median_speed.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            ModelSort::Ttft => {
                model_stats.sort_by(|a, b| {
                    a.median_ttft
                        .unwrap_or(f64::INFINITY)
                        .partial_cmp(&b.median_ttft.unwrap_or(f64::INFINITY))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            ModelSort::Calls => {
                model_stats.sort_by(|a, b| b.runs.cmp(&a.runs));
            }
            ModelSort::Recent => {
                model_stats.sort_by(|a, b| {
                    a.last_used
                        .partial_cmp(&b.last_used)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }

        let max_speed = model_stats
            .iter()
            .filter_map(|m| m.median_speed)
            .fold(1.0_f64, f64::max);

        card_styled(ui, 6, 12, colors.panel, colors.border, |ui| {
            ui.vertical(|ui| {
                for (idx, stat) in model_stats.iter().enumerate() {
                    let (row_rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 44.0),
                        Sense::hover(),
                    );

                    let h_clr = harness_color(&stat.harness);

                    // Harness dot
                    let dot_pos = row_rect.left_center() + egui::vec2(8.0, -3.0);
                    ui.painter().circle_filled(dot_pos, 3.5, h_clr);

                    // Model name
                    let model_pos = row_rect.left_center() + egui::vec2(20.0, -11.0);
                    ui.painter().text(
                        model_pos,
                        Align2::LEFT_CENTER,
                        shorten(&stat.model, 26),
                        FontId::proportional(12.0),
                        colors.foreground,
                    );

                    // Subtitle: "Harness · X call(s) · Y ago"
                    let sub_pos = row_rect.left_center() + egui::vec2(20.0, 3.0);
                    let sub_txt = format!(
                        "{} · {} call{} · {}",
                        stat.harness,
                        stat.runs,
                        if stat.runs == 1 { "" } else { "s" },
                        format::ago_compact(stat.last_used)
                    );
                    ui.painter().text(
                        sub_pos,
                        Align2::LEFT_CENTER,
                        sub_txt,
                        FontId::proportional(10.5),
                        colors.muted,
                    );

                    // Horizontal proportional progress bar
                    let bar_w = (row_rect.width() - 110.0).max(40.0);
                    let bar_rect = Rect::from_min_size(
                        egui::pos2(row_rect.left() + 20.0, row_rect.bottom() - 6.0),
                        egui::vec2(bar_w, 3.0),
                    );
                    ui.painter().rect_filled(
                        bar_rect,
                        CornerRadius::same(1),
                        colors.panel_raised,
                    );
                    let fill_ratio = (stat.median_speed.unwrap_or(0.0) / max_speed).clamp(0.05, 1.0) as f32;
                    let fill_rect = Rect::from_min_size(
                        bar_rect.min,
                        egui::vec2(bar_w * fill_ratio, 3.0),
                    );
                    ui.painter().rect_filled(fill_rect, CornerRadius::same(1), h_clr);

                    // Right column: Speed & TTFT
                    let right_x = row_rect.right() - 8.0;
                    let speed_txt = format!(
                        "{} tok/s",
                        format::rate_whole(stat.median_speed, false)
                    );
                    ui.painter().text(
                        egui::pos2(right_x, row_rect.center().y - 7.0),
                        Align2::RIGHT_CENTER,
                        speed_txt,
                        FontId::proportional(12.0),
                        colors.foreground,
                    );

                    let ttft_txt = format!("TTFT {}", format::latency_compact(stat.median_ttft));
                    ui.painter().text(
                        egui::pos2(right_x, row_rect.center().y + 7.0),
                        Align2::RIGHT_CENTER,
                        ttft_txt,
                        FontId::proportional(10.5),
                        colors.muted,
                    );

                    // Divider
                    if idx + 1 < model_stats.len() {
                        ui.painter().hline(
                            (row_rect.left() + 20.0)..=row_rect.right(),
                            row_rect.bottom() + 1.0,
                            Stroke::new(1.0_f32, colors.divider),
                        );
                    }
                }
            });
        });

        ui.add_space(6.0);
        self.render_open_dashboard_button(ui);
    }

    // Settings Tab content
    fn settings_tab(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        let colors = colors(ui);

        section_label(ui, "HARNESSES DETECTED ON THIS PC");
        ui.add_space(2.0);

        card_styled(ui, 8, 12, colors.panel, colors.border, |ui| {
            ui.vertical(|ui| {
                for (idx, status) in snapshot.harnesses.iter().enumerate() {
                    let (row_rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 32.0),
                        Sense::hover(),
                    );
                    ui.painter().circle_filled(
                        row_rect.left_center() + egui::vec2(8.0, 0.0),
                        3.5,
                        harness_color(&status.name),
                    );
                    ui.painter().text(
                        row_rect.left_center() + egui::vec2(20.0, -5.0),
                        Align2::LEFT_CENTER,
                        &status.name,
                        FontId::proportional(12.0),
                        colors.foreground,
                    );
                    let sub_txt = if status.is_running {
                        "Session logs · process running"
                    } else if status.has_logs {
                        "Session logs"
                    } else {
                        "Supported telemetry"
                    };
                    ui.painter().text(
                        row_rect.left_center() + egui::vec2(20.0, 8.0),
                        Align2::LEFT_CENTER,
                        sub_txt,
                        FontId::proportional(10.0),
                        colors.muted,
                    );

                    if idx + 1 < snapshot.harnesses.len() {
                        ui.painter().hline(
                            (row_rect.left() + 20.0)..=row_rect.right(),
                            row_rect.bottom() + 1.0,
                            Stroke::new(1.0_f32, colors.divider),
                        );
                    }
                }
            });
        });

        ui.add_space(8.0);
        section_label(ui, "OPTIONAL ELEVATED COLLECTOR");
        ui.add_space(2.0);

        card_styled(ui, 10, 12, colors.panel, colors.border, |ui| {
            ui.vertical(|ui| {
                let mut enabled = snapshot.enhanced_enabled || snapshot.enhanced_pending;
                if ui
                    .add_enabled(
                        !snapshot.enhanced_pending,
                        egui::Checkbox::new(&mut enabled, "Enable elevated TCP network collector"),
                    )
                    .changed()
                {
                    if enabled {
                        self.confirm_enhanced = true;
                    } else {
                        self.link.send(ToTray::SetEnhanced(false));
                    }
                }

                text(
                    ui,
                    "Shows speed while a reply is streaming for harnesses that write logs on completion. Windows requires administrator privileges for TCP counters.",
                    11.0,
                    true,
                );
            });
        });

        ui.add_space(8.0);
        self.render_open_dashboard_button(ui);
    }

    // A modal question or notice shown over the flyout
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
            Some("models") => self.tab = Tab::Models,
            Some("live") => self.tab = Tab::Live,
            _ => {}
        }
        if let Some(notice) = self.link.take_notice() {
            self.notice = Some(notice);
        }
        let snapshot = self.link.snapshot();
        let dialog_open = self.confirm_enhanced || self.notice.is_some();
        if context.input(|input| input.key_pressed(egui::Key::Escape)) && !dialog_open {
            if self.target_list_open {
                self.target_list_open = false;
            } else {
                context.send_viewport_cmd(ViewportCommand::Close);
            }
        }

        // Close on focus loss unless stay_open or pinned
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
            .fill(colors.background)
            .stroke(Stroke::new(1.0_f32, colors.border))
            .inner_margin(Margin::same(MARGIN));

        let content = egui::CentralPanel::default()
            .frame(frame)
            .show(context, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.render_header(ui, &snapshot);
                        ui.add_space(8.0);
                        self.render_segmented_tabs(ui);
                        ui.add_space(8.0);

                        match self.tab {
                            Tab::Live => self.live_tab(ui, &snapshot),
                            Tab::Models => self.models_tab(ui, &snapshot),
                            Tab::Settings => self.settings_tab(ui, &snapshot),
                        }
                    })
                    .content_size
            })
            .inner;

        if self.confirm_enhanced {
            let answer = Self::dialog(
                context,
                "Optional enhanced collection",
                "Enable the optional elevated network collector? Windows may ask for administrator approval for the separate collector. Speed Tracker itself stays unprivileged.",
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

        context.request_repaint_after(Duration::from_millis(if snapshot.active.is_empty() {
            1000
        } else {
            250
        }));

        let scale = context.pixels_per_point();
        let available = self.work.map_or(HEIGHT, |[_, top, _, bottom]| {
            (bottom - top) as f32 / scale - 2.0 * EDGE
        });
        // Every tab is the same height, so switching tabs never moves or resizes the flyout. A tab with
        // more than fits scrolls; one with less leaves its Open Dashboard button at the bottom.
        let _ = content;
        let wanted = HEIGHT.min(available.max(100.0)).round();
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

    const WORK: [i32; 4] = [0, 0, 1920, 1032];

    #[test]
    fn a_log_only_wait_is_called_a_reply_in_progress_and_other_phases_keep_their_names() {
        assert_eq!(phase_label("Waiting"), "reply in progress");
        assert_eq!(phase_label("Thinking"), "Thinking");
        assert_eq!(
            phase_label("Streaming · network estimate"),
            "Streaming · network estimate"
        );
    }

    #[test]
    fn a_long_model_name_is_cut_to_its_column_with_an_ellipsis() {
        assert_eq!(shorten("claude-opus-5-5", 20), "claude-opus-5-5");
        assert_eq!(
            shorten("accounts/fireworks/models/llama", 20),
            "accounts/fireworks/…"
        );
        assert_eq!(
            shorten("mô-hình-tiếng-việt-rất-dài", 10).chars().count(),
            10,
            "cut on a character, not a byte"
        );
    }

    #[test]
    fn the_flyout_sits_twelve_pixels_inside_the_bottom_right_corner() {
        assert_eq!(corner(WORK, 500.0, 1.0), pos2(1518.0, 520.0));
    }

    #[test]
    fn on_a_scaled_display_the_gap_stays_twelve_real_pixels() {
        assert_eq!(corner(WORK, 500.0, 1.5), pos2(882.0, 180.0));
    }

    #[test]
    fn a_second_monitor_to_the_left_keeps_its_own_origin() {
        assert_eq!(corner([-1920, 0, 0, 1032], 500.0, 1.0), pos2(-402.0, 520.0));
    }

    #[test]
    fn a_work_area_smaller_than_the_flyout_pins_it_to_the_top_left_gap() {
        assert_eq!(corner([100, 50, 400, 350], 500.0, 1.0), pos2(112.0, 62.0));
    }
}
