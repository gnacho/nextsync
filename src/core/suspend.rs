//! Suspend/resume handling.
//!
//! Fase 3 (Task 3.2): resume trigger after the system wakes from sleep.
//! Mirrors `core/suspend.py`, which subscribes to the `PrepareForSleep` signal
//! of `org.freedesktop.login1` and fires `on_resume` (through a 3 s delay, so
//! the network is back) when the system wakes up.
//!
//! The probe is injectable so tests use a fake; production uses
//! [`Login1SuspendProbe`] over the GLib system bus connection. The app wires
//! `on_resume` to [`Scheduler::request`] with a resume trigger.
//!
//! [`Scheduler::request`]: crate::core::scheduler::Scheduler::request

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// Shared callback invoked when the system wakes up.
type ResumeCallback = Rc<dyn Fn()>;

/// Source of system sleep/wake signals. Implemented by [`Login1SuspendProbe`]
/// in production and by fakes in tests.
pub trait SuspendProbe {
    /// Subscribe to wake-up notifications. Returns a token for
    /// [`Self::unsubscribe`] (or `0` when the backend is unavailable).
    fn subscribe(&self, on_resume: Rc<dyn Fn()>) -> u64;

    /// Stop a subscription started with [`Self::subscribe`].
    fn unsubscribe(&self, id: u64);
}

/// [`SuspendProbe`] over the `org.freedesktop.login1` `PrepareForSleep`
/// signal. Same backend and semantics as `suspend.py`:
/// `bus_get_sync(System)` + `signal_subscribe("PrepareForSleep")`, with a
/// 3-second delay before `on_resume` after wake.
#[derive(Default)]
pub struct Login1SuspendProbe {
    connection: Rc<RefCell<Option<gio::DBusConnection>>>,
    subscription: Rc<RefCell<Option<gio::SignalSubscriptionId>>>,
    on_resume: Rc<RefCell<Option<ResumeCallback>>>,
}

impl Login1SuspendProbe {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SuspendProbe for Login1SuspendProbe {
    // `signal_subscribe`/`signal_unsubscribe` are deprecated in favour of the
    // async `subscribe_to_signal` stream, which does not fit this callback
    // style; the deprecated callbacks replicate `suspend.py` directly.
    #[allow(deprecated)]
    fn subscribe(&self, on_resume: Rc<dyn Fn()>) -> u64 {
        *self.on_resume.borrow_mut() = Some(on_resume);
        if self.subscription.borrow().is_some() {
            return 1;
        }
        let connection = match gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>) {
            Ok(connection) => connection,
            Err(_) => return 0,
        };
        let weak = Rc::downgrade(&self.on_resume);
        let id = connection.signal_subscribe(
            Some("org.freedesktop.login1"),
            Some("org.freedesktop.login1.Manager"),
            Some("PrepareForSleep"),
            Some("/org/freedesktop/login1"),
            None,
            gio::DBusSignalFlags::NONE,
            move |_connection, _sender, _object_path, _interface, _signal, parameters| {
                let sleeping = if parameters.n_children() > 0 {
                    parameters.child_value(0).get::<bool>().unwrap_or(false)
                } else {
                    false
                };
                if !sleeping {
                    // Wait a few seconds for the network to come back.
                    if let Some(slot) = weak.upgrade() {
                        let on_resume = slot.borrow().clone();
                        if let Some(on_resume) = on_resume {
                            glib::timeout_add_local(Duration::from_secs(3), move || {
                                on_resume();
                                glib::ControlFlow::Break
                            });
                        }
                    }
                }
            },
        );
        *self.connection.borrow_mut() = Some(connection);
        *self.subscription.borrow_mut() = Some(id);
        1
    }

    #[allow(deprecated)]
    fn unsubscribe(&self, _id: u64) {
        if let Some(id) = self.subscription.borrow_mut().take() {
            if let Some(connection) = self.connection.borrow().as_ref() {
                connection.signal_unsubscribe(id);
            }
            *self.connection.borrow_mut() = None;
        }
        self.on_resume.borrow_mut().take();
    }
}

