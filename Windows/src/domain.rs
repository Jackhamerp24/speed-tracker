use crate::time::Time;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

/// One finished model call: a line of history.jsonl. The schema is additive; old files must keep reading.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestRecord {
    pub id: Uuid,
    pub started_at: Time,
    pub harness: String,
    pub route: String,
    pub upstream_host: String,
    pub format: String,
    pub model: String,
    pub streamed: bool,
    pub status: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_visible: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<f64>,
    pub total: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<i32>,
    pub output_tokens: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<i32>,
    pub tokens_estimated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tps: Option<f64>,
    pub aborted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
}

impl Default for RequestRecord {
    fn default() -> Self {
        RequestRecord {
            id: Uuid::new_v4(),
            started_at: Time(0),
            harness: "Unknown".into(),
            route: String::new(),
            upstream_host: String::new(),
            format: "unknown".into(),
            model: "unknown".into(),
            streamed: false,
            status: 200,
            ttft: None,
            first_visible: None,
            ttfb: None,
            generation: None,
            total: 0.0,
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: 0,
            reasoning_tokens: None,
            tokens_estimated: false,
            tps: None,
            aborted: false,
            source: None,
            source_key: None,
        }
    }
}

impl RequestRecord {
    /// What makes two lines the same call: the source's own key when it has one, else the record id.
    pub fn dedup_key(&self) -> String {
        match self.source_key.as_deref() {
            Some(key) if !key.is_empty() => format!("source:{key}"),
            _ => format!("id:{}", self.id),
        }
    }

    /// Reads a history line. Property names match in any letter case and absent fields take their
    /// defaults, as the files written by earlier versions expect. A field of the wrong type refuses
    /// the whole line rather than guessing.
    pub fn from_json(value: &Value) -> Option<RequestRecord> {
        let object = value.as_object()?;
        if !object.contains_key("startedAt") || !object.contains_key("id") {
            return None;
        }
        let fields: HashMap<String, &Value> = object
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value))
            .collect();
        let present = |name: &str| fields.get(name).copied().filter(|value| !value.is_null());
        let text = |name: &str, default: &str| -> Option<String> {
            match present(name) {
                None => Some(default.to_string()),
                Some(value) => value.as_str().map(str::to_string),
            }
        };
        let optional_text = |name: &str| -> Option<Option<String>> {
            match present(name) {
                None => Some(None),
                Some(value) => value.as_str().map(|text| Some(text.to_string())),
            }
        };
        let flag = |name: &str| -> Option<bool> {
            match present(name) {
                None => Some(false),
                Some(value) => value.as_bool(),
            }
        };
        let integer = |value: &Value| value.as_i64().and_then(|number| i32::try_from(number).ok());
        let optional_int = |name: &str| -> Option<Option<i32>> {
            match present(name) {
                None => Some(None),
                Some(value) => integer(value).map(Some),
            }
        };
        let optional_number = |name: &str| -> Option<Option<f64>> {
            match present(name) {
                None => Some(None),
                Some(value) => value.as_f64().map(Some),
            }
        };
        let id = present("id")?
            .as_str()
            .filter(|text| text.len() == 36)
            .and_then(|text| Uuid::parse_str(text).ok())?;
        Some(RequestRecord {
            id,
            started_at: Time::parse(present("startedat")?.as_str()?)?,
            harness: text("harness", "Unknown")?,
            route: text("route", "")?,
            upstream_host: text("upstreamhost", "")?,
            format: text("format", "unknown")?,
            model: text("model", "unknown")?,
            streamed: flag("streamed")?,
            status: match present("status") {
                None => 200,
                Some(value) => integer(value)?,
            },
            ttft: optional_number("ttft")?,
            first_visible: optional_number("firstvisible")?,
            ttfb: optional_number("ttfb")?,
            generation: optional_number("generation")?,
            total: optional_number("total")?.unwrap_or(0.0),
            input_tokens: optional_int("inputtokens")?,
            cached_input_tokens: optional_int("cachedinputtokens")?,
            output_tokens: optional_int("outputtokens")?.unwrap_or(0),
            reasoning_tokens: optional_int("reasoningtokens")?,
            tokens_estimated: flag("tokensestimated")?,
            tps: optional_number("tps")?,
            aborted: flag("aborted")?,
            source: optional_text("source")?,
            source_key: optional_text("sourcekey")?,
        })
    }
}

/// Who served a call, as far as the evidence goes: an endpoint host or a label the harness reported.
/// Never a claim about the model's vendor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderIdentity {
    pub key: String,
    pub name: String,
    pub host: Option<String>,
    pub evidence: String,
    pub unverified: bool,
}

