//! Local filesystem watcher.
//!
//! Fase 3 (Task 3.1): recursive inotify watching over the `notify` crate
//! (backed by `INotifyWatcher` on Linux). Events are classified in the watcher
//! thread and pushed into a bounded `async_channel`; the main loop drains it
//! and turns every [`WatcherEvent::Change`] into a
//! [`Trigger::LocalInotify`](crate::core::triggers::Trigger::LocalInotify)
//! request on the scheduler.
//!
//! Mirrors `core/inotify.py`: the same event mask (create, modify, move,
//! remove, attribute, self-delete — everything except pure access) and the
//! same failure modes. `notify` collapses an inotify queue overflow into an
//! `Other` event flagged `Rescan`, which becomes a [`WatcherEvent::Degraded`]
//! with [`WatcherError::Overflow`]; the consumer should react by calling
//! [`FsWatcher::rescan`] and scheduling a full reconciliation, since the event
//! history is incomplete.

use std::path::{Path, PathBuf};

use async_channel::{Receiver, Sender};
use notify::event::{CreateKind, Flag, RemoveKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::core::exclusions::ExclusionMatcher;

/// One event emitted by the filesystem watcher, consumed on the main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherEvent {
    /// A file or directory changed, was created or was removed.
    Change(PathBuf),
    /// The watcher lost the event stream and must fall back to a full scan.
    Degraded(WatcherError),
    /// The consumer asked for a fresh recursive registration (`rescan`).
    Rescan,
}

/// Why the watcher degraded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatcherError {
    /// The inotify event queue overflowed; local event history is incomplete.
    Overflow,
    /// The system inotify watch limit was reached.
    WatchLimit(String),
    /// The backend is unavailable (wrong path, unsupported platform, …).
    Unavailable(String),
    /// An I/O error while watching or re-registering paths.
    Io(String),
}

impl From<notify::Error> for WatcherError {
    fn from(error: notify::Error) -> Self {
        match error.kind {
            notify::ErrorKind::MaxFilesWatch => WatcherError::WatchLimit(error.to_string()),
            notify::ErrorKind::Io(io_error) => WatcherError::Io(io_error.to_string()),
            notify::ErrorKind::Generic(message) => WatcherError::Unavailable(message),
            notify::ErrorKind::PathNotFound => {
                WatcherError::Unavailable("path not found".to_string())
            }
            notify::ErrorKind::WatchNotFound => {
                WatcherError::Unavailable("watch not found".to_string())
            }
            notify::ErrorKind::InvalidConfig(_) => {
                WatcherError::Unavailable("invalid watcher configuration".to_string())
            }
        }
    }
}

/// Recursive filesystem watcher for one sync folder.
///
/// The `notify` handler runs on its own thread; the returned
/// [`Receiver<WatcherEvent>`] is the only way the main loop talks to it.
#[derive(Debug)]
pub struct FsWatcher {
    root: PathBuf,
    watcher: RecommendedWatcher,
    sender: Sender<WatcherEvent>,
    /// Set by the notify callback when the bounded channel is full and an
    /// event was dropped. Read and cleared by the consumer; it does not
    /// compete for the full buffer, so the overflow is never lost (issue
    /// #134).
    overflow: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FsWatcher {
    /// Start watching `root` recursively. The caller keeps the `Receiver`
    /// alive; dropping the watcher stops the thread.
    pub fn start(
        root: impl AsRef<Path>,
        matcher: ExclusionMatcher,
    ) -> Result<(Self, Receiver<WatcherEvent>), WatcherError> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            return Err(WatcherError::Unavailable(format!(
                "watch root is not a directory: {}",
                root.display()
            )));
        }
        let (sender, receiver) = async_channel::bounded(1024);
        let event_sender = sender.clone();
        let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overflow_signal = overflow.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: Result<Event, notify::Error>| {
                let event = match result {
                    Ok(event) => classify_event(&event, &matcher),
                    Err(error) => Some(WatcherEvent::Degraded(WatcherError::from(error))),
                };
                if let Some(event) = event {
                    if event_sender.try_send(event).is_err() {
                        // Backpressure: the consumer fell behind and the bounded
                        // channel dropped history. Set the overflow flag instead
                        // of trying to send again into the same full buffer (that
                        // second send could also fail and the rescan request
                        // would be lost): the consumer polls it and rescans,
                        // avoiding a sync from a partial event stream.
                        overflow_signal.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            })
            .map_err(WatcherError::from)?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(WatcherError::from)?;
        Ok((
            Self {
                root,
                watcher,
                sender,
                overflow,
            },
            receiver,
        ))
    }