/// Fires a callback when the system wakes from sleep.
#[derive(Clone)]
pub struct SuspendWatcher {
    inner: Rc<RefCell<SuspendWatcherInner>>,
}

struct SuspendWatcherInner {
    probe: Box<dyn SuspendProbe>,
    on_resume: Option<Rc<dyn Fn()>>,
    subscription: Option<u64>,
}

impl SuspendWatcher {
    /// Create a watcher around the given probe.
    pub fn new(probe: Box<dyn SuspendProbe>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(SuspendWatcherInner {
                probe,
                on_resume: None,
                subscription: None,
            })),
        }
    }

    /// A watcher over the `login1` D-Bus signal.
    pub fn login1() -> Self {
        Self::new(Box::new(Login1SuspendProbe::new()))
    }

    /// Set the callback invoked after wake.
    pub fn set_on_resume(&mut self, callback: impl Fn() + 'static) {
        self.inner.borrow_mut().on_resume = Some(Rc::new(callback));
    }

    /// Subscribe to wake notifications.
    pub fn start(&mut self) {
        let on_resume = self.inner.borrow().on_resume.clone();
        let Some(on_resume) = on_resume else {
            return;
        };
        let id = {
            let inner = self.inner.borrow();
            inner.probe.subscribe(on_resume)
        };
        self.inner.borrow_mut().subscription = if id == 0 { None } else { Some(id) };
    }

    /// Unsubscribe and stop firing the callback.
    pub fn stop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(id) = inner.subscription.take() {
            inner.probe.unsubscribe(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct FakeSuspendProbe(Rc<RefCell<FakeSuspendProbeInner>>);

    #[derive(Default)]
    struct FakeSuspendProbeInner {
        on_resume: Option<Rc<dyn Fn()>>,
        subscribed: bool,
    }

    impl FakeSuspendProbe {
        fn simulate_resume(&self) {
            let on_resume = {
                let inner = self.0.borrow();
                inner.on_resume.clone()
            };
            if let Some(on_resume) = on_resume {
                on_resume();
            }
        }
    }

    impl SuspendProbe for FakeSuspendProbe {
        fn subscribe(&self, on_resume: Rc<dyn Fn()>) -> u64 {
            let mut inner = self.0.borrow_mut();
            inner.on_resume = Some(on_resume);
            inner.subscribed = true;
            1
        }

        fn unsubscribe(&self, _id: u64) {
            let mut inner = self.0.borrow_mut();
            inner.on_resume = None;
            inner.subscribed = false;
        }
    }

    #[test]
    fn resume_is_forwarded_to_the_callback() {
        let probe = FakeSuspendProbe::default();
        let mut watcher = SuspendWatcher::new(Box::new(probe.clone()));
        let resumed = Rc::new(RefCell::new(0));
        watcher.set_on_resume({
            let resumed = Rc::clone(&resumed);
            move || *resumed.borrow_mut() += 1
        });
        watcher.start();
        assert!(probe.0.borrow().subscribed);
        probe.simulate_resume();
        probe.simulate_resume();
        assert_eq!(*resumed.borrow(), 2);
    }

    #[test]
    fn start_without_callback_does_not_subscribe() {
        let probe = FakeSuspendProbe::default();
        let mut watcher = SuspendWatcher::new(Box::new(probe.clone()));
        watcher.start();
        assert!(!probe.0.borrow().subscribed);
    }

    #[test]
    fn stop_unsubscribes() {
        let probe = FakeSuspendProbe::default();
        let mut watcher = SuspendWatcher::new(Box::new(probe.clone()));
        watcher.set_on_resume(|| {});
        watcher.start();
        assert!(probe.0.borrow().subscribed);
        watcher.stop();
        assert!(!probe.0.borrow().subscribed);
    }
}
