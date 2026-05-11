use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// `FormatTime` impl that renders timestamps in the local timezone.
///
/// The default `tracing_subscriber` timer emits UTC, which forces timezone
/// math every time a log file is read alongside wall-clock observations
/// ("I saw it break around 7am"). `jiff` is already a dependency (see
/// `commands/util.rs`), its `TimeZone::system()` reads the tzdb safely, and
/// it tracks DST transitions for a daemon that runs for weeks.
struct LocalTimer;

/// Write `ts` as `YYYY-MM-DDTHH:MM:SS.ffffff±HH:MM` in the local timezone.
/// Split out from the trait impl so it is directly testable.
fn write_local_timestamp(out: &mut impl std::fmt::Write, ts: jiff::Timestamp) -> std::fmt::Result {
    let zoned = ts.to_zoned(jiff::tz::TimeZone::system());
    write!(out, "{}", zoned.strftime("%Y-%m-%dT%H:%M:%S.%6f%:z"))
}

impl tracing_subscriber::fmt::time::FormatTime for LocalTimer {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write_local_timestamp(w, jiff::Timestamp::now())
    }
}

type ReloadFn = Box<dyn Fn(&str) + Send + Sync>;

/// Type-erased handle for reloading the tracing filter at runtime.
/// Stored in a global so the daemon's SIGUSR1 handler can cycle log levels.
static LOG_RELOAD: OnceLock<ReloadFn> = OnceLock::new();

/// Current log level index (0=info, 1=debug, 2=trace). Daemon cycles
/// through these on SIGUSR1.
static LOG_LEVEL_INDEX: AtomicU8 = AtomicU8::new(0);

const LOG_LEVELS: [&str; 3] = ["gritty=info", "gritty=debug", "gritty=trace"];

/// Cycle to the next log level. Called from the daemon's SIGUSR1 handler.
pub fn cycle_log_level() {
    let idx = LOG_LEVEL_INDEX
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |i| {
            Some((i + 1) % LOG_LEVELS.len() as u8)
        })
        .unwrap_or(0);
    let next = ((idx + 1) % LOG_LEVELS.len() as u8) as usize;
    if let Some(reload) = LOG_RELOAD.get() {
        reload(LOG_LEVELS[next]);
    }
}

/// Return the human-readable name of the current log level.
pub fn current_log_level_name() -> &'static str {
    let idx = LOG_LEVEL_INDEX.load(Ordering::Relaxed) as usize;
    match LOG_LEVELS.get(idx) {
        Some(s) => s.strip_prefix("gritty=").unwrap_or(s),
        None => "unknown",
    }
}

/// Path to the log file currently being written by the daemon.
/// Stored in a global so the SIGUSR2 handler can reopen it.
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Writer that can be told to reopen its underlying file (for log rotation).
/// The daemon's SIGUSR2 handler sets the reopen flag; the next write reopens.
pub struct ReopenableWriter {
    file: Mutex<std::fs::File>,
    reopen: AtomicBool,
}

impl ReopenableWriter {
    fn new(file: std::fs::File) -> Self {
        Self { file: Mutex::new(file), reopen: AtomicBool::new(false) }
    }

    fn request_reopen(&self) {
        self.reopen.store(true, Ordering::Relaxed);
    }
}

impl std::io::Write for &ReopenableWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.reopen.swap(false, Ordering::Relaxed)
            && let Some(path) = LOG_PATH.get()
            && let Some(new_file) = open_log_file(path)
        {
            *self.file.lock().unwrap() = new_file;
        }
        self.file.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.lock().unwrap().flush()
    }
}

/// Global writer so the SIGUSR2 handler can trigger a reopen.
static LOG_WRITER: OnceLock<Arc<ReopenableWriter>> = OnceLock::new();

/// Reopen the daemon log file. Called from the daemon's SIGUSR2 handler.
pub fn reopen_log_file() {
    if let Some(w) = LOG_WRITER.get() {
        w.request_reopen();
    }
}

fn open_log_file(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path).ok()
}

/// Initialize the tracing subscriber.
///
/// When `log_path` is `Some`, logs to a file with reload support (SIGUSR1
/// cycles the level, SIGUSR2 reopens the file). Otherwise logs to stderr.
pub fn init_tracing(verbose: bool, log_path: Option<&Path>) {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter_spec = if std::env::var("RUST_LOG").is_ok() {
        None // RUST_LOG takes priority, no cycling
    } else if verbose {
        LOG_LEVEL_INDEX.store(1, Ordering::Relaxed);
        Some("gritty=debug")
    } else {
        Some("gritty=info")
    };

    let filter = match filter_spec {
        Some(spec) => EnvFilter::new(spec),
        None => EnvFilter::from_default_env(),
    };

    match log_path.and_then(|p| {
        LOG_PATH.set(p.to_path_buf()).ok();
        open_log_file(p)
    }) {
        Some(file) => {
            let writer = Arc::new(ReopenableWriter::new(file));
            let _ = LOG_WRITER.set(writer.clone());

            let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(filter);
            let _ = LOG_RELOAD.set(Box::new(move |spec: &str| {
                let _ = reload_handle.reload(EnvFilter::new(spec));
            }));

            tracing_subscriber::registry()
                .with(filter_layer)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(writer)
                        .with_ansi(false)
                        .with_timer(LocalTimer)
                        .with_line_number(true)
                        .with_file(true)
                        .with_target(true),
                )
                .init();
        }
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_timer(LocalTimer)
                .init();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_timestamp_has_rfc3339_shape_and_offset() {
        let mut s = String::new();
        // 2024-03-10T12:34:56.789000Z
        let ts = jiff::Timestamp::new(1_710_074_096, 789_000_000).unwrap();
        write_local_timestamp(&mut s, ts).unwrap();
        // YYYY-MM-DDTHH:MM:SS.ffffff±HH:MM -- exact wall-clock values depend on
        // the host TZ, so assert shape, not content.
        assert_eq!(s.len(), "2024-03-10T05:34:56.789000-07:00".len(), "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[19..20], ".");
        assert_eq!(&s[20..26], "789000");
        assert!(matches!(&s[26..27], "+" | "-"), "expected offset sign in {s}");
        assert_eq!(&s[29..30], ":");
    }

    #[test]
    fn local_timestamp_microsecond_padding() {
        let mut s = String::new();
        write_local_timestamp(&mut s, jiff::Timestamp::new(0, 1_000).unwrap()).unwrap();
        assert!(s.contains(".000001"), "micros not zero-padded: {s}");
    }
}