    /// The watched root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether a watch is registered (always `true` after a successful
    /// [`start`](Self::start)).
    pub fn is_active(&self) -> bool {
        true
    }

    /// Re-register the whole tree. Used as the fallback after a
    /// [`WatcherError::Overflow`]: events arriving while the queue overflowed
    /// may be missing, so a fresh recursive watch plus a [`WatcherEvent::Rescan`]
    /// lets the consumer schedule a full reconciliation.
    pub fn rescan(&mut self) {
        let _ = self.watcher.unwatch(&self.root);
        match self.watcher.watch(&self.root, RecursiveMode::Recursive) {
            Ok(()) => {
                let _ = self.sender.try_send(WatcherEvent::Rescan);
            }
            Err(error) => {
                let _ = self
                    .sender
                    .try_send(WatcherEvent::Degraded(WatcherError::from(error)));
            }
        }
    }

    /// Whether the notify callback reported an overflow since the last poll.
    /// The consumer calls this on every received event and rescans when it
    /// reads `true`, so a full buffer never silently loses the rescan
    /// request (issue #134).
    pub fn take_overflow(&self) -> bool {
        self.overflow
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }
}

/// Map a `notify` event to a [`WatcherEvent`].
///
/// Mutating events (create, modify, remove) become changes; `notify` reports
/// an inotify overflow as `Other` with the `Rescan` flag; access and other
/// meta events are ignored. Excluded names are skipped for file events, like
/// `inotify.py` does (directory events always pass, so a folder matching an
/// exclude pattern still gets watched).
pub fn classify_event(event: &Event, matcher: &ExclusionMatcher) -> Option<WatcherEvent> {
    // `notify` reports an inotify queue overflow as an `Other` event flagged
    // `Rescan`: local event history is incomplete, so the consumer must
    // rescan. A bare `Other` is a meta event and is ignored below.
    if matches!(event.flag(), Some(Flag::Rescan)) {
        return Some(WatcherEvent::Degraded(WatcherError::Overflow));
    }
    let path = event.paths.first()?;
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any => {
            if !is_directory_event(event) {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if matcher.matches_name(&name) {
                    return None;
                }
            }
            Some(WatcherEvent::Change(path.clone()))
        }
        EventKind::Access(_) | EventKind::Other => None,
    }
}

