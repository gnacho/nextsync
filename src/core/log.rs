//! Application logger with live subscription and on-disk daily files.
//!
//! Task 5.4: ports `storage/log.py` (v0.4.0). The [`LogBuffer`] is the real
//! logger the app consumes: emitters (the per-run outcome lines, the update
//! checker) append through it, and the conflict resolver's Recent tab uses
//! [`LogBuffer::subscribe`] and [`LogBuffer::recent_lines`] (mirroring
//! `ui/conflict_resolver.py`). The interactive log window was removed by
//! user decision (issue #15); the daily files under `$XDG_STATE_HOME` are
//! what "check the log" refers to.
//!
//! The buffer keeps the last `live_history_lines` lines in memory (the Python
//! `deque(maxlen=...)`), so `tail` works without a subscription. When
//! `save_to_disk` is enabled it also appends to one private, predictably named
//! file per local day (`<prefix>-YYYY-MM-DD.log`), pruning files older than
//! `retention_days`, exactly like `DailyFileHandler`.
//!
//! # Deviations from `storage/log.py` (motivated)
//! - `append(line)` receives an already formatted line; the Python `logging`
//!   formatter (timestamp + level) lives outside this module, so emitters
//!   build the full line themselves. There is no `%`-style argument formatting
//!   (`test_numeric_format_arguments_remain_numeric` does not apply).
//! - The daily date defaults to UTC (`utc_date_string`, no `chrono`
//!   dependency) instead of `dt.date.today()` (local). `date_provider` is
//!   injectable for tests and for a future local-time implementation.
//! - The log directory is created lazily on the first disk write instead of
//!   eagerly in the constructor, so building a `LogBuffer` has no filesystem
//!   side effects.
//! - `Subscription` mirrors the `state.rs` pattern but does **not** invoke the
//!   callback with the current history on subscribe (the Python `subscribe`
//!   only registers; consumers seed from `tail` explicitly).
//! - Subscribers unsubscribed during a notification stop receiving in the same
//!   pass (the Python snapshot `tuple()` would still notify them once). This is
//!   unobservable in practice and only differs under adversarial callbacks.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::util::paths::state_dir;
use crate::util::redact::Redact;

/// Maximum lines kept in the in-memory history (Python `LIVE_HISTORY_LINES`).
pub const DEFAULT_LIVE_HISTORY_LINES: usize = 500;
/// Default daily-file retention in days (Python `retention_days=30`).
pub const DEFAULT_RETENTION_DAYS: i64 = 30;
/// Default log file prefix (Python `path.stem` / `"nextsync"`).
pub const DEFAULT_PREFIX: &str = "nextsync";
/// How many trailing bytes to read per backward seek when tailing a file.
const TAIL_BLOCK_SIZE: usize = 8 * 1024;

/// Options controlling a [`LogBuffer`].
#[derive(Clone)]
pub struct LogBufferOptions {
    /// Directory holding the daily log files.
    pub directory: PathBuf,
    /// File name prefix (`<prefix>-YYYY-MM-DD.log`).
    pub prefix: String,
    /// Whether `append` also writes to disk.
    pub save_to_disk: bool,
    /// Number of daily files to keep (clamped to `1..=365`).
    pub retention_days: i64,
    /// Maximum lines kept in memory for `tail`/`recent_lines` without disk.
    pub live_history_lines: usize,
    /// Returns the current day as `YYYY-MM-DD`. Defaults to UTC.
    pub date_provider: Option<Rc<dyn Fn() -> String>>,
}

impl Default for LogBufferOptions {
    fn default() -> Self {
        Self {
            directory: state_dir(),
            prefix: DEFAULT_PREFIX.to_owned(),
            save_to_disk: true,
            retention_days: DEFAULT_RETENTION_DAYS,
            live_history_lines: DEFAULT_LIVE_HISTORY_LINES,
            date_provider: None,
        }
    }
}

/// Shared state behind a [`LogBuffer`] (single-threaded, like the rest of the
/// GTK glue).
struct Inner {
    directory: PathBuf,
    prefix: String,
    save_to_disk: bool,
    retention_days: i64,
    live_history_lines: usize,
    date_provider: Rc<dyn Fn() -> String>,
    live_history: VecDeque<String>,
    active_date: Option<String>,
    stream: Option<BufWriter<File>>,
}

