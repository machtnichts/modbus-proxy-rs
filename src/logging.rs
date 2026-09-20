//! Tiny logger: stdout plus an optional size-rotated file.
//!
//! Written by hand because the binary must not depend on a logging crate, and
//! journald already captures stdout for the systemd service. The file handler
//! exists for the same reason the Python version has one: evidence that survives
//! a service restart.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::LogCfg;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARNING",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }

    pub fn from_str(s: &str) -> Level {
        match s.trim().to_ascii_uppercase().as_str() {
            "ERROR" | "CRITICAL" => Level::Error,
            "WARN" | "WARNING" => Level::Warn,
            "DEBUG" => Level::Debug,
            _ => Level::Info,
        }
    }
}

struct FileSink {
    path: PathBuf,
    file: File,
    size: u64,
    max_bytes: u64,
    backups: u32,
}

pub struct Logger {
    level: Level,
    file: Option<Mutex<FileSink>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

impl Logger {
    fn new(cfg: &LogCfg) -> Logger {
        let file = cfg.file.as_ref().and_then(|p| {
            let path = PathBuf::from(p);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(fh) => {
                    let size = fh.metadata().map(|m| m.len()).unwrap_or(0);
                    Some(Mutex::new(FileSink {
                        path,
                        file: fh,
                        size,
                        max_bytes: cfg.max_bytes,
                        backups: cfg.backup_count,
                    }))
                }
                Err(e) => {
                    eprintln!("muxproxy: file logging disabled: {}", e);
                    None
                }
            }
        });
        Logger {
            level: Level::from_str(&cfg.level),
            file,
        }
    }

    fn emit(&self, level: Level, msg: &str) {
        if level > self.level {
            return;
        }
        let line = format!(
            "{} {:<7} muxproxy: {}\n",
            format_timestamp(SystemTime::now()),
            level.label(),
            msg
        );
        let mut out = std::io::stdout();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
        if let Some(sink) = &self.file {
            let mut s = sink.lock().unwrap();
            if s.size + line.len() as u64 > s.max_bytes && s.max_bytes > 0 {
                rotate(&s.path, s.backups);
                if let Ok(fh) = OpenOptions::new().create(true).append(true).open(&s.path) {
                    s.file = fh;
                    s.size = 0;
                }
            }
            if s.file.write_all(line.as_bytes()).is_ok() {
                s.size += line.len() as u64;
            }
        }
    }
}

/// Rotate `path` -> `path.1`, `path.1` -> `path.2`, ... keeping `backups` files.
fn rotate(path: &Path, backups: u32) {
    if backups == 0 {
        let _ = std::fs::remove_file(path);
        return;
    }
    for i in (1..backups).rev() {
        let from = PathBuf::from(format!("{}.{}", path.display(), i));
        let to = PathBuf::from(format!("{}.{}", path.display(), i + 1));
        let _ = std::fs::rename(from, to);
    }
    let _ = std::fs::rename(path, PathBuf::from(format!("{}.1", path.display())));
}

pub fn init(cfg: &LogCfg) {
    let _ = LOGGER.set(Logger::new(cfg));
    // if init was called twice the first logger stays; that is fine and avoids a
    // panic in a service that restarts itself
}

pub fn log(level: Level, msg: String) {
    match LOGGER.get() {
        Some(l) => l.emit(level, &msg),
        None => {
            if level <= Level::Info {
                eprintln!("muxproxy: {}", msg);
            }
        }
    }
}

#[allow(dead_code)]
pub fn error(msg: String) {
    log(Level::Error, msg);
}
pub fn warn(msg: String) {
    log(Level::Warn, msg);
}
pub fn info(msg: String) {
    log(Level::Info, msg);
}
pub fn debug(msg: String) {
    log(Level::Debug, msg);
}

/// UTC timestamp without a date crate: civil-from-days (Howard Hinnant's
/// algorithm), the same one used by most std-only timestamp helpers.
pub fn format_timestamp(t: SystemTime) -> String {
    let dur = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return "1970-01-01 00:00:00.000".to_string(),
    };
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days since 1970-01-01 -> civil date
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        y, m, d, h, mi, s, millis
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_a_known_epoch() {
        assert_eq!(
            format_timestamp(UNIX_EPOCH),
            "1970-01-01 00:00:00.000".to_string()
        );
        // 2026-09-13T09:09:53Z = 1789290593 (checked against date -u -d @1789290593)
        let t = UNIX_EPOCH + Duration::from_secs(1_789_290_593);
        assert_eq!(format_timestamp(t), "2026-09-13 09:09:53.000".to_string());
    }

    #[test]
    fn level_ordering_filters_debug() {
        assert!(Level::Debug > Level::from_str("INFO"));
        assert!(Level::Error <= Level::from_str("INFO"));
        assert_eq!(Level::from_str("warning"), Level::Warn);
        assert_eq!(Level::from_str("nonsense"), Level::Info);
    }
}
