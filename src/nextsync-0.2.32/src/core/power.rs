//! Power state watcher.
//!
//! Fase 3 (Task 3.2): pause syncs while running on battery. Mirrors
//! `core/power.py`, which reads UPower's `OnBattery` property; the callback
//! is wired to [`Scheduler::set_battery_paused`] (replicating the
//! `paused_battery` state).
//!
//! The probe is injectable so tests use a fake. Production uses
//! [`SysfsPowerProbe`], which reads the ACPI power supply sysfs tree — the
//! simplest libc-free Linux source for the same signal (a UPower D-Bus
//! subscription is left to the integration phase; `SysfsPowerProbe` re-reads
//! on demand through [`PowerWatcher::refresh`], e.g. after resume).
//!
//! [`Scheduler::set_battery_paused`]: crate::core::scheduler::Scheduler::set_battery_paused

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

/// Source of the battery state. Implemented by [`SysfsPowerProbe`] in
/// production and by fakes in tests.
pub trait PowerProbe {
    /// Whether the system currently runs on battery power.
    fn on_battery(&self) -> bool;

    /// Subscribe to battery-state changes. Returns a token for
    /// [`Self::unsubscribe`]. The default does not track changes: callers may
    /// poll [`Self::on_battery`] through [`PowerWatcher::refresh`] instead.
    fn subscribe(&self, _callback: Rc<dyn Fn(bool)>) -> u64 {
        0
    }

    /// Stop a subscription started with [`Self::subscribe`].
    fn unsubscribe(&self, _id: u64) {}
}

/// [`PowerProbe`] over the ACPI sysfs tree (`/sys/class/power_supply`).
///
/// A power supply whose `type` is `Battery` and whose `status` is
/// `Discharging` means the machine is on battery. A machine without any
/// discharging battery is treated as on mains. The root is injectable so tests
/// can fake a power supply.
#[derive(Debug, Clone)]
pub struct SysfsPowerProbe {
    root: PathBuf,
}

impl SysfsPowerProbe {
    /// The default sysfs power supply directory.
    pub fn new() -> Self {
        Self {
            root: PathBuf::from("/sys/class/power_supply"),
        }
    }

