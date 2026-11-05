//! Debounce gate that serializes local file-system feedback.
//!
//! Fase 2 (Task 2.2): local events arrive in bursts, so this gate collapses
//! them into a single `on_ready` call after a quiet window (2 s) and then
//! holds a cooldown (4 s) so the next reconciliation does not start
//! immediately after the previous one. Mirrors `core/debounce.py`.
//!
//! Timing goes through the [`TimeoutSource`] trait so the scheduler and the
//! gate are testable without a GLib main loop: production uses
//! [`GlibTimeoutSource`], tests use the fake in this module.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// Backend that can schedule and cancel one-shot/repeating timers.
///
/// Callbacks run on the caller's main loop; they must not run while the
/// source itself is borrowed. Implemented over GLib in production and by the
/// fake in this module for tests.
pub trait TimeoutSource {
    /// Run `callback` once after `delay`; returns a token for `cancel`.
    fn add_timeout(&mut self, delay: Duration, callback: Box<dyn FnOnce()>) -> u64;

    /// Run `callback` every `interval`, until cancelled.
    fn add_repeating(&mut self, interval: Duration, callback: Box<dyn Fn()>) -> u64;

    /// Run `callback` on the next idle moment of the loop.
    fn add_idle(&mut self, callback: Box<dyn FnOnce()>) -> u64;

    /// Cancel a pending timer. Returns whether it was still scheduled.
    fn cancel(&mut self, id: u64) -> bool;
}

/// [`TimeoutSource`] over the GLib main loop (`timeout_add_local` /
/// `idle_add_local`), safe to call from the UI thread.
pub struct GlibTimeoutSource {
    next_id: u64,
    sources: std::collections::HashMap<u64, glib::SourceId>,
}

impl GlibTimeoutSource {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            sources: std::collections::HashMap::new(),
        }
    }

    fn register(&mut self, source: glib::SourceId) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.sources.insert(id, source);
        id
    }
}

impl Default for GlibTimeoutSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeoutSource for GlibTimeoutSource {
    fn add_timeout(&mut self, delay: Duration, callback: Box<dyn FnOnce()>) -> u64 {
        let mut pending = Some(callback);
        let source = glib::timeout_add_local(delay, move || {
            if let Some(callback) = pending.take() {
                callback();
            }
            glib::ControlFlow::Break
        });
        self.register(source)
    }

    fn add_repeating(&mut self, interval: Duration, callback: Box<dyn Fn()>) -> u64 {
        let source = glib::timeout_add_local(interval, move || {
            callback();
            glib::ControlFlow::Continue
        });
        self.register(source)
    }

    fn add_idle(&mut self, callback: Box<dyn FnOnce()>) -> u64 {
        let mut pending = Some(callback);
        let source = glib::idle_add_local(move || {
            if let Some(callback) = pending.take() {
                callback();
            }
            glib::ControlFlow::Break
        });
        self.register(source)
    }

    fn cancel(&mut self, id: u64) -> bool {
        if let Some(source) = self.sources.remove(&id) {
            source.remove();
            true
        } else {
            false
        }
    }
}

/// Serializes local feedback into a single delayed start.
#[derive(Clone)]
pub struct DebounceGate {
    inner: Rc<RefCell<DebounceInner>>,
}

struct DebounceInner {
    source: Rc<RefCell<dyn TimeoutSource>>,
    debounce: Duration,
    cooldown: Duration,
    on_ready: Rc<dyn Fn()>,
    debounce_source: Option<u64>,
    cooldown_source: Option<u64>,
    cooldown_callback: Option<Rc<dyn Fn()>>,
}

impl DebounceGate {
    pub fn new(
        debounce_ms: u64,
        cooldown_seconds: u64,
        on_ready: impl Fn() + 'static,
        source: Rc<RefCell<dyn TimeoutSource>>,
    ) -> Self {
        Self {
            inner: Rc::new(RefCell::new(DebounceInner {
                source,
                debounce: Duration::from_millis(debounce_ms),
                cooldown: Duration::from_secs(cooldown_seconds),
                on_ready: Rc::new(on_ready),
                debounce_source: None,
                cooldown_source: None,
                cooldown_callback: None,
            })),
        }
    }

    /// Restart the debounce window: any previous debounce timer is cancelled
    /// and a fresh one is armed. A running cooldown is left untouched.
    pub fn kick(&self) {
        let weak = Rc::downgrade(&self.inner);
        let mut inner = self.inner.borrow_mut();
        if let Some(id) = inner.debounce_source.take() {
            inner.source.borrow_mut().cancel(id);
        }
        let debounce = inner.debounce;
        let id = inner.source.borrow_mut().add_timeout(
            debounce,
            Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    let on_ready = {
                        let mut inner = inner.borrow_mut();
                        inner.debounce_source = None;
                        inner.on_ready.clone()
                    };
                    on_ready();
                }
            }),
        );
        inner.debounce_source = Some(id);
    }

    /// Whether the cooldown from the previous sync is still running.
    pub fn in_cooldown(&self) -> bool {
        self.inner.borrow().cooldown_source.is_some()
    }

    /// Start the post-sync cooldown and schedule `on_finished`.
    pub fn begin_cooldown(&self, on_finished: impl Fn() + 'static) {
        let weak = Rc::downgrade(&self.inner);
        let mut inner = self.inner.borrow_mut();
        inner.cooldown_callback = Some(Rc::new(on_finished));
        let cooldown = inner.cooldown;
        let id = inner.source.borrow_mut().add_timeout(
            cooldown,
            Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    let callback = {
                        let mut inner = inner.borrow_mut();
                        inner.cooldown_source = None;
                        inner.cooldown_callback.take()
                    };
                    if let Some(callback) = callback {
                        callback();
                    }
                }
            }),
        );
        inner.cooldown_source = Some(id);
    }

    /// Cancel both the debounce and the cooldown timers.
    pub fn stop(&self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(id) = inner.debounce_source.take() {
            inner.source.borrow_mut().cancel(id);
        }
        if let Some(id) = inner.cooldown_source.take() {
            inner.source.borrow_mut().cancel(id);
        }
        inner.cooldown_callback = None;
    }
}

