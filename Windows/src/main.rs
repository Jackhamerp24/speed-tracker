// No console window: this is a notification-area app. Debug builds keep the console so a panic can be read.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;

fn main() {
    #[cfg(windows)]
    std::process::exit(app::main());
    #[cfg(not(windows))]
    eprintln!("This is the Windows build of Speed Tracker. On macOS, build the Swift app instead.");
}
