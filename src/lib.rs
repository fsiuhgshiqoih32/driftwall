//! Driftwall: light, GPU-decoded live wallpapers for Windows.
//!
//! Two processes from one exe: the library window (`ui`, egui) and the background process
//! (`daemon`) that keeps wallpapers playing. They share JSON files and talk through window
//! messages (`ipc`).

pub mod art;
pub mod autostart;
pub mod daemon;
pub mod desktop;
pub mod engine;
pub mod gpu;
pub mod host;
pub mod import;
pub mod ipc;
pub mod layout;
pub mod logging;
pub mod mf;
pub mod model;
pub mod paths;
pub mod pictures;
pub mod player;
pub mod probe;
pub mod selftest;
pub mod ui;
pub mod watch;
pub mod wic;

/// Clears every wallpaper, puts Windows' own pictures back, turns off start with Windows and
/// stops the background process. Safe to run before deleting Driftwall.
pub fn reset() {
    mf::init_com_mta();
    let mut s = model::Settings::load();
    s.displays.clear();
    s.span = None;
    s.start_with_windows = false;
    let _ = s.save();
    autostart::set(false);
    if ipc::daemon_running() {
        ipc::notify_reload();
        // Let it put the pictures back before it quits.
        std::thread::sleep(std::time::Duration::from_secs(3));
        ipc::quit_daemon();
    } else {
        pictures::restore_now();
    }
}
