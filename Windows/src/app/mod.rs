//! The Windows program. One executable plays three parts, chosen by its arguments: the tray app
//! (no arguments), a window (`--ui`), or the optional elevated collector (`--collector`).
//! `--icon-preview FILE.png` draws the tray icon's states to a picture for checking.

pub mod collector;
pub mod enhanced;
pub mod ipc;
pub mod tray;
pub mod ui;
pub mod win;

use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

pub fn main() -> i32 {
    // Screen coordinates must mean real pixels in every part, or the flyout lands in the wrong place on scaled displays.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("--collector") => collector::run(&arguments[1..]),
        Some("--ui") => ui::run(&arguments[1..]),
        Some("--icon-preview") => arguments.get(1).map_or(2, |path| tray::icon_preview(path)),
        _ => tray::run(
            arguments.iter().any(|argument| argument == "--dashboard"),
            arguments.iter().any(|argument| argument == "--live"),
        ),
    }
}