/// Opaque handle that cancels a log subscription when `unsubscribe` is called.
///
/// Dropping the handle without calling `unsubscribe` keeps the subscription
/// active, exactly like the Python unsubscribe callable.
pub struct Subscription {
    unsubscribe_fn: Option<Box<dyn FnOnce()>>,
}

impl Subscription {
    /// Stop receiving lines. Idempotent.
    pub fn unsubscribe(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe_fn.take() {
            unsubscribe();
        }
    }
}

/// A subscriber to new log lines.
type LogListener = Box<dyn Fn(&str) + 'static>;

/// The application logger: live subscribers, in-memory history and optional
/// daily files.
///
/// Cloning shares the same underlying buffer/subscribers (`Rc<RefCell>`), so a
/// window can hold one clone while the scheduler appends through another.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Rc<RefCell<Inner>>,
    listeners: Rc<RefCell<Vec<(usize, LogListener)>>>,
    next_id: Rc<Cell<usize>>,
}

impl LogBuffer {
    /// Create a logger with default options (state dir, prefix `nextsync`,
    /// disk writes enabled, 30-day retention).
    pub fn new() -> Self {
        Self::with_options(LogBufferOptions::default())
    }

    /// Create a logger with explicit options (used by the UI and tests).
    pub fn with_options(options: LogBufferOptions) -> Self {
        let date_provider = options
            .date_provider
            .unwrap_or_else(|| Rc::new(utc_date_string));
        Self {
            inner: Rc::new(RefCell::new(Inner {
                directory: options.directory,
                prefix: options.prefix,
                save_to_disk: options.save_to_disk,
                retention_days: options.retention_days.clamp(1, 365),
                live_history_lines: options.live_history_lines,
                date_provider,
                live_history: VecDeque::with_capacity(options.live_history_lines.min(1024)),
                active_date: None,
                stream: None,
            })),
            listeners: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(Cell::new(1)),
        }
    }

    /// The directory holding the daily log files.
    pub fn directory(&self) -> PathBuf {
        self.inner.borrow().directory.clone()
    }

    /// Reconfigure disk writes and retention. `retention_days` is clamped to
    /// `1..=365`; disabling disk writes closes the current daily file.
    pub fn configure(&self, save_to_disk: bool, retention_days: i64) {
        let retention_days = retention_days.clamp(1, 365);
        let mut inner = self.inner.borrow_mut();
        if inner.save_to_disk == save_to_disk && inner.retention_days == retention_days {
            return;
        }
        if inner.save_to_disk && !save_to_disk {
            inner.stream = None;
            inner.active_date = None;
        }
        inner.save_to_disk = save_to_disk;
        inner.retention_days = retention_days;
    }

    /// Subscribe to new lines. The callback is invoked synchronously for every
    /// appended line (backpressure is implicit: a slow subscriber blocks
    /// `append` and the history is bounded). Returns a handle to unsubscribe.
    pub fn subscribe(&self, callback: impl Fn(&str) + 'static) -> Subscription {
        let id = {
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            self.listeners.borrow_mut().push((id, Box::new(callback)));
            id
        };
        let listeners = Rc::clone(&self.listeners);
        Subscription {
            unsubscribe_fn: Some(Box::new(move || {
                listeners
                    .borrow_mut()
                    .retain(|(existing, _)| *existing != id);
            })),
        }
    }

    /// Append a formatted line to the log: redacted, kept in memory, written to
    /// disk (when enabled) and broadcast to subscribers.
    pub fn append(&self, line: &str) {
        let safe = Redact::redact_line(line);
        {
            let mut inner = self.inner.borrow_mut();
            if inner.live_history.len() == inner.live_history_lines {
                inner.live_history.pop_front();
            }
            inner.live_history.push_back(safe.clone());
        }
        self.emit_to_disk(&safe);

        let ids: Vec<usize> = self.listeners.borrow().iter().map(|(id, _)| *id).collect();
        let listeners = Rc::clone(&self.listeners);
        for id in ids {
            let borrowed = listeners.borrow();
            let listener = borrowed
                .iter()
                .find(|(existing, _)| *existing == id)
                .map(|(_, listener)| listener);
            if let Some(listener) = listener {
                listener(&safe);
            }
        }
    }

