//! A small file logger: `%LOCALAPPDATA%\Driftwall\driftwall.log`, trimmed when it grows past
//! 1 MB. Each process tags its lines (`bg` for the background engine, `ui` for the window).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

struct Logger {
    tag: &'static str,
    file: Mutex<Option<File>>,
    echo: bool,
}

impl log::Log for Logger {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= log::Level::Info || meta.target().starts_with("driftwall")
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let line = format!(
            "{secs:.3} {} {:5} {}\n",
            self.tag,
            record.level(),
            record.args()
        );
        if self.echo {
            eprint!("{line}");
        }
        if let Ok(mut f) = self.file.lock()
            && let Some(f) = f.as_mut()
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock()
            && let Some(f) = f.as_mut()
        {
            let _ = f.flush();
        }
    }
}

pub fn init(tag: &'static str, echo: bool) {
    let path = crate::paths::log_file();
    let _ = std::fs::create_dir_all(crate::paths::home());
    if std::fs::metadata(&path)
        .map(|m| m.len() > 1 << 20)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, path.with_extension("old.log"));
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    let logger = Box::leak(Box::new(Logger {
        tag,
        file: Mutex::new(file),
        echo,
    }));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(log::LevelFilter::Debug);
    }
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        log::logger().flush();
    }));
}
