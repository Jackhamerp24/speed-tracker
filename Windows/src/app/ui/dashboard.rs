//! The Dashboard: completed-call analytics read from history.jsonl. Its filters never change the
//! Live target, and live calls are never counted in its numbers.

use super::format::{self, SHORT};
use super::plot::{self, Selection};
use super::{card, colors, text, Link};
use crate::app::ipc::Snapshot;
use chrono::{Days, Local, NaiveDate, TimeZone};
use eframe::egui::{self, Align, Layout, RichText, Sense, ViewportBuilder, ViewportCommand};
use egui_extras::{Column, DatePickerButton, TableBuilder};
use speedtracker::domain::{DashboardFilter, DashboardReport, ProviderIdentity, RequestRecord};
use speedtracker::history::HistoryStore;
use speedtracker::time::Time;
use std::cmp::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use uuid::Uuid;

const PAGE_SIZE: usize = 75;
const RANGES: [&str; 5] = [
    "Last 24 hours",
    "Last 7 days",
    "Last 30 days",
    "All history",
    "Custom dates",
];
const CUSTOM: usize = 4;

pub fn viewport() -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title("Speed Tracker — Dashboard")
        .with_inner_size([1250.0, 850.0])
        .with_min_inner_size([1000.0, 680.0])
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Trends,
    Calls,
}

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Started,
    Harness,
    Provider,
    Model,
    Latency,
    Speed,
    Output,
    Outcome,
    Source,
}

const COLUMNS: [(&str, SortBy, f32); 9] = [
    ("Started (local)", SortBy::Started, 140.0),
    ("Harness", SortBy::Harness, 105.0),
    ("Provider", SortBy::Provider, 120.0),
    ("Model", SortBy::Model, 155.0),
    ("TTFT (s)", SortBy::Latency, 80.0),
    ("tok/s", SortBy::Speed, 75.0),
    ("Output", SortBy::Output, 75.0),
    ("Outcome", SortBy::Outcome, 120.0),
    ("Source", SortBy::Source, 150.0),
];

struct Row {
    record: Arc<RequestRecord>,
    // The provider's name, starred when the endpoint was not observed directly.
    provider: String,
}

type Loaded = Result<(Vec<Arc<RequestRecord>>, usize), String>;

pub struct Dashboard {
    link: Link,
    history: Arc<HistoryStore>,
    records: Vec<Arc<RequestRecord>>,
    skipped_lines: usize,
    loaded: bool,
    loader: Option<Receiver<Loaded>>,
    load_error: Option<String>,
    seen_revision: u64,
    range: usize,
    from: NaiveDate,
    through: NaiveDate,
    harness: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    report: Option<DashboardReport>,
    // Filters or records changed; the report must be rebuilt.
    dirty: bool,
    filter_error: Option<&'static str>,
    rows: Vec<Row>,
    latencies: Vec<f64>,
    speeds: Vec<f64>,
    page: usize,
    sort: (SortBy, bool),
    selected: Option<Uuid>,
    tab: Tab,
    plots: [Selection; 4],
}

fn caseless(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

// Local midnight at the start of a date, as an instant.
fn start_of_day(date: NaiveDate) -> Option<Time> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .earliest()
        .map(|local| Time::from_unix_seconds(local.timestamp()))
}

impl Dashboard {
    pub fn new(link: Link, tab: &str) -> Dashboard {
        let today = Local::now().date_naive();
        Dashboard {
            link,
            history: Arc::new(HistoryStore::new(None)),
            records: Vec::new(),
            skipped_lines: 0,
            loaded: false,
            loader: None,
            load_error: None,
            seen_revision: 0,
            range: 1,
            from: today.checked_sub_days(Days::new(7)).unwrap_or(today),
            through: today,
            harness: None,
            provider: None,
            model: None,
            report: None,
            dirty: false,
            filter_error: None,
            rows: Vec::new(),
            latencies: Vec::new(),
            speeds: Vec::new(),
            page: 0,
            sort: (SortBy::Started, false),
            selected: None,
            tab: match tab {
                "trends" => Tab::Trends,
                "calls" => Tab::Calls,
                _ => Tab::Overview,
            },
            plots: Default::default(),
        }
    }