    /// A probe reading from a custom directory (used by tests).
    pub fn with_path(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Default for SysfsPowerProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl PowerProbe for SysfsPowerProbe {
    fn on_battery(&self) -> bool {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        for entry in entries.flatten() {
            let type_path = entry.path().join("type");
            let Ok(type_text) = fs::read_to_string(&type_path) else {
                continue;
            };
            if type_text.trim() != "Battery" {
                continue;
            }
            let status_path = entry.path().join("status");
            let Ok(status_text) = fs::read_to_string(&status_path) else {
                continue;
            };
            if status_text.trim() == "Discharging" {
                return true;
            }
        }
        false
    }
}

/// Watches the battery state and forwards every change to a callback.
#[derive(Clone)]
pub struct PowerWatcher {
    inner: Rc<RefCell<PowerWatcherInner>>,
}

struct PowerWatcherInner {
    probe: Box<dyn PowerProbe>,
    on_change: Option<Rc<dyn Fn(bool)>>,
    on_battery: bool,
    subscription: Option<u64>,
}

impl PowerWatcher {
    /// Create a watcher around the given probe.
    pub fn new(probe: Box<dyn PowerProbe>) -> Self {
        let on_battery = probe.on_battery();
        Self {
            inner: Rc::new(RefCell::new(PowerWatcherInner {
                probe,
                on_change: None,
                on_battery,
                subscription: None,
            })),
        }
    }

    /// A watcher over the ACPI sysfs tree.
    pub fn sysfs() -> Self {
        Self::new(Box::new(SysfsPowerProbe::new()))
    }

    /// Set the callback that receives battery-state changes. The current state
    /// is reported once on [`start`](Self::start).
    pub fn set_callback(&mut self, callback: impl Fn(bool) + 'static) {
        self.inner.borrow_mut().on_change = Some(Rc::new(callback));
    }

    /// Whether the system currently runs on battery.
    pub fn on_battery(&self) -> bool {
        self.inner.borrow().on_battery
    }

    /// Subscribe to the probe and report the current state immediately, like
    /// `power.py`'s `start`.
    pub fn start(&mut self) {
        let weak = Rc::downgrade(&self.inner);
        let id = {
            let inner = self.inner.borrow();
            inner.probe.subscribe(Rc::new(move |on_battery| {
                if let Some(inner) = weak.upgrade() {
                    let mut inner = inner.borrow_mut();
                    if inner.on_battery != on_battery {
                        inner.on_battery = on_battery;
                        if let Some(callback) = &inner.on_change {
                            callback(on_battery);
                        }
                    }
                }
            }))
        };
        let mut inner = self.inner.borrow_mut();
        inner.subscription = if id == 0 { None } else { Some(id) };
        let on_battery = inner.on_battery;
        if let Some(callback) = &inner.on_change {
            callback(on_battery);
        }
    }

    /// Unsubscribe and stop reporting changes.
    pub fn stop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(id) = inner.subscription.take() {
            inner.probe.unsubscribe(id);
        }
    }

    /// Re-read the probe and report if the state changed. Useful with probes
    /// that do not push (like [`SysfsPowerProbe`]), e.g. after resume.
    pub fn refresh(&mut self) {
        let on_battery = {
            let inner = self.inner.borrow();
            inner.probe.on_battery()
        };
        let mut inner = self.inner.borrow_mut();
        if inner.on_battery != on_battery {
            inner.on_battery = on_battery;
            if let Some(callback) = &inner.on_change {
                callback(on_battery);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use tempfile::tempdir;

    #[derive(Clone, Default)]
    struct FakePowerProbe(Rc<RefCell<FakePowerProbeInner>>);

    #[derive(Default)]
    struct FakePowerProbeInner {
        on_battery: bool,
        callback: Option<Rc<dyn Fn(bool)>>,
    }

    impl FakePowerProbe {
        fn simulate(&self, on_battery: bool) {
            let callback = {
                let inner = self.0.borrow();
                inner.callback.clone()
            };
            if let Some(callback) = callback {
                callback(on_battery);
            }
        }
    }

    impl PowerProbe for FakePowerProbe {
        fn on_battery(&self) -> bool {
            self.0.borrow().on_battery
        }

        fn subscribe(&self, callback: Rc<dyn Fn(bool)>) -> u64 {
            self.0.borrow_mut().callback = Some(callback);
            1
        }

        fn unsubscribe(&self, _id: u64) {
            self.0.borrow_mut().callback = None;
        }
    }

    #[test]
    fn start_reports_the_current_state_immediately() {
        let probe = FakePowerProbe::default();
        probe.0.borrow_mut().on_battery = true;
        let mut watcher = PowerWatcher::new(Box::new(probe));
        let seen = Rc::new(RefCell::new(Vec::new()));
        watcher.set_callback({
            let seen = Rc::clone(&seen);
            move |on_battery| seen.borrow_mut().push(on_battery)
        });
        watcher.start();
        assert_eq!(*seen.borrow(), vec![true]);
        assert!(watcher.on_battery());
    }

    #[test]
    fn probe_changes_are_forwarded_to_the_callback() {
        let probe = FakePowerProbe::default();
        let mut watcher = PowerWatcher::new(Box::new(probe.clone()));
        let seen = Rc::new(RefCell::new(Vec::new()));
        watcher.set_callback({
            let seen = Rc::clone(&seen);
            move |on_battery| seen.borrow_mut().push(on_battery)
        });
        watcher.start();
        probe.simulate(true);
        assert_eq!(*seen.borrow(), vec![false, true]);
        assert!(watcher.on_battery());
        probe.simulate(false);
        assert_eq!(*seen.borrow(), vec![false, true, false]);
    }

    #[test]
    fn refresh_re_reads_a_probe_that_does_not_push() {
        let probe = FakePowerProbe::default();
        let mut watcher = PowerWatcher::new(Box::new(probe.clone()));
        let seen = Rc::new(RefCell::new(Vec::new()));
        watcher.set_callback({
            let seen = Rc::clone(&seen);
            move |on_battery| seen.borrow_mut().push(on_battery)
        });
        watcher.start();
        probe.0.borrow_mut().on_battery = true;
        watcher.refresh();
        assert_eq!(*seen.borrow(), vec![false, true]);
        watcher.refresh();
        assert_eq!(*seen.borrow(), vec![false, true]);
    }

    #[test]
    fn sysfs_probe_detects_a_discharging_battery() {
        let dir = tempdir().unwrap();
        let battery = dir.path().join("BAT0");
        fs::create_dir_all(&battery).unwrap();
        fs::write(battery.join("type"), "Battery\n").unwrap();
        fs::write(battery.join("status"), "Discharging\n").unwrap();
        let probe = SysfsPowerProbe::with_path(dir.path());
        assert!(probe.on_battery());
    }

    #[test]
    fn sysfs_probe_ignores_charging_and_non_batteries() {
        let dir = tempdir().unwrap();
        let battery = dir.path().join("BAT0");
        fs::create_dir_all(&battery).unwrap();
        fs::write(battery.join("type"), "Battery\n").unwrap();
        fs::write(battery.join("status"), "Charging\n").unwrap();
        let adapter = dir.path().join("AC");
        fs::create_dir_all(&adapter).unwrap();
        fs::write(adapter.join("type"), "Mains\n").unwrap();
        let probe = SysfsPowerProbe::with_path(dir.path());
        assert!(!probe.on_battery());
    }

    #[test]
    fn sysfs_probe_without_power_supplies_is_on_mains() {
        let dir = tempdir().unwrap();
        let probe = SysfsPowerProbe::with_path(dir.path());
        assert!(!probe.on_battery());
    }
}