    /// The last `limit` lines from the in-memory history (never reads disk).
    pub fn recent_lines(&self, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let inner = self.inner.borrow();
        let start = inner.live_history.len().saturating_sub(limit);
        inner.live_history.iter().skip(start).cloned().collect()
    }

    /// The last `limit` lines: from the daily files when disk writes are
    /// enabled (newest files first), otherwise from the in-memory history. Any
    /// read failure falls back to the in-memory history, like the Python
    /// `try/except OSError`.
    pub fn tail(&self, limit: usize) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let directory = self.inner.borrow().directory.clone();
        let prefix = self.inner.borrow().prefix.clone();
        let save_to_disk = self.inner.borrow().save_to_disk;
        if !save_to_disk {
            return self.recent_lines(limit);
        }
        let paths = match log_paths(&directory, &prefix) {
            Ok(paths) => paths,
            Err(_) => return self.recent_lines(limit),
        };
        let mut content: Vec<String> = Vec::new();
        let mut remaining = limit;
        for path in paths.iter().rev() {
            let selected = match tail_path(path, remaining, TAIL_BLOCK_SIZE) {
                Some(lines) => lines,
                None => return self.recent_lines(limit),
            };
            let selected_len = selected.len();
            let mut merged = selected;
            merged.extend(content);
            content = merged;
            remaining = remaining.saturating_sub(selected_len);
            if remaining == 0 {
                break;
            }
        }
        if content.len() > limit {
            let keep = content.len() - limit;
            content.drain(..keep);
        }
        content
    }

    /// Number of lines currently kept in the in-memory history.
    pub fn line_count(&self) -> usize {
        self.inner.borrow().live_history.len()
    }

    /// Close the daily file and clear listeners and history (shutdown path and
    /// test teardown, mirroring `AppLogger.close`).
    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.stream = None;
        inner.active_date = None;
        inner.live_history.clear();
        self.listeners.borrow_mut().clear();
    }

    fn emit_to_disk(&self, line: &str) {
        let mut inner = self.inner.borrow_mut();
        if !inner.save_to_disk {
            return;
        }
        let date = (inner.date_provider)();
        let needs_open =
            inner.stream.is_none() || inner.active_date.as_deref() != Some(date.as_str());
        if needs_open && open_for_date(&mut inner, &date).is_err() {
            return;
        }
        if let Some(stream) = inner.stream.as_mut() {
            let _ = writeln!(stream, "{line}");
            let _ = stream.flush();
        }
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// The set of daily log files (`<prefix>-YYYY-MM-DD.log`) sorted by name.
fn log_paths(directory: &Path, prefix: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_daily_log_name(&name, prefix) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

/// Whether `name` matches `<prefix>-YYYY-MM-DD.log` (no regex crate; manual
/// shape check, mirroring the Python `glob`).
fn is_daily_log_name(name: &str, prefix: &str) -> bool {
    let expected = format!("{prefix}-");
    let Some(rest) = name.strip_prefix(&expected) else {
        return false;
    };
    // rest must be "YYYY-MM-DD.log" (14 bytes).
    let bytes = rest.as_bytes();
    if bytes.len() != 14
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !rest[10..].eq(".log")
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    true
}

/// Open (append, create, mode 0600) the file for `date`, closing the previous
/// one, and prune files beyond the retention window.
fn open_for_date(inner: &mut Inner, date: &str) -> std::io::Result<()> {
    fs::create_dir_all(&inner.directory)?;
    fs::set_permissions(&inner.directory, fs::Permissions::from_mode(0o700))?;
    inner.stream = None;
    inner.active_date = Some(date.to_owned());
    let path = inner.directory.join(format!("{}-{date}.log", inner.prefix));
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)?;
    inner.stream = Some(BufWriter::new(file));
    prune(inner);
    Ok(())
}

/// Remove daily files beyond `retention_days`, keeping at least the newest one.
fn prune(inner: &mut Inner) {
    let paths = match log_paths(&inner.directory, &inner.prefix) {
        Ok(paths) => paths,
        Err(_) => return,
    };
    let keep = inner.retention_days.max(1) as usize;
    if paths.len() > keep {
        for path in &paths[..paths.len() - keep] {
            let _ = fs::remove_file(path);
        }
    }
}

/// Read the last `lines` lines of `path` by scanning backwards in blocks,
/// decoding as UTF-8 with replacement (mirrors `AppLogger._tail_path`).
fn tail_path(path: &Path, lines: usize, block_size: usize) -> Option<Vec<String>> {
    if lines == 0 {
        return Some(Vec::new());
    }
    let file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut position = length;
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut newline_count = 0usize;
    while position > 0 && newline_count <= lines {
        let size = block_size.min(position as usize);
        position -= size as u64;
        let mut buffer = vec![0u8; size];
        if file.read_at(&mut buffer, position).unwrap_or(0) != size {
            return None;
        }
        newline_count += buffer.iter().filter(|byte| **byte == b'\n').count();
        chunks.push(buffer);
    }
    let mut data: Vec<u8> = Vec::with_capacity(chunks.len() * block_size);
    for chunk in chunks.iter().rev() {
        data.extend_from_slice(chunk);
    }
    let text = String::from_utf8_lossy(&data);
    let selected: Vec<String> = text
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(str::to_owned)
        .collect();
    Some(selected)
}

/// Today's date as `YYYY-MM-DD` in UTC.
fn utc_date_string() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    civil_date_from_seconds(seconds)
}