    // Reads history off the UI thread; a long file must not freeze the window.
    fn reload(&mut self, context: &egui::Context) {
        if self.loader.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let (history, context) = (Arc::clone(&self.history), context.clone());
        std::thread::spawn(move || {
            let result = history
                .read(None)
                .map(|records| (records, history.skipped_lines()))
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
            context.request_repaint();
        });
        self.loader = Some(receiver);
    }

    fn receive(&mut self) {
        let Some(result) = self
            .loader
            .as_ref()
            .and_then(|loader| loader.try_recv().ok())
        else {
            return;
        };
        self.loader = None;
        match result {
            Ok((records, skipped)) => {
                self.records = records;
                self.skipped_lines = skipped;
                self.loaded = true;
                self.load_error = None;
                self.dirty = true;
            }
            Err(error) => self.load_error = Some(error),
        }
    }

    fn filter(&mut self) -> Option<DashboardFilter> {
        let now = Time::now();
        let (from, through) = match self.range {
            0 => (Some(now.add_days(-1.0)), Some(now)),
            1 => (Some(now.add_days(-7.0)), Some(now)),
            2 => (Some(now.add_days(-30.0)), Some(now)),
            CUSTOM => {
                // Both dates are inclusive, in local time.
                let window = (self.from <= self.through)
                    .then(|| {
                        Some((
                            start_of_day(self.from)?,
                            start_of_day(self.through.checked_add_days(Days::new(1))?)?,
                        ))
                    })
                    .flatten();
                let Some((from, through)) = window else {
                    self.filter_error = Some("Choose a valid start and end date; the end date must not precede the start.");
                    return None;
                };
                (Some(from), Some(through))
            }
            _ => (None, None),
        };
        self.filter_error = None;
        Some(DashboardFilter {
            from,
            through,
            harness: self.harness.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
        })
    }

    fn rebuild(&mut self) {
        self.dirty = false;
        let Some(filter) = self.filter() else { return };
        let report = DashboardReport::create(&self.records, &filter);
        self.latencies = report
            .records
            .iter()
            .filter_map(|record| format::latency(record))
            .collect();
        self.speeds = report
            .records
            .iter()
            .filter_map(|record| format::speed(record))
            .collect();
        self.rows = report
            .records
            .iter()
            .map(|record| {
                let identity = ProviderIdentity::from(record);
                Row {
                    record: Arc::clone(record),
                    provider: format!(
                        "{}{}",
                        identity.name,
                        if identity.unverified { " *" } else { "" }
                    ),
                }
            })
            .collect();
        self.report = Some(report);
        self.sort_rows();
    }

