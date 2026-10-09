//! What the tray process and its windows say to each other, one JSON object per line over the
//! window process's standard input and output.

use serde::{Deserialize, Serialize};
use speedtracker::domain::{FlowSample, HarnessStatus, LiveCall, RequestRecord};

/// Live state, sent to every open window whenever it changes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub active: Vec<LiveCall>,
    pub harnesses: Vec<HarnessStatus>,
    pub held_record: Option<RequestRecord>,
    /// The newest few finished calls for the Live target, newest first.
    #[serde(default)]
    pub recent: Vec<RequestRecord>,
    /// Recent finished calls across all harnesses, newest first, for the Models tab.
    #[serde(default)]
    pub all_recent: Vec<RequestRecord>,
    pub held_rate: Option<f64>,
    pub held_ttft: Option<f64>,
    pub held_rate_estimated: bool,
    pub held_ttft_estimated: bool,
    pub target: Option<String>,
    pub network_status: String,
    pub enhanced_available: bool,
    pub enhanced_enabled: bool,
    /// Windows is asking for administrator approval; the flyout must not close when it loses focus.
    pub enhanced_pending: bool,
    /// Counts finished calls written since launch, so the dashboard knows to read history again.
    pub history_revision: u64,
    /// Something the user should be told, such as a setting that could not be saved.
    pub notice: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToWindow {
    Snapshot(Box<Snapshot>),
    Focus,
    Close,
    ShowTab(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToTray {
    SetTarget(Option<String>),
    SetEnhanced(bool),
    OpenDashboard,
}

/// One message from the elevated collector.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub status: String,
    pub samples: Vec<FlowSample>,
}
