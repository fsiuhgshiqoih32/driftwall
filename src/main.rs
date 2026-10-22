#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use driftwall::{daemon, ipc, logging, selftest, ui};

fn attach_console() {
    use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x == a);
    if has("--version") {
        attach_console();
        println!("Driftwall {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if has("--self-test") {
        attach_console();
        std::process::exit(selftest::run());
    }
    if has("--quit") {
        ipc::quit_daemon();
        return;
    }
    if has("--reset") {
        attach_console();
        logging::init("reset", false);
        driftwall::reset();
        println!(
            "Driftwall reset: wallpapers cleared, Windows' pictures restored, start with Windows off."
        );
        return;
    }
    if has("--toggle-pause") {
        ipc::toggle_pause();
        return;
    }
    if has("--reload") {
        ipc::notify_reload();
        return;
    }
    if has("--background") {
        logging::init("bg", cfg!(debug_assertions));
        if let Err(e) = daemon::run() {
            log::error!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    logging::init("ui", cfg!(debug_assertions));
    if let Err(e) = ui::run() {
        log::error!("{e:#}");
        std::process::exit(1);
    }
}