    // Sorts the whole filtered result, not just the visible page.
    fn sort_rows(&mut self) {
        let (by, ascending) = self.sort;
        let direction = |ordering: Ordering| {
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        };
        self.rows.sort_by(|first, second| {
            let (a, b) = (&first.record, &second.record);
            let ordering = match by {
                // Calls without the measurement go last in either direction.
                SortBy::Latency | SortBy::Speed => {
                    let value = |record: &RequestRecord| {
                        if by == SortBy::Latency {
                            format::latency(record)
                        } else {
                            format::speed(record)
                        }
                    };
                    match (value(a), value(b)) {
                        (None, None) => Ordering::Equal,
                        (None, Some(_)) => Ordering::Greater,
                        (Some(_), None) => Ordering::Less,
                        (Some(a), Some(b)) => direction(a.total_cmp(&b)),
                    }
                }
                SortBy::Started => direction(a.started_at.cmp(&b.started_at)),
                SortBy::Output => direction(a.output_tokens.cmp(&b.output_tokens)),
                SortBy::Harness => direction(caseless(&a.harness, &b.harness)),
                SortBy::Provider => direction(caseless(&first.provider, &second.provider)),
                SortBy::Model => direction(caseless(&a.model, &b.model)),
                SortBy::Outcome => direction(caseless(&format::outcome(a), &format::outcome(b))),
                SortBy::Source => direction(caseless(format::source(a), format::source(b))),
            };
            ordering.then_with(|| a.id.cmp(&b.id))
        });
        self.page = self.page.min(self.rows.len().saturating_sub(1) / PAGE_SIZE);
        if !self
            .rows
            .iter()
            .skip(self.page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .any(|row| Some(row.record.id) == self.selected)
        {
            self.selected = None;
        }
    }

    fn changed(&mut self) {
        self.page = 0;
        self.dirty = true;
    }

    fn header(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                text(ui, "Completed-call analytics", 24.0, false);
                text(ui, "Provider means endpoint or reported label, not model vendor. Dashboard filters never change the Live target.", 12.0, true);
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                if ui.add_enabled(self.loader.is_none(), egui::Button::new("Refresh history")).clicked() {
                    self.reload(ui.ctx());
                }
            });
        });
        let live = if snapshot.active.is_empty() {
            "Idle — no model call in progress".to_string()
        } else {
            snapshot
                .active
                .iter()
                .map(|call| {
                    format!(
                        "{}: {} / {} / {}",
                        call.harness,
                        call.phase,
                        call.model,
                        format::number(call.rate, " tok/s", call.estimated)
                    )
                })
                .collect::<Vec<_>>()
                .join(" · ")
        };
        card(ui, 9, |ui| {
            ui.set_width(ui.available_width());
            text(
                ui,
                format!(
                    "LIVE (not included in completed analytics) · {live}\nLive target: {} · {}",
                    snapshot.target.as_deref().unwrap_or("Auto"),
                    snapshot.network_status
                ),
                12.0,
                false,
            );
        });
        self.filters(ui);
    }

    fn filters(&mut self, ui: &mut egui::Ui) {
        // Each list offers only what has calls with the other two selections. A selection left
        // without calls, by a change of dates, stays visible so it can be cleared.
        let (harnesses, providers, models) = match &self.report {
            Some(report) => (
                report.harness_options.clone(),
                report
                    .provider_options
                    .iter()
                    .map(|identity| {
                        (
                            identity.key.clone(),
                            format!(
                                "{}{}",
                                identity.name,
                                if identity.unverified {
                                    " · unverified"
                                } else {
                                    ""
                                }
                            ),
                        )
                    })
                    .collect(),
                report.model_options.clone(),
            ),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let named = |names: Vec<String>| {
            names
                .into_iter()
                .map(|name| (name.clone(), name))
                .collect::<Vec<_>>()
        };
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.vertical(|ui| {
                ui.label("Period");
                let before = self.range;
                egui::ComboBox::from_id_salt("period")
                    .width(125.0)
                    .selected_text(RANGES[self.range])
                    .show_ui(ui, |ui| {
                        for (index, name) in RANGES.iter().enumerate() {
                            ui.selectable_value(&mut self.range, index, *name);
                        }
                    });
                changed |= before != self.range;
            });
            for (label, salt, date) in [
                ("From", "from", &mut self.from),
                ("Through", "through", &mut self.through),
            ] {
                ui.vertical(|ui| {
                    ui.label(label);
                    let before = *date;
                    ui.add_enabled(
                        self.range == CUSTOM,
                        DatePickerButton::new(date).id_salt(salt),
                    );
                    changed |= before != *date;
                });
            }
            for (label, all, options, selection) in [
                (
                    "Harness",
                    "All harnesses",
                    named(harnesses),
                    &mut self.harness,
                ),
                ("Provider", "All providers", providers, &mut self.provider),
                ("Model", "All models", named(models), &mut self.model),
            ] {
                ui.vertical(|ui| {
                    ui.label(label);
                    let before = selection.clone();
                    let shown = match selection.as_ref() {
                        None => all.to_string(),
                        Some(key) => options
                            .iter()
                            .find(|(option, _)| option == key)
                            .map_or_else(|| format!("{key} · no calls"), |(_, name)| name.clone()),
                    };
                    egui::ComboBox::from_id_salt(label)
                        .width(180.0)
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(selection, None, all);
                            for (key, name) in &options {
                                ui.selectable_value(selection, Some(key.clone()), name);
                            }
                            if let Some(key) = before
                                .as_ref()
                                .filter(|key| !options.iter().any(|(option, _)| option == *key))
                            {
                                ui.selectable_value(
                                    selection,
                                    Some(key.clone()),
                                    format!("{key} · no calls"),
                                );
                            }
                        });
                    changed |= before != *selection;
                });
            }
            ui.vertical(|ui| {
                ui.label("");
                if ui.button("Reset filters").clicked() {
                    self.range = 1;
                    self.harness = None;
                    self.provider = None;
                    self.model = None;
                    changed = true;
                }
            });
        });
        if changed {
            self.changed();
        }
    }

    fn overview(&mut self, ui: &mut egui::Ui) {
        let Some(report) = &self.report else { return };
        let summary = &report.summary;
        let mut drill = None;
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(8.0);
            let metrics = [
                ("Calls / output tokens", format::count(summary.count as i64), format!("{} output tokens", format::count(summary.output_tokens.into()))),
                ("Generation speed", format::number(summary.median_tps, " tok/s", false), format!("p95 {} · n={}", format::number(summary.p95_tps, " tok/s", false), summary.speed_count)),
                ("Time to first token", format::number(summary.median_ttft, " s", false), format!("p95 {} · n={}", format::number(summary.p95_ttft, " s", false), summary.latency_count)),
                ("Provenance / outcomes", format!("{} estimated", format::count(summary.estimated_count as i64)), format!("{} interrupted or errors", format::count(summary.interrupted_count as i64))),
            ];
            ui.columns(4, |columns| {
                for (column, (title, value, detail)) in columns.iter_mut().zip(metrics) {
                    card(column, 14, |ui| {
                        ui.set_width(ui.available_width());
                        text(ui, title, 12.0, true);
                        text(ui, value, 23.0, false);
                        text(ui, detail, 12.0, true);
                    });
                }
            });
            text(ui, "Provider × harness · select a cell to inspect its calls", 17.0, false);
            text(ui, "Endpoint attribution is separate from model identity. “Unverified” includes reported labels and passive estimates. A dash is unavailable, not zero.", 12.0, true);
            if report.groups.is_empty() {
                card(ui, 14, |ui| text(ui, "No completed calls match these filters. Live collection stays independent. Try All history or reset the filters.", 13.0, false));
                return;
            }
            let mut providers: Vec<&ProviderIdentity> = report.providers.iter().filter(|identity| report.groups.iter().any(|group| group.provider.key == identity.key)).collect();
            providers.sort_by(|a, b| caseless(&a.name, &b.name));
            let mut harnesses: Vec<&str> = report.groups.iter().map(|group| group.harness.as_str()).collect();
            harnesses.sort_by(|a, b| caseless(a, b));
            harnesses.dedup();
            egui::Grid::new("matrix").min_col_width(182.0).spacing([8.0, 8.0]).show(ui, |ui| {
                ui.label("");
                for harness in &harnesses {
                    text(ui, *harness, 13.0, false);
                }
                ui.end_row();
                for identity in providers {
                    text(ui, format!("{}\n{}", identity.name, if identity.unverified { "Unverified endpoint" } else { "Observed endpoint" }), 13.0, false).on_hover_text(&identity.evidence);
                    for harness in &harnesses {
                        let group = report.groups.iter().find(|group| group.provider.key == identity.key && group.harness == *harness);
                        let content = match group {
                            Some(group) => format!("{} calls\np50 {}\nTTFT {}", format::count(group.summary.count as i64), format::number(group.summary.median_tps, " tok/s", false), format::number(group.summary.median_ttft, " s", false)),
                            None => "No calls".into(),
                        };
                        let mut cell = ui.add_enabled(group.is_some(), egui::Button::new(RichText::new(content).size(13.0)).min_size(egui::vec2(182.0, 62.0)));
                        if let Some(group) = group {
                            cell = cell.on_hover_text(&group.provider.evidence);
                        }
                        if cell.clicked() {
                            drill = Some((identity.key.clone(), harness.to_string()));
                        }
                    }
                    ui.end_row();
                }
            });
            let mut sources: Vec<(&str, usize)> = Vec::new();
            for record in &report.records {
                let source = format::source(record);
                match sources.iter_mut().find(|(known, _)| *known == source) {
                    Some((_, count)) => *count += 1,
                    None => sources.push((source, 1)),
                }
            }
            sources.sort();
            text(ui, format!("Sources · {}", sources.iter().map(|(source, count)| format!("{source}: {}", format::count(*count as i64))).collect::<Vec<_>>().join(" · ")), 12.0, true);
            text(ui, "Percentiles use valid nonnegative measurements from non-error, non-interrupted calls. Speed requires a positive generation window; whole-request fallback is never presented as generation speed. Estimated observations are not exact measurements.", 12.0, true);
        });
        // Choosing a cell narrows the filters to it and shows its calls.
        if let Some((provider, harness)) = drill {
            self.provider = Some(provider);
            self.harness = Some(harness);
            self.tab = Tab::Calls;
            self.changed();
        }
    }

    fn trends(&mut self, ui: &mut egui::Ui) {
        let Some(report) = &self.report else { return };
        let summary = &report.summary;
        let [latency_trend, latency_distribution, speed_trend, speed_distribution] =
            &mut self.plots;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(8.0);
            text(
                ui,
                format!(
                    "TTFT: {} measured calls. Generation-window speed: {} measured calls. {} whole-request speed fallbacks excluded from speed percentiles. Errors / interruptions and invalid values are excluded from both distributions; estimates remain included and marked in Calls.",
                    format::count(summary.latency_count as i64),
                    format::count(summary.speed_count as i64),
                    format::count(summary.round_trip_count as i64)
                ),
                12.0,
                true,
            );
            ui.columns(2, |columns| {
                card(&mut columns[0], 14, |ui| {
                    text(ui, "TTFT · p50 / p95", 15.0, false);
                    plot::trend(ui, &report.trends, false, latency_trend);
                });
                card(&mut columns[1], 14, |ui| {
                    text(ui, "TTFT distribution · measured calls", 15.0, false);
                    plot::distribution(ui, &self.latencies, "s", latency_distribution);
                });
            });
            ui.columns(2, |columns| {
                card(&mut columns[0], 14, |ui| {
                    text(ui, "Generation speed · p50 / p95", 15.0, false);
                    plot::trend(ui, &report.trends, true, speed_trend);
                });
                card(&mut columns[1], 14, |ui| {
                    text(ui, "Speed distribution · measured calls", 15.0, false);
                    plot::distribution(ui, &self.speeds, "tok/s", speed_distribution);
                });
            });
            text(ui, "Solid: p50 · dashed: p95. R7 interpolation. Hover a chart, or click it and use ← / →, to inspect. Missing metrics are gaps, not zeros. Dates are shown in local time.", 12.0, true);
        });
    }

    fn calls(&mut self, ui: &mut egui::Ui) {
        let selected = self
            .selected
            .and_then(|id| self.rows.iter().find(|row| row.record.id == id))
            .map(|row| Arc::clone(&row.record));
        egui::SidePanel::right("inspector")
            .exact_width(315.0)
            .resizable(false)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| inspector(ui, selected.as_deref()));
            });
        let total = self.rows.len();
        egui::TopBottomPanel::bottom("pagination").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(self.page > 0, egui::Button::new("Previous"))
                    .clicked()
                {
                    self.page -= 1;
                }
                let label = if total == 0 {
                    "No calls".to_string()
                } else {
                    format!(
                        "Page {} of {} · {}–{} / {}",
                        self.page + 1,
                        total.div_ceil(PAGE_SIZE),
                        self.page * PAGE_SIZE + 1,
                        total.min((self.page + 1) * PAGE_SIZE),
                        format::count(total as i64)
                    )
                };
                text(ui, label, 12.0, true);
                if ui
                    .add_enabled(
                        (self.page + 1) * PAGE_SIZE < total,
                        egui::Button::new("Next"),
                    )
                    .clicked()
                {
                    self.page += 1;
                }
            });
        });
        let mut sort = None;
        let mut choose = None;
        egui::CentralPanel::default().show_inside(ui, |ui| {
            // The columns can be wider than the space beside the inspector; scroll sideways rather than hide any.
            egui::ScrollArea::horizontal()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut table = TableBuilder::new(ui)
                        .striped(true)
                        .resizable(true)
                        .sense(Sense::click())
                        .cell_layout(Layout::left_to_right(Align::Center));
                    for (_, _, width) in COLUMNS {
                        table = table.column(Column::initial(width).at_least(50.0).clip(true));
                    }
                    let (page, current) = (self.page, self.sort);
                    let visible: Vec<&Row> = self
                        .rows
                        .iter()
                        .skip(page * PAGE_SIZE)
                        .take(PAGE_SIZE)
                        .collect();
                    table
                        .header(28.0, |mut header| {
                            for (title, by, _) in COLUMNS {
                                header.col(|ui| {
                                    let arrow = if current.0 != by {
                                        ""
                                    } else if current.1 {
                                        " ▲"
                                    } else {
                                        " ▼"
                                    };
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new(format!("{title}{arrow}")).strong(),
                                            )
                                            .frame(false),
                                        )
                                        .on_hover_text("Sort all filtered calls by this column")
                                        .clicked()
                                    {
                                        sort = Some(by);
                                    }
                                });
                            }
                        })
                        .body(|body| {
                            body.rows(30.0, visible.len(), |mut row| {
                                let Row { record, provider } = visible[row.index()];
                                row.set_selected(Some(record.id) == self.selected);
                                let estimate = if record.tokens_estimated { "~" } else { "" };
                                let cells = [
                                    format::local(record.started_at, SHORT),
                                    record.harness.clone(),
                                    provider.clone(),
                                    record.model.clone(),
                                    format::number(format::latency(record), "", false),
                                    format::number(
                                        format::speed(record),
                                        "",
                                        record.tokens_estimated,
                                    ),
                                    format!(
                                        "{estimate}{}",
                                        format::count(record.output_tokens.into())
                                    ),
                                    format::outcome(record),
                                    format::source(record).to_string(),
                                ];
                                for cell in cells {
                                    row.col(|ui| {
                                        ui.add(egui::Label::new(cell).truncate().selectable(false));
                                    });
                                }
                                if row.response().clicked() {
                                    choose = Some(record.id);
                                }
                            });
                        });
                });
        });
        if let Some(by) = sort {
            // A second click on the same column reverses it.
            self.sort = (by, self.sort.0 == by && !self.sort.1);
            self.page = 0;
            self.sort_rows();
        }
        if let Some(id) = choose {
            self.selected = Some(id);
        }
    }
}

