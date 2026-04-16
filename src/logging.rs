use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
                        .with_line_number(true)
                        .with_file(true)
                        .with_target(true),
                )
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
        }
    }
}