/// Deterministic [`TimeoutSource`] for tests: timers are stored and fired
/// explicitly by id, mirroring the `FakeGLib` of the Python tests.
#[cfg(test)]
#[derive(Default)]
pub struct FakeTimeoutSource {
    entries: std::collections::HashMap<u64, FakeEntry>,
    next_id: u64,
}

#[cfg(test)]
enum FakeEntry {
    OneShot(Option<Box<dyn FnOnce()>>),
    Repeating(Box<dyn Fn()>),
}

#[cfg(test)]
impl FakeTimeoutSource {
    /// Number of currently scheduled timers.
    pub fn pending(&self) -> usize {
        self.entries.len()
    }

    /// Whether a timer with the given id is scheduled.
    pub fn has(&self, id: u64) -> bool {
        self.entries.contains_key(&id)
    }

    /// Ids of all scheduled timers.
    pub fn ids(&self) -> Vec<u64> {
        self.entries.keys().copied().collect()
    }

    /// The id of the single currently scheduled timer (panics otherwise).
    pub fn only_id(&self) -> u64 {
        let ids = self.ids();
        assert_eq!(
            ids.len(),
            1,
            "expected exactly one pending timer, got {}",
            ids.len()
        );
        ids[0]
    }
}

#[cfg(test)]
impl TimeoutSource for FakeTimeoutSource {
    fn add_timeout(&mut self, _delay: Duration, callback: Box<dyn FnOnce()>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.insert(id, FakeEntry::OneShot(Some(callback)));
        id
    }

    fn add_repeating(&mut self, _interval: Duration, callback: Box<dyn Fn()>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.insert(id, FakeEntry::Repeating(callback));
        id
    }

    fn add_idle(&mut self, callback: Box<dyn FnOnce()>) -> u64 {
        self.add_timeout(Duration::ZERO, callback)
    }

    fn cancel(&mut self, id: u64) -> bool {
        self.entries.remove(&id).is_some()
    }
}

/// Run the timer with the given id (panics if it does not exist).
///
/// The borrow on the fake is released before the callback runs, so callbacks
/// may schedule new timers (mirroring how the GLib loop hands control back).
#[cfg(test)]
pub fn fire_timer(source: &Rc<RefCell<FakeTimeoutSource>>, id: u64) {
    let callback = {
        let mut inner = source.borrow_mut();
        inner.entries.remove(&id)
    };
    match callback {
        Some(FakeEntry::OneShot(mut callback)) => {
            if let Some(callback) = callback.take() {
                callback();
            }
        }
        Some(FakeEntry::Repeating(callback)) => {
            callback();
            source
                .borrow_mut()
                .entries
                .insert(id, FakeEntry::Repeating(callback));
        }
        None => panic!("no fake timer with id {id}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_source() -> Rc<RefCell<FakeTimeoutSource>> {
        Rc::new(RefCell::new(FakeTimeoutSource::default()))
    }

    #[test]
    fn restarting_debounce_cancels_the_previous_timer() {
        let source = fake_source();
        let gate = DebounceGate::new(2000, 4, || {}, source.clone());
        gate.kick();
        let first = source.borrow().pending();
        assert_eq!(first, 1);
        gate.kick();
        assert_eq!(source.borrow().pending(), 1);
    }

    #[test]
    fn debounce_elapsed_triggers_ready() {
        let source = fake_source();
        let fired = Rc::new(RefCell::new(0));
        let gate = DebounceGate::new(
            2000,
            4,
            {
                let fired = Rc::clone(&fired);
                move || *fired.borrow_mut() += 1
            },
            source.clone(),
        );
        gate.kick();
        let id = source.borrow().only_id();
        fire_timer(&source, id);
        assert_eq!(*fired.borrow(), 1);
        assert_eq!(source.borrow().pending(), 0);
    }

    #[test]
    fn cooldown_locks_until_it_expires() {
        let source = fake_source();
        let gate = DebounceGate::new(2000, 4, || {}, source.clone());
        assert!(!gate.in_cooldown());
        gate.begin_cooldown(|| {});
        assert!(gate.in_cooldown());
        let id = source.borrow().only_id();
        fire_timer(&source, id);
        assert!(!gate.in_cooldown());
    }

    #[test]
    fn cooldown_runs_its_finished_callback() {
        let source = fake_source();
        let gate = DebounceGate::new(2000, 4, || {}, source.clone());
        let fired = Rc::new(RefCell::new(false));
        gate.begin_cooldown({
            let fired = Rc::clone(&fired);
            move || *fired.borrow_mut() = true
        });
        let id = source.borrow().only_id();
        fire_timer(&source, id);
        assert!(*fired.borrow());
    }

    #[test]
    fn stop_cancels_both_sources() {
        let source = fake_source();
        let gate = DebounceGate::new(2000, 4, || {}, source.clone());
        gate.kick();
        gate.begin_cooldown(|| {});
        assert_eq!(source.borrow().pending(), 2);
        gate.stop();
        assert_eq!(source.borrow().pending(), 0);
        assert!(!gate.in_cooldown());
    }

    #[test]
    fn kick_during_cooldown_still_arms_a_debounce() {
        let source = fake_source();
        let gate = DebounceGate::new(2000, 4, || {}, source.clone());
        gate.begin_cooldown(|| {});
        gate.kick();
        assert_eq!(source.borrow().pending(), 2);
    }
}