fn inspector(ui: &mut egui::Ui, record: Option<&RequestRecord>) {
    text(ui, "Call inspector", 17.0, false);
    let Some(record) = record else {
        text(ui, "Select a completed call with the mouse. Column headers sort the entire filtered result, not just this page.", 13.0, true);
        return;
    };
    let identity = ProviderIdentity::from(record);
    let optional = |count: Option<i32>| {
        count.map_or("unavailable".to_string(), |count| {
            format::count(count.into())
        })
    };
    let plain = |value: Option<f64>| format::number(value, "", false);
    let estimate = if record.tokens_estimated { "~" } else { "" };
    let speed = format::speed(record);
    let mut details = vec![
        (
            "Started",
            format::local(record.started_at, "%A, %B %-d, %Y %H:%M:%S"),
        ),
        (
            "Harness / model",
            format!("{}\n{}", record.harness, record.model),
        ),
        (
            "Provider / endpoint",
            format!(
                "{}\n{}\n{}",
                identity.name,
                identity
                    .host
                    .as_deref()
                    .unwrap_or("Endpoint host unavailable"),
                identity.evidence
            ),
        ),
        (
            "Observation",
            format!(
                "{}{}{}",
                format::source(record),
                if record.tokens_estimated {
                    " · estimated token counts"
                } else {
                    " · no token-estimate flag"
                },
                if matches!(record.source.as_deref(), Some("log" | "network" | "proxy")) {
                    ""
                } else {
                    "\nLegacy / unknown source: exactness is not established."
                }
            ),
        ),
        (
            "Outcome",
            format!(
                "{} · {}",
                format::outcome(record),
                if record.streamed {
                    "streamed"
                } else {
                    "non-streamed"
                }
            ),
        ),
        (
            "Generation speed",
            format!(
                "{}\n{}",
                format::number(speed, " tok/s", record.tokens_estimated),
                if speed.is_some() {
                    "Positive generation window; eligible for speed percentiles."
                } else {
                    "Not eligible for generation-speed percentiles. Missing timing, interruption or an error is not zero speed."
                }
            ),
        ),
        (
            "Recorded speed",
            format!(
                "{}{}",
                format::number(record.tps, " tok/s", record.tokens_estimated),
                if record.generation.is_none_or(|generation| generation == 0.0) {
                    " · whole-request fallback / generation timing unavailable"
                } else {
                    ""
                }
            ),
        ),
        (
            "Timing (seconds)",
            format!(
                "TTFT {}\nFirst visible {}\nFirst byte {}\nGeneration {}\nTotal request {}",
                plain(record.ttft),
                plain(record.first_visible),
                plain(record.ttfb),
                plain(record.generation),
                plain(Some(record.total))
            ),
        ),
        (
            "Tokens",
            format!(
                "Input {}\nCached input {}\nOutput {estimate}{}\nReasoning {}",
                optional(record.input_tokens),
                optional(record.cached_input_tokens),
                format::count(record.output_tokens.into()),
                optional(record.reasoning_tokens)
            ),
        ),
        ("API format", record.format.clone()),
        ("Record ID", record.id.to_string()),
    ];
    if let Some(key) = record
        .source_key
        .as_ref()
        .filter(|key| !key.trim().is_empty())
    {
        details.push(("Stable source key", key.clone()));
    }
    let foreground = colors(ui).foreground;
    for (label, value) in details {
        text(ui, label, 12.0, true);
        // Selectable, so a value can be copied.
        ui.add(
            egui::Label::new(RichText::new(value).color(foreground))
                .wrap()
                .selectable(true),
        );
        ui.add_space(6.0);
    }
}

