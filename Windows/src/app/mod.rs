//! The Windows program. One executable plays three parts, chosen by its arguments: the tray app
//! (no arguments), a window (`--ui`), or the optional elevated collector (`--collector`).

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
        _ => tray::run(
            arguments.iter().any(|argument| argument == "--dashboard"),
            arguments.iter().any(|argument| argument == "--live"),
        ),
    }
}