/// `YYYY-MM-DD` for a Unix timestamp, computed from the days since the epoch
/// (Howard Hinnant's `civil_from_days`, no `chrono` dependency).
fn civil_date_from_seconds(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn options(directory: &Path) -> LogBufferOptions {
        LogBufferOptions {
            directory: directory.to_path_buf(),
            prefix: "test".to_owned(),
            ..Default::default()
        }
    }

    fn provider(day: Rc<RefCell<String>>) -> Rc<dyn Fn() -> String> {
        Rc::new(move || day.borrow().clone())
    }

    fn daily_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                (name.starts_with("test-") && name.ends_with(".log")).then_some(name)
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn subscribe_receives_each_line_and_unsubscribe_stops() {
        let directory = tempfile::tempdir().unwrap();
        let logger = LogBuffer::with_options(options(directory.path()));
        let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let mut subscription = {
            let received = received.clone();
            logger.subscribe(move |line| received.borrow_mut().push(line.to_owned()))
        };
        logger.append("first");
        logger.append("second");
        assert_eq!(
            *received.borrow(),
            vec!["first".to_owned(), "second".to_owned()]
        );
        subscription.unsubscribe();
        logger.append("third");
        assert_eq!(
            *received.borrow(),
            vec!["first".to_owned(), "second".to_owned()]
        );
        logger.close();
    }

    #[test]
    fn tail_without_disk_returns_live_lines() {
        let directory = tempfile::tempdir().unwrap();
        let logger = LogBuffer::with_options(LogBufferOptions {
            save_to_disk: false,
            ..options(directory.path())
        });
        logger.append("live only");
        assert!(logger.line_count() == 1);
        assert_eq!(logger.tail(500), vec!["live only".to_owned()]);
        assert_eq!(daily_names(directory.path()), Vec::<String>::new());
        logger.close();
    }

    #[test]
    fn tail_with_limit_zero_returns_empty() {
        let directory = tempfile::tempdir().unwrap();
        let logger = LogBuffer::with_options(options(directory.path()));
        logger.append("one");
        assert_eq!(logger.tail(0), Vec::<String>::new());
        assert_eq!(logger.recent_lines(0), Vec::<String>::new());
        logger.close();
    }

    #[test]
    fn live_history_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let logger = LogBuffer::with_options(LogBufferOptions {
            save_to_disk: false,
            live_history_lines: 3,
            ..options(directory.path())
        });
        for line in ["one", "two", "three", "four", "five"] {
            logger.append(line);
        }
        assert_eq!(logger.line_count(), 3);
        assert_eq!(
            logger.tail(10),
            vec!["three".to_owned(), "four".to_owned(), "five".to_owned()]
        );
        logger.close();
    }

    #[test]
    fn append_redacts_through_redact_line() {
        let directory = tempfile::tempdir().unwrap();
        let logger = LogBuffer::with_options(LogBufferOptions {
            save_to_disk: false,
            ..options(directory.path())
        });
        let line = "https://user:password@example.com/sync";
        logger.append(line);
        // The buffer must expose exactly what Redact::redact_line produces for
        // the same input (the contract the redactor will enforce when Fase 1
        // secrets land).
        assert_eq!(logger.tail(10), vec![Redact::redact_line(line)]);
        logger.close();
    }

    #[test]
    fn tail_reads_only_requested_end_of_daily_logs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("test-2026-08-06.log"),
            (0..10_000)
                .map(|index| format!("old-{index}"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        fs::write(
            root.join("test-2026-08-07.log"),
            (0..20)
                .map(|index| format!("new-{index}"))
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let logger = LogBuffer::with_options(options(root));
        let tail = logger.tail(25);
        assert_eq!(
            tail[..5],
            (9_995..10_000)
                .map(|index| format!("old-{index}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            tail[5..],
            (0..20)
                .map(|index| format!("new-{index}"))
                .collect::<Vec<_>>()
        );
        logger.close();
    }

    #[test]
    fn creates_one_file_per_day_and_prunes_by_retention() {
        let directory = tempfile::tempdir().unwrap();
        let current = Rc::new(RefCell::new("2026-08-05".to_owned()));
        let logger = LogBuffer::with_options(LogBufferOptions {
            save_to_disk: true,
            retention_days: 2,
            date_provider: Some(provider(current.clone())),
            ..options(directory.path())
        });
        for day in ["2026-08-05", "2026-08-06", "2026-08-07"] {
            *current.borrow_mut() = day.to_owned();
            logger.append("Day");
        }
        logger.close();
        assert_eq!(
            daily_names(directory.path()),
            vec![
                "test-2026-08-06.log".to_owned(),
                "test-2026-08-07.log".to_owned()
            ]
        );
    }

    #[test]
    fn single_file_legacy_log_is_not_loaded() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("test.log"), "legacy entry\n").unwrap();
        let logger = LogBuffer::with_options(options(directory.path()));
        assert!(!logger
            .tail(10)
            .iter()
            .any(|line| line.contains("legacy entry")));
        logger.close();
    }

    #[test]
    fn configure_clamps_retention_and_can_disable_disk() {
        let directory = tempfile::tempdir().unwrap();
        let current = Rc::new(RefCell::new("2026-08-05".to_owned()));
        let logger = LogBuffer::with_options(LogBufferOptions {
            save_to_disk: true,
            date_provider: Some(provider(current.clone())),
            ..options(directory.path())
        });
        logger.configure(true, 999);
        logger.append("one");
        logger.configure(false, 0);
        logger.append("two");
        logger.close();
        assert_eq!(
            daily_names(directory.path()),
            vec!["test-2026-08-05.log".to_owned()]
        );
    }

    #[test]
    fn close_clears_history_and_listeners() {
        let directory = tempfile::tempdir().unwrap();
        let logger = LogBuffer::with_options(options(directory.path()));
        let received: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let _subscription = {
            let received = received.clone();
            logger.subscribe(move |line| received.borrow_mut().push(line.to_owned()))
        };
        logger.append("before");
        logger.close();
        assert_eq!(logger.line_count(), 0);
        logger.append("after");
        assert_eq!(*received.borrow(), vec!["before".to_owned()]);
    }

    #[test]
    fn utc_date_at_epoch_is_1970_01_01() {
        assert_eq!(civil_date_from_seconds(0), "1970-01-01");
    }

    #[test]
    fn utc_date_known_day() {
        // 2026-01-01 00:00:00 UTC (verified with `date -u -d '2026-01-01' +%s`).
        assert_eq!(civil_date_from_seconds(1_767_225_600), "2026-01-01");
    }
}