/// Whether the event describes a directory (kind says so), so the exclusion
/// matcher must not swallow it.
fn is_directory_event(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(CreateKind::Folder | CreateKind::Any)
            | EventKind::Remove(RemoveKind::Folder | RemoveKind::Any)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::rc::Rc;
    use std::time::Duration;
    use tempfile::tempdir;

    fn empty_matcher() -> ExclusionMatcher {
        ExclusionMatcher::new(Vec::<String>::new(), true)
    }

    /// Drain events until one `Change` satisfies `predicate` (5 s budget).
    fn next_change(
        receiver: &Receiver<WatcherEvent>,
        predicate: impl Fn(&Path) -> bool,
    ) -> PathBuf {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(WatcherEvent::Change(path)) = receiver.try_recv() {
                if predicate(&path) {
                    return path;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no matching watcher event within 5 s");
    }

    // ---- event classification (unit, mirrors test_inotify_overflow.py) ----

    #[test]
    fn overflow_event_is_reported_as_degraded_overflow() {
        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        let classified = classify_event(&event, &empty_matcher());
        assert_eq!(
            classified,
            Some(WatcherEvent::Degraded(WatcherError::Overflow))
        );
    }

    #[test]
    fn take_overflow_reads_once_and_clears_the_flag() {
        // Issue #134: the overflow flag must survive a full channel (it is
        // set by the notify callback instead of a second try_send into the
        // same full buffer) and be consumed once by the loop.
        let dir = tempdir().unwrap();
        let (watcher, _receiver) = FsWatcher::start(dir.path(), empty_matcher()).unwrap();
        assert!(!watcher.take_overflow());
        watcher
            .overflow
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(watcher.take_overflow());
        assert!(!watcher.take_overflow());
    }

    #[test]
    fn mutating_events_become_changes() {
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            EventKind::Remove(RemoveKind::File),
        ] {
            let event = Event::new(kind).add_path(PathBuf::from("/tmp/x.txt"));
            assert_eq!(
                classify_event(&event, &empty_matcher()),
                Some(WatcherEvent::Change(PathBuf::from("/tmp/x.txt"))),
                "kind {kind:?}"
            );
        }
    }

    #[test]
    fn access_and_other_events_are_ignored() {
        let access = Event::new(EventKind::Access(notify::event::AccessKind::Read))
            .add_path(PathBuf::from("/tmp/x.txt"));
        assert!(classify_event(&access, &empty_matcher()).is_none());
        let other = Event::new(EventKind::Other);
        assert!(classify_event(&other, &empty_matcher()).is_none());
    }

    #[test]
    fn excluded_file_events_are_ignored() {
        let matcher = ExclusionMatcher::new(["*.tmp"], true);
        let event = Event::new(EventKind::Create(CreateKind::File))
            .add_path(PathBuf::from("/tmp/draft.tmp"));
        assert!(classify_event(&event, &matcher).is_none());
        // Directory events pass even when the name matches a pattern.
        let dir = Event::new(EventKind::Create(CreateKind::Folder))
            .add_path(PathBuf::from("/tmp/draft.tmp"));
        assert!(classify_event(&dir, &matcher).is_some());
    }

    #[test]
    fn notify_errors_map_to_watcher_errors() {
        let io_error = notify::Error::io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(matches!(WatcherError::from(io_error), WatcherError::Io(_)));
        let limit = notify::Error::new(notify::ErrorKind::MaxFilesWatch);
        assert!(matches!(
            WatcherError::from(limit),
            WatcherError::WatchLimit(_)
        ));
        let generic = notify::Error::generic("boom");
        assert!(matches!(
            WatcherError::from(generic),
            WatcherError::Unavailable(_)
        ));
    }

    // ---- real filesystem behaviour ----------------------------------------

    #[test]
    fn create_emits_a_change_event() {
        let dir = tempdir().unwrap();
        let (watcher, receiver) = FsWatcher::start(dir.path(), empty_matcher()).unwrap();
        assert!(watcher.is_active());
        fs::write(dir.path().join("hello.txt"), "hi").unwrap();
        let changed = next_change(&receiver, |path| {
            path.file_name().map(|n| n.to_string_lossy().into_owned()) == Some("hello.txt".into())
        });
        assert_eq!(changed.file_name().unwrap(), "hello.txt");
    }

    #[test]
    fn remove_emits_a_change_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("gone.txt");
        fs::write(&path, "x").unwrap();
        let (_watcher, receiver) = FsWatcher::start(dir.path(), empty_matcher()).unwrap();
        // Consume the create events that follow the initial watch, if any.
        let _ = receiver.try_recv();
        fs::remove_file(&path).unwrap();
        next_change(&receiver, |changed| changed == path);
    }

    #[test]
    fn rename_emits_a_change_event() {
        let dir = tempdir().unwrap();
        let from = dir.path().join("old.txt");
        let to = dir.path().join("new.txt");
        fs::write(&from, "x").unwrap();
        let (_watcher, receiver) = FsWatcher::start(dir.path(), empty_matcher()).unwrap();
        let _ = receiver.try_recv();
        fs::rename(&from, &to).unwrap();
        next_change(&receiver, |changed| changed == to);
    }

    #[test]
    fn excluded_names_are_not_forwarded() {
        let dir = tempdir().unwrap();
        let matcher = ExclusionMatcher::new(["*.tmp"], true);
        let (_watcher, receiver) = FsWatcher::start(dir.path(), matcher).unwrap();
        fs::write(dir.path().join("junk.tmp"), "x").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            if let Ok(WatcherEvent::Change(path)) = receiver.try_recv() {
                let name = path.file_name().unwrap().to_string_lossy();
                assert_ne!(name, "junk.tmp");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn missing_root_is_rejected() {
        let err = FsWatcher::start("/nonexistent/watch-root", empty_matcher()).unwrap_err();
        assert!(matches!(err, WatcherError::Unavailable(_)));
    }

    #[test]
    fn rescan_re_registers_and_emits_rescan() {
        let dir = tempdir().unwrap();
        let (mut watcher, receiver) = FsWatcher::start(dir.path(), empty_matcher()).unwrap();
        watcher.rescan();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match receiver.try_recv() {
                Ok(WatcherEvent::Rescan) => break,
                Ok(_) => {}
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        panic!("no rescan event");
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // ---- watcher → scheduler plumbing ---------------------------------------

    #[derive(Clone, Default)]
    struct FakeRunner(Rc<std::cell::RefCell<FakeRunnerState>>);

    #[derive(Default)]
    struct FakeRunnerState {
        start_calls: usize,
        pending: Option<Box<dyn FnOnce(crate::core::scheduler::SyncOutcome)>>,
    }

    impl crate::core::scheduler::SyncRunner for FakeRunner {
        fn start(
            &mut self,
            _reasons: &[crate::core::triggers::Trigger],
            on_finished: Box<dyn FnOnce(crate::core::scheduler::SyncOutcome) + 'static>,
        ) {
            self.0.borrow_mut().start_calls += 1;
            self.0.borrow_mut().pending = Some(on_finished);
        }
    }

    #[test]
    fn watcher_events_trigger_the_scheduler() {
        use crate::core::debounce::{fire_timer, FakeTimeoutSource};
        use crate::core::scheduler::Scheduler;
        use crate::core::triggers::TriggerSettings;
        use crate::state::{AppState, StateController};

        let dir = tempdir().unwrap();
        let (_watcher, receiver) = FsWatcher::start(dir.path(), empty_matcher()).unwrap();

        let source = Rc::new(std::cell::RefCell::new(FakeTimeoutSource::default()));
        let source_dyn: Rc<std::cell::RefCell<dyn crate::core::debounce::TimeoutSource>> =
            source.clone();
        let runner = FakeRunner::default();
        let scheduler = Scheduler::new(
            StateController::new(AppState::IdleOk),
            TriggerSettings {
                local_inotify_enabled: true,
                ..TriggerSettings::default()
            },
            Box::new(runner.clone()),
            source_dyn,
            None,
            None,
        );

        fs::write(dir.path().join("new.txt"), "x").unwrap();
        // The main loop would drain this channel; here we do it inline and
        // forward every change to the scheduler like the app does.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match receiver.try_recv() {
                Ok(WatcherEvent::Change(_)) => {
                    scheduler.request(crate::core::triggers::Trigger::LocalInotify);
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        panic!("no change event within 5 s");
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // Debounce → ready → idle → start: the run begins.
        let id = source.borrow().only_id();
        fire_timer(&source, id);
        let id = source.borrow().only_id();
        fire_timer(&source, id);
        assert_eq!(runner.0.borrow().start_calls, 1);
    }
}