fn alias(label: &str) -> Option<(&'static str, &'static str)> {
    Some(match label {
        "anthropic" | "claude" => ("anthropic", "Anthropic"),
        "openai" | "open-ai" => ("openai", "OpenAI"),
        "openai-codex" | "openai_codex" | "chatgpt" | "codex" => ("openai-codex", "OpenAI Codex"),
        "google" | "gemini" | "google-gemini" | "google-vertex" | "vertex" | "vertexai" => {
            ("gemini", "Google Gemini")
        }
        "deepseek" => ("deepseek", "DeepSeek"),
        "openrouter" | "open-router" => ("openrouter", "OpenRouter"),
        "xai" | "x-ai" | "x.ai" => ("xai", "xAI"),
        "groq" => ("groq", "Groq"),
        "cerebras" => ("cerebras", "Cerebras"),
        "mistral" => ("mistral", "Mistral"),
        "moonshot" | "kimi" => ("moonshot", "Moonshot"),
        "zai" | "z-ai" | "z.ai" | "zhipu" => ("zai", "Z.ai"),
        "together" | "togetherai" | "together-ai" => ("together", "Together AI"),
        "fireworks" | "fireworks-ai" => ("fireworks", "Fireworks AI"),
        "dashscope" | "alibaba" | "qwen" => ("dashscope", "Alibaba DashScope"),
        "ollama" => ("ollama", "Ollama (reported local)"),
        "lmstudio" | "lm-studio" | "lm_studio" => ("lmstudio", "LM Studio (reported local)"),
        _ => return None,
    })
}

fn host_alias(host: &str) -> Option<&'static str> {
    Some(match host {
        "api.anthropic.com" => "anthropic",
        "api.openai.com" => "openai",
        "chatgpt.com" | "www.chatgpt.com" | "chat.openai.com" => "openai-codex",
        "generativelanguage.googleapis.com"
        | "cloudcode-pa.googleapis.com"
        | "aiplatform.googleapis.com" => "gemini",
        "api.deepseek.com" => "deepseek",
        "openrouter.ai" | "api.openrouter.ai" => "openrouter",
        "api.x.ai" => "xai",
        "api.groq.com" => "groq",
        "api.cerebras.ai" => "cerebras",
        "api.mistral.ai" => "mistral",
        "api.moonshot.ai" | "api.moonshot.cn" | "api.kimi.com" => "moonshot",
        "api.z.ai" | "open.bigmodel.cn" => "zai",
        "api.together.xyz" | "api.together.ai" => "together",
        "api.fireworks.ai" => "fireworks",
        "dashscope.aliyuncs.com" | "dashscope-intl.aliyuncs.com" => "dashscope",
        _ => return None,
    })
}

// A provider label, lower-cased; anything with characters a label would not have is not one.
fn label(text: &str) -> String {
    let value = text.trim().to_lowercase();
    if value
        .chars()
        .all(|c| c.is_alphanumeric() || "-_. ".contains(c))
    {
        value
    } else {
        String::new()
    }
}

// The host and non-default port of an endpoint, if the text names one rather than a label.
fn endpoint(text: &str) -> Option<(String, Option<u16>)> {
    let mut value = text.trim().to_string();
    if value.is_empty() {
        return None;
    }
    let scheme = value.contains("://");
    if !scheme
        && (alias(&label(&value)).is_some()
            || !(value.contains('.')
                || value.contains(':')
                || value.eq_ignore_ascii_case("localhost")))
    {
        return None;
    }
    if !scheme && value.matches(':').count() > 1 && !value.starts_with('[') {
        value = format!("[{value}]");
    }
    let url = url::Url::parse(&if scheme {
        value
    } else {
        format!("http://{value}")
    })
    .ok()?;
    let host = url.host_str().filter(|host| !host.is_empty())?;
    Some((host.to_string(), url.port()))
}

