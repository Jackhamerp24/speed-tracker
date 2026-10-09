//! The dashboard's two charts: a p50/p95 trend over time and a histogram of measurements.
//! Hover, or focus and use ← / →, to read one bucket.

use super::format::{self, SHORT};
use super::{colors, Palette};
use eframe::egui::{self, Align2, FontId, Key, Rect, Sense, Shape, Stroke};
use speedtracker::domain::DashboardBucket;

const HEIGHT: f32 = 210.0;

/// Which bucket of a chart is being inspected. Lives across frames.
#[derive(Default)]
pub struct Selection(Option<usize>);

struct Chart<'a> {
    ui: &'a egui::Ui,
    area: Rect,
    colors: Palette,
}

impl Chart<'_> {
    fn label(&self, text: &str, x: f32, y: f32, anchor: Align2) {
        self.ui.painter().text(
            egui::pos2(x, y),
            anchor,
            text,
            FontId::proportional(11.0),
            self.colors.muted,
        );
    }

    // Horizontal grid lines with their values on the left.
    fn grid(&self, scale: f64, whole: bool) {
        for tick in 0..=3 {
            let y = self.area.bottom() - self.area.height() * tick as f32 / 3.0;
            self.ui.painter().hline(
                self.area.x_range(),
                y,
                Stroke::new(1.0_f32, self.colors.border),
            );
            let value = scale * f64::from(tick) / 3.0;
            let text = if whole {
                format!("{value:.0}")
            } else {
                format!("{value:.2}")
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_string()
            };
            self.label(&text, self.area.left() - 6.0, y, Align2::RIGHT_CENTER);
        }
    }
}

// Lays out a chart, moves the selection with the pointer or the arrow keys, and draws the focus ring.
fn frame<'a>(
    ui: &'a mut egui::Ui,
    count: usize,
    selection: &mut Selection,
    description: &str,
    nearest: impl Fn(f32) -> usize,
) -> (Chart<'a>, egui::Response) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), HEIGHT), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, description));
    if response.clicked() {
        response.request_focus();
    }
    let area = Rect::from_min_max(
        rect.min + egui::vec2(58.0, 14.0),
        egui::pos2(
            (rect.right() - 18.0).max(rect.left() + 59.0),
            (rect.bottom() - 40.0).max(rect.top() + 15.0),
        ),
    );
    let focused = response.has_focus();
    if count == 0 {
        selection.0 = None;
    } else if let Some(pointer) = response.hover_pos() {
        selection.0 = Some(
            nearest(((pointer.x - area.left()) / area.width()).clamp(0.0, 1.0)).min(count - 1),
        );
    } else if focused {
        let step = ui.input(|input| {
            i64::from(input.key_pressed(Key::ArrowRight))
                - i64::from(input.key_pressed(Key::ArrowLeft))
        });
        if step != 0 {
            selection.0 = Some(
                (selection.0.map_or(0, |index| index as i64 + step)).clamp(0, count as i64 - 1)
                    as usize,
            );
        }
    } else {
        selection.0 = None;
    }
    let colors = colors(ui);
    if focused {
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            2.0,
            Stroke::new(1.0_f32, colors.accent),
            egui::StrokeKind::Inside,
        );
    }
    (Chart { ui, area, colors }, response)
}

