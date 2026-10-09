//! Speed Tracker for Windows. The modules here hold no UI: session-log readers, the tracker that
//! combines them, history and dashboard analytics, and the optional proxy. Parser and analytics
//! behaviour mirrors the Swift core on macOS; keep the two in step.

pub mod antigravity;
pub mod discovery;
pub mod domain;
pub mod harness;
pub mod history;
pub mod json;
pub mod logs;
pub mod opencode;
pub mod parsers;
pub mod processes;
pub mod proxy;
pub mod sqlite;
pub mod time;
pub mod tracker;
