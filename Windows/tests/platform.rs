//! The Win32 process queries, checked against what this test process knows about itself.
#![cfg(windows)]

use speedtracker::processes;
use std::time::{SystemTime, UNIX_EPOCH};

fn this_executable() -> String {
    std::env::current_exe().unwrap().file_stem().unwrap().to_string_lossy().into_owned()
}

#[test]
fn the_process_list_names_this_process_without_its_exe_suffix() {
    let me = std::process::id();
    let listed: Vec<_> = processes::list().into_iter().filter(|process| process.pid == me).collect();
    assert_eq!(listed.len(), 1, "this process is listed once");
    assert!(listed[0].name.eq_ignore_ascii_case(&this_executable()), "{} is named as {}", this_executable(), listed[0].name);
}

#[test]
fn start_time_of_this_process_is_a_recent_file_time() {
    // File times count 100 ns from 1601; 11,644,473,600 s separate that from the Unix epoch.
    let started = processes::start_time(std::process::id()).expect("a process can read its own start time");
    let started_unix = (started / 10_000_000) as i64 - 11_644_473_600;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((now - 3600..=now).contains(&started_unix), "started at {started_unix}, now {now}");
}

#[test]
fn command_line_of_this_process_names_its_executable() {
    let command_line = processes::command_line(std::process::id()).expect("a process can read its own command line");
    assert!(command_line.to_lowercase().contains(&this_executable().to_lowercase()), "{command_line}");
    assert!(!command_line.contains('\0'), "the text ends where the command line does");
}

#[test]
fn a_process_id_that_does_not_exist_has_no_start_time_or_command_line() {
    // Process ids are multiples of four on Windows; this one cannot be live.
    assert_eq!(processes::start_time(u32::MAX - 2), None);
    assert_eq!(processes::command_line(u32::MAX - 2), None);
}

#[test]
fn image_path_of_this_process_is_the_file_it_runs_from() {
    let reported = processes::image_path(std::process::id()).expect("a process can read its own path");
    let expected = std::env::current_exe().unwrap();
    // Both name one file; spelling of the drive and prefix can differ.
    assert_eq!(std::fs::canonicalize(&reported).unwrap(), std::fs::canonicalize(&expected).unwrap(), "{reported}");
    assert_eq!(processes::image_path(u32::MAX - 2), None, "a process that does not exist has no path");
}