impl ProviderIdentity {
    pub fn from(record: &RequestRecord) -> ProviderIdentity {
        if let Some((raw, port)) =
            endpoint(&record.upstream_host).or_else(|| endpoint(&record.route))
        {
            let host = raw
                .trim_matches(|c| c == '[' || c == ']' || c == '.')
                .to_lowercase();
            let local = host == "localhost"
                || host.ends_with(".localhost")
                || host.starts_with("127.")
                || matches!(host.as_str(), "::1" | "0.0.0.0" | "::");
            let mut authority = if host.contains(':') {
                format!("[{host}]")
            } else {
                host.clone()
            };
            if let Some(port) = port {
                authority.push_str(&format!(":{port}"));
            }
            let (key, name) = match host_alias(&host).and_then(alias) {
                Some((key, name)) => (key.to_string(), name.to_string()),
                None => (format!("host:{host}"), host.clone()),
            };
            let evidence = match record.source.as_deref() {
                Some("proxy") => format!("Observed upstream endpoint host: {authority}. Endpoint identity does not prove the model vendor behind it."),
                Some("network") => format!("Provider host attributed from passive network discovery: {authority}. Shared addresses and gateways do not prove the original host or model vendor."),
                Some("log") => format!("Endpoint host reported in session log: {authority}; not independently verified on the network."),
                _ => format!("Recorded endpoint host: {authority}; observation source is unknown, so attribution is unverified."),
            };
            return ProviderIdentity {
                key: if local {
                    format!("local:{authority}")
                } else {
                    key
                },
                name: if local {
                    format!("Local · {authority}")
                } else {
                    name
                },
                host: Some(host),
                evidence,
                unverified: record.source.as_deref() != Some("proxy"),
            };
        }
        let upstream = label(&record.upstream_host);
        let reported = if upstream.is_empty() || upstream == "unknown" {
            label(&record.route)
        } else {
            upstream
        };
        if matches!(reported.as_str(), "" | "unknown" | "_") {
            return ProviderIdentity {
                key: "unknown".into(),
                name: "Unknown".into(),
                host: None,
                evidence: "No endpoint host or provider label was recorded; provider is unknown."
                    .into(),
                unverified: true,
            };
        }
        let (key, name) = match alias(&reported) {
            Some((key, name)) => (key.to_string(), name.to_string()),
            None => (format!("reported:{reported}"), reported.clone()),
        };
        ProviderIdentity { key, name, host: None, evidence: format!("Reported provider label: {reported}; endpoint unverified. No endpoint host was recorded."), unverified: true }
    }