impl eframe::App for Dashboard {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        match self.link.apply_requests(context).as_deref() {
            Some("trends") => self.tab = Tab::Trends,
            Some("calls") => self.tab = Tab::Calls,
            _ => {}
        }
        // The dashboard has nowhere to show the flyout's notices; drop them.
        let _ = self.link.take_notice();
        let snapshot = self.link.snapshot();
        if context.input(|input| input.key_pressed(egui::Key::Escape)) {
            context.send_viewport_cmd(ViewportCommand::Close);
        }
        self.receive();
        // Read history when the window opens, and again whenever the tray reports a new finished call.
        if !self.loaded && self.loader.is_none() && self.load_error.is_none() {
            self.reload(context);
        }
        if snapshot.history_revision != self.seen_revision && self.loader.is_none() {
            self.seen_revision = snapshot.history_revision;
            self.reload(context);
        }
        if self.dirty && self.loaded {
            self.rebuild();
        }

        egui::TopBottomPanel::top("header").show(context, |ui| {
            ui.add_space(12.0);
            self.header(ui, &snapshot);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(context, |ui| {
            let status = if let Some(error) = &self.load_error {
                format!("History could not be read: {error}. Use Refresh history to retry.")
            } else if let Some(error) = self.filter_error {
                error.to_string()
            } else if let (true, Some(report)) = (self.loaded, &self.report) {
                format!(
                    "{} calls in filter · {} estimated · {} interrupted / errors · {} malformed history lines skipped · {}",
                    format::count(report.summary.count as i64),
                    format::count(report.summary.estimated_count as i64),
                    format::count(report.summary.interrupted_count as i64),
                    format::count(self.skipped_lines as i64),
                    self.history.home().display()
                )
            } else {
                "Reading completed-call history…".to_string()
            };
            text(ui, status, 12.0, true);
        });
        egui::CentralPanel::default().show(context, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Overview, "Overview");
                ui.selectable_value(&mut self.tab, Tab::Trends, "Trends & distributions");
                ui.selectable_value(&mut self.tab, Tab::Calls, "Calls & inspector");
            });
            ui.separator();
            match self.tab {
                Tab::Overview => self.overview(ui),
                Tab::Trends => self.trends(ui),
                Tab::Calls => self.calls(ui),
            }
        });
        // Filters changed during this frame take effect in the next.
        if self.dirty {
            context.request_repaint();
        }
    }
}