/// p50 (solid) and p95 (dashed) per time bucket. Buckets with no measurement leave a gap, not a zero.
pub fn trend(
    ui: &mut egui::Ui,
    buckets: &[DashboardBucket],
    speed: bool,
    selection: &mut Selection,
) {
    let unit = if speed { " tok/s" } else { " s" };
    let median = |bucket: &DashboardBucket| {
        if speed {
            bucket.summary.median_tps
        } else {
            bucket.summary.median_ttft
        }
    };
    let p95 = |bucket: &DashboardBucket| {
        if speed {
            bucket.summary.p95_tps
        } else {
            bucket.summary.p95_ttft
        }
    };
    let valid = |value: Option<f64>| value.filter(|number| number.is_finite() && *number >= 0.0);
    let maximum = buckets
        .iter()
        .flat_map(|bucket| [valid(median(bucket)), valid(p95(bucket))])
        .flatten()
        .fold(0.0, f64::max);
    let span = match (buckets.first(), buckets.last()) {
        (Some(first), Some(last)) => last.date.since(first.date),
        _ => 0.0,
    };
    // How far along the time axis a bucket sits, from 0 to 1.
    let along = |index: usize| {
        if buckets.len() <= 1 || span <= 0.0 {
            0.5
        } else {
            (buckets[index].date.since(buckets[0].date) / span) as f32
        }
    };
    let nearest = |ratio: f32| {
        (0..buckets.len())
            .min_by(|a, b| {
                (along(*a) - ratio)
                    .abs()
                    .total_cmp(&(along(*b) - ratio).abs())
            })
            .unwrap_or(0)
    };
    let description = format!("{} trend, p50 and p95, {} time buckets. Solid line is p50; dashed line is p95. Missing measurements are not zero.", if speed { "Generation speed" } else { "Time to first token" }, buckets.len());
    let (chart, response) = frame(ui, buckets.len(), selection, &description, nearest);
    let (area, colors, painter) = (chart.area, chart.colors, chart.ui.painter());
    let scale = if maximum > 0.0 { maximum } else { 1.0 };
    chart.grid(scale, false);
    if !buckets.iter().any(|bucket| valid(median(bucket)).is_some()) {
        chart.label(
            "No valid measurements in this selection",
            area.left() + 12.0,
            area.center().y,
            Align2::LEFT_CENTER,
        );
        return;
    }
    let point = |index: usize, value: f64| {
        egui::pos2(
            area.left() + along(index) * area.width(),
            area.bottom() - (value / scale) as f32 * area.height(),
        )
    };
    for (metric, color, dashed) in [
        (
            &median as &dyn Fn(&DashboardBucket) -> Option<f64>,
            colors.accent,
            false,
        ),
        (&p95, colors.secondary, true),
    ] {
        let mut previous = None;
        for (index, bucket) in buckets.iter().enumerate() {
            let Some(value) = valid(metric(bucket)) else {
                previous = None;
                continue;
            };
            let current = point(index, value);
            if let Some(previous) = previous {
                if dashed {
                    painter.extend(Shape::dashed_line(
                        &[previous, current],
                        Stroke::new(2.0_f32, color),
                        6.0,
                        4.0,
                    ));
                } else {
                    painter.line_segment([previous, current], Stroke::new(2.0_f32, color));
                }
            }
            painter.circle_filled(current, 2.5, color);
            previous = Some(current);
        }
    }
    chart.label(
        &format::local(buckets[0].date, "%b %-d %H:%M"),
        area.left(),
        area.bottom() + 8.0,
        Align2::LEFT_TOP,
    );
    if buckets.len() > 1 {
        chart.label(
            &format::local(buckets[buckets.len() - 1].date, "%b %-d %H:%M"),
            area.right(),
            area.bottom() + 8.0,
            Align2::RIGHT_TOP,
        );
    }
    if let Some(index) = selection.0.filter(|index| *index < buckets.len()) {
        let x = area.left() + along(index) * area.width();
        painter.extend(Shape::dashed_line(
            &[egui::pos2(x, area.top()), egui::pos2(x, area.bottom())],
            Stroke::new(1.0_f32, colors.muted),
            2.0,
            3.0,
        ));
        let bucket = &buckets[index];
        let samples = if speed {
            bucket.summary.speed_count
        } else {
            bucket.summary.latency_count
        };
        response.on_hover_text_at_pointer(format!(
            "{} · p50 {} · p95 {} · {samples} measured calls",
            format::local(bucket.date, SHORT),
            format::number(median(bucket), unit, false),
            format::number(p95(bucket), unit, false)
        ));
    }
}

/// A histogram of valid measurements. Invalid, interrupted and unavailable ones are left out.
pub fn distribution(
    ui: &mut egui::Ui,
    measurements: &[f64],
    unit: &str,
    selection: &mut Selection,
) {
    let values: Vec<f64> = measurements
        .iter()
        .copied()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .collect();
    let maximum = values.iter().copied().fold(0.0, f64::max);
    // About the square root of the sample size, capped at twelve bars.
    let mut counts = vec![
        0usize;
        if values.is_empty() {
            0
        } else {
            ((values.len() as f64).sqrt().ceil() as usize).clamp(1, 12)
        }
    ];
    for value in &values {
        let index = if maximum > 0.0 {
            ((value / maximum * counts.len() as f64) as usize).min(counts.len() - 1)
        } else {
            0
        };
        counts[index] += 1;
    }
    let bars = counts.len();
    let description = format!("Distribution of {} measurements in {unit}. Invalid, interrupted and unavailable measurements are excluded.", values.len());
    let (chart, response) = frame(ui, bars, selection, &description, |ratio| {
        (ratio * bars as f32) as usize
    });
    let (area, colors, painter) = (chart.area, chart.colors, chart.ui.painter());
    let tallest = counts.iter().copied().max().unwrap_or(0);
    let scale = if tallest > 0 { tallest as f64 } else { 1.0 };
    chart.grid(scale, true);
    if values.is_empty() {
        chart.label(
            "No valid measurements in this selection",
            area.left() + 12.0,
            area.center().y,
            Align2::LEFT_CENTER,
        );
        return;
    }
    let width = area.width() / bars as f32;
    for (index, count) in counts.iter().enumerate() {
        let height = (*count as f64 / scale) as f32 * area.height();
        let bar = Rect::from_min_max(
            egui::pos2(
                area.left() + index as f32 * width + 1.0,
                area.bottom() - height,
            ),
            egui::pos2(
                area.left() + (index + 1) as f32 * width - 1.0,
                area.bottom(),
            ),
        );
        painter.rect_filled(
            bar,
            0.0,
            if selection.0 == Some(index) {
                colors.secondary
            } else {
                colors.accent
            },
        );
    }
    chart.label(
        &format!("0 {unit}"),
        area.left(),
        area.bottom() + 8.0,
        Align2::LEFT_TOP,
    );
    chart.label(
        &format::number(Some(maximum), &format!(" {unit}"), false),
        area.right(),
        area.bottom() + 8.0,
        Align2::RIGHT_TOP,
    );
    if let Some(index) = selection.0.filter(|index| *index < bars) {
        let (lower, upper) = (
            maximum * index as f64 / bars as f64,
            maximum * (index + 1) as f64 / bars as f64,
        );
        response.on_hover_text_at_pointer(format!(
            "{}–{} {unit}: {} calls",
            format::number(Some(lower), "", false),
            format::number(Some(upper), "", false),
            format::count(counts[index] as i64)
        ));
    }
}