    fn merge(self, other: &ProviderIdentity) -> ProviderIdentity {
        if &self == other {
            return self;
        }
        let unverified = self.unverified || other.unverified;
        ProviderIdentity {
            host: self.host.or_else(|| other.host.clone()),
            unverified,
            evidence: if unverified { "Includes endpoint-unverified provenance; inspect individual calls for reported labels and observations." } else { "Observed endpoints; inspect individual calls for hosts. Endpoints do not prove model vendors." }.into(),
            ..self
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DashboardFilter {
    pub from: Option<Time>,
    pub through: Option<Time>,
    pub harness: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl DashboardFilter {
    fn includes_date(&self, record: &RequestRecord) -> bool {
        self.from.is_none_or(|from| record.started_at >= from)
            && self
                .through
                .is_none_or(|through| record.started_at < through)
    }
    pub fn includes(&self, record: &RequestRecord) -> bool {
        self.includes_date(record)
            && self
                .harness
                .as_ref()
                .is_none_or(|harness| *harness == record.harness)
            && self
                .model
                .as_ref()
                .is_none_or(|model| *model == record.model)
            && self
                .provider
                .as_ref()
                .is_none_or(|provider| *provider == ProviderIdentity::from(record).key)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DashboardSummary {
    pub count: usize,
    pub median_ttft: Option<f64>,
    pub p95_ttft: Option<f64>,
    pub median_tps: Option<f64>,
    pub p95_tps: Option<f64>,
    pub output_tokens: i32,
    pub estimated_count: usize,
    pub interrupted_count: usize,
    pub latency_count: usize,
    pub speed_count: usize,
    pub round_trip_count: usize,
}

fn measured(value: Option<f64>) -> Option<f64> {
    value.filter(|number| number.is_finite() && *number >= 0.0)
}

impl DashboardSummary {
    pub fn valid_latency(record: &RequestRecord) -> bool {
        !record.aborted && record.status < 400 && measured(record.ttft).is_some()
    }
    /// Speed over a real generation window. A whole-request fallback is not generation speed.
    pub fn valid_speed(record: &RequestRecord) -> bool {
        !record.aborted
            && record.status < 400
            && measured(record.tps).is_some()
            && record
                .generation
                .is_some_and(|generation| generation.is_finite() && generation > 0.0)
    }
    pub fn from<'a>(source: impl IntoIterator<Item = &'a RequestRecord>) -> DashboardSummary {
        let (mut count, mut tokens, mut estimated, mut interrupted, mut round_trip) =
            (0, 0i32, 0, 0, 0);
        let (mut latency, mut speed) = (Vec::new(), Vec::new());
        for record in source {
            count += 1;
            tokens = tokens.saturating_add(record.output_tokens.max(0));
            if record.tokens_estimated {
                estimated += 1;
            }
            if record.aborted || record.status >= 400 {
                interrupted += 1;
                continue;
            }
            if Self::valid_latency(record) {
                latency.extend(record.ttft);
            }
            if Self::valid_speed(record) {
                speed.extend(record.tps);
            } else if measured(record.tps).is_some()
                && record.generation.is_none_or(|generation| generation == 0.0)
                && record.total.is_finite()
                && record.total > 0.0
            {
                round_trip += 1;
            }
        }
        latency.sort_by(f64::total_cmp);
        speed.sort_by(f64::total_cmp);
        DashboardSummary {
            count,
            median_ttft: percentile(&latency, 0.5),
            p95_ttft: percentile(&latency, 0.95),
            median_tps: percentile(&speed, 0.5),
            p95_tps: percentile(&speed, 0.95),
            output_tokens: tokens,
            estimated_count: estimated,
            interrupted_count: interrupted,
            latency_count: latency.len(),
            speed_count: speed.len(),
            round_trip_count: round_trip,
        }
    }
}

// R7 (linear interpolation between closest ranks).
fn percentile(sorted: &[f64], probability: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let position = (sorted.len() - 1) as f64 * probability;
    let lower = position as usize;
    let upper = (lower + 1).min(sorted.len() - 1);
    Some(sorted[lower] + (sorted[upper] - sorted[lower]) * (position - lower as f64))
}

#[derive(Clone, Debug, PartialEq)]
pub struct DashboardGroup {
    pub provider: ProviderIdentity,
    pub harness: String,
    pub summary: DashboardSummary,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DashboardBucket {
    pub date: Time,
    pub summary: DashboardSummary,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DashboardReport {
    pub records: Vec<Arc<RequestRecord>>,
    pub providers: Vec<ProviderIdentity>,
    pub harnesses: Vec<String>,
    pub models: Vec<String>,
    pub summary: DashboardSummary,
    pub groups: Vec<DashboardGroup>,
    pub trends: Vec<DashboardBucket>,
    // What each filter can still be set to, given the other two. Choosing a harness narrows the models
    // and providers to the ones it has calls with, and the same in every direction, so no offered choice
    // leads to an empty result. A dimension's own filter does not narrow it. Mirrors Dashboard.swift.
    pub harness_options: Vec<String>,
    pub provider_options: Vec<ProviderIdentity>,
    pub model_options: Vec<String>,
}

fn distinct_sorted<'a>(values: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut names: Vec<String> = values
        .collect::<HashSet<_>>()
        .into_iter()
        .cloned()
        .collect();
    names.sort();
    names
}

impl DashboardReport {
    pub fn create(source: &[Arc<RequestRecord>], filter: &DashboardFilter) -> DashboardReport {
        let mut seen = HashSet::new();
        // Each record in the date window, with its provider worked out once.
        let window: Vec<(&Arc<RequestRecord>, ProviderIdentity)> = source
            .iter()
            .filter(|record| seen.insert(record.dedup_key()) && filter.includes_date(record))
            .map(|record| (record, ProviderIdentity::from(record)))
            .collect();

        let mut identities: Vec<ProviderIdentity> = Vec::new();
        let mut positions: HashMap<String, usize> = HashMap::new();
        for (_, identity) in &window {
            match positions.get(&identity.key) {
                Some(&index) => identities[index] = identities[index].clone().merge(identity),
                None => {
                    positions.insert(identity.key.clone(), identities.len());
                    identities.push(identity.clone());
                }
            }
        }
        identities.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.key.cmp(&b.key)));

        let by_harness = |record: &RequestRecord| {
            filter
                .harness
                .as_ref()
                .is_none_or(|harness| *harness == record.harness)
        };
        let by_model = |record: &RequestRecord| {
            filter
                .model
                .as_ref()
                .is_none_or(|model| *model == record.model)
        };
        let by_provider = |identity: &ProviderIdentity| {
            filter
                .provider
                .as_ref()
                .is_none_or(|provider| *provider == identity.key)
        };

        let mut selected: Vec<&(&Arc<RequestRecord>, ProviderIdentity)> = window
            .iter()
            .filter(|(record, identity)| {
                by_harness(record) && by_model(record) && by_provider(identity)
            })
            .collect();
        selected.sort_by(|a, b| {
            b.0.started_at
                .cmp(&a.0.started_at)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        let records: Vec<Arc<RequestRecord>> = selected
            .iter()
            .map(|(record, _)| Arc::clone(record))
            .collect();

        let mut grouped: Vec<(ProviderIdentity, String, Vec<&RequestRecord>)> = Vec::new();
        let mut group_positions: HashMap<(String, String), usize> = HashMap::new();
        for (record, identity) in selected.iter().map(|pair| (&pair.0, &pair.1)) {
            match group_positions.get(&(identity.key.clone(), record.harness.clone())) {
                Some(&index) => {
                    grouped[index].0 = grouped[index].0.clone().merge(identity);
                    grouped[index].2.push(record);
                }
                None => {
                    group_positions.insert(
                        (identity.key.clone(), record.harness.clone()),
                        grouped.len(),
                    );
                    grouped.push((identity.clone(), record.harness.clone(), vec![record]));
                }
            }
        }
        let mut groups: Vec<DashboardGroup> = grouped
            .into_iter()
            .map(|(provider, harness, members)| DashboardGroup {
                provider,
                harness,
                summary: DashboardSummary::from(members),
            })
            .collect();
        groups.sort_by(|a, b| {
            a.provider
                .name
                .cmp(&b.provider.name)
                .then_with(|| a.harness.cmp(&b.harness))
        });

        let mut trends = Vec::new();
        if let (Some(newest), Some(oldest)) = (records.first(), records.last()) {
            let span = newest.started_at.since(oldest.started_at);
            // Hourly up to two days, daily up to 119 days, then whole weeks sized to keep at most 119 buckets.
            let interval = if span <= 48.0 * 3600.0 {
                3600.0
            } else if span <= 119.0 * 86_400.0 {
                86_400.0
            } else {
                ((span / (7.0 * 86_400.0) + 1.0) / 119.0).ceil().max(1.0) * 7.0 * 86_400.0
            };
            // The epoch began on a Thursday; weeks are anchored four days later, on a Monday.
            let anchor = if interval > 86_400.0 {
                4.0 * 86_400.0
            } else {
                0.0
            };
            let mut buckets: Vec<(i64, Vec<&RequestRecord>)> = Vec::new();
            let mut bucket_positions: HashMap<i64, usize> = HashMap::new();
            for record in &records {
                let start = (anchor
                    + ((record.started_at.unix_seconds() as f64 - anchor) / interval).floor()
                        * interval) as i64;
                match bucket_positions.get(&start) {
                    Some(&index) => buckets[index].1.push(record),
                    None => {
                        bucket_positions.insert(start, buckets.len());
                        buckets.push((start, vec![record]));
                    }
                }
            }
            buckets.sort_by_key(|(start, _)| *start);
            trends = buckets
                .into_iter()
                .map(|(start, members)| DashboardBucket {
                    date: Time::from_unix_seconds(start),
                    summary: DashboardSummary::from(members),
                })
                .collect();
        }

        let provider_facet: HashSet<&str> = window
            .iter()
            .filter(|(record, _)| by_harness(record) && by_model(record))
            .map(|(_, identity)| identity.key.as_str())
            .collect();
        DashboardReport {
            summary: DashboardSummary::from(records.iter().map(Arc::as_ref)),
            harnesses: distinct_sorted(window.iter().map(|(record, _)| &record.harness)),
            models: distinct_sorted(window.iter().map(|(record, _)| &record.model)),
            harness_options: distinct_sorted(
                window
                    .iter()
                    .filter(|(record, identity)| by_provider(identity) && by_model(record))
                    .map(|(record, _)| &record.harness),
            ),
            provider_options: identities
                .iter()
                .filter(|identity| provider_facet.contains(identity.key.as_str()))
                .cloned()
                .collect(),
            model_options: distinct_sorted(
                window
                    .iter()
                    .filter(|(record, identity)| by_harness(record) && by_provider(identity))
                    .map(|(record, _)| &record.model),
            ),
            providers: identities,
            records,
            groups,
            trends,
        }
    }
}

/// A call in flight, as shown in Live. Never written to history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveCall {
    pub id: String,
    pub harness: String,
    pub model: String,
    pub provider: String,
    pub phase: String,
    pub started_at: Time,
    pub last_activity: Time,
    pub ttft: Option<f64>,
    pub rate: Option<f64>,
    pub output_tokens: Option<i32>,
    pub estimated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessStatus {
    pub name: String,
    pub has_logs: bool,
    pub is_running: bool,
    pub limitation: Option<String>,
    pub is_installed: bool,
}

/// One connection's byte counters at one moment, from the optional elevated collector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlowSample {
    pub pid: u32,
    pub harness: String,
    pub host: String,
    pub received: u64,
    pub sent: u64,
    pub connection_id: Option<String>,
}

pub trait FlowSampler: Send + Sync {
    fn available(&self) -> bool;
    fn status(&self) -> String;
    fn sample(&self) -> Vec<FlowSample>;
}
