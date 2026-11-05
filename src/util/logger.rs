//! Minimal stderr logger for the `log` facade, so no logging framework has to be compiled in.
//!
//! The level comes from `KENKURS_LOG` (`off`, `error`, `warn`, `info`, `debug` or `trace`) and
//! defaults to `info`. Each line carries the time elapsed since start-up, which is handy when
//! reasoning about audio timing.

use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

use log::{LevelFilter, Log, Metadata, Record};

struct StderrLogger;

static LOGGER: StderrLogger = StderrLogger;
static START: OnceLock<Instant> = OnceLock::new();

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let elapsed = START.get_or_init(Instant::now).elapsed().as_secs_f64();
        let target = record.target();
        let target = target.strip_prefix("kenkurs::").unwrap_or(target);
        let _ = writeln!(
            std::io::stderr().lock(),
            "[{elapsed:9.3} {:<5} {target}] {}",
            record.level(),
            record.args()
        );
    }

    fn flush(&self) {}
}

/// Installs the logger. Calling it again is harmless.
pub fn init() {
    START.get_or_init(Instant::now);
    let level = std::env::var("KENKURS_LOG")
        .ok()
        .and_then(|value| value.trim().parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::Info);
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}
