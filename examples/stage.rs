//! Fills a Driftwall data folder for a manual test (set DRIFTWALL_HOME first).
//!
//!     stage add <file>...      import files and show the first on every display
//!     stage rules <default|keep-playing>
//!     stage clear              remove all assignments

use driftwall::model::{FullscreenRule, Library, Settings};
use driftwall::{desktop, import, mf};

fn main() {
    mf::init_com_mta();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut lib = Library::load();
    let mut s = Settings::load();
    s.start_with_windows = false;
    match args.first().map(String::as_str) {
        Some("add") => {
            for f in &args[1..] {
                let item = import::import(std::path::Path::new(f)).expect("import");
                println!(
                    "imported {} as {} ({}x{}, {} ms)",
                    f, item.id, item.width, item.height, item.duration_ms
                );
                lib.items.push(item);
            }
            lib.save().unwrap();
            let id = lib.items.last().unwrap().id.clone();
            for d in desktop::displays().unwrap() {
                s.displays.insert(d.id, id.clone());
            }
        }
        Some("rules") => match args.get(1).map(String::as_str) {
            Some("keep-playing") => {
                s.pause_when_covered = false;
                s.fullscreen = FullscreenRule::KeepPlaying;
            }
            _ => {
                let d = Settings::default();
                s.pause_when_covered = d.pause_when_covered;
                s.fullscreen = d.fullscreen;
            }
        },
        Some("clear") => {
            s.displays.clear();
            s.span = None;
        }
        _ => {
            eprintln!("usage: stage add <file>... | rules <default|keep-playing> | clear");
            std::process::exit(2);
        }
    }
    s.save().unwrap();
    println!(
        "settings: {} display(s) assigned, covered pause {}, full screen {:?}",
        s.displays.len(),
        s.pause_when_covered,
        s.fullscreen
    );
}
