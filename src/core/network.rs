//! Network status watcher.
//!
//! Fase 3 (Task 3.2): online/offline detection to gate syncs. Mirrors
//! `core/network.py` (Gio `NetworkMonitor` + `network-changed`): the watcher
//! subscribes to availability changes and reports the current state through a
//! callback, which the app wires to [`Scheduler::set_online`] (the scheduler
//! itself turns the restore into a [`Trigger::NetworkRestored`]).
//!
//! The probe is injectable so tests use a fake; production uses
//! [`GioNetworkProbe`].
//!
//! [`Scheduler::set_online`]: crate::core::scheduler::Scheduler::set_online
//! [`Trigger::NetworkRestored`]: crate::core::triggers::Trigger::NetworkRestored

use std::cell::RefCell;
use std::rc::Rc;

use gio::prelude::*;

/// Source of network availability. Implemented by [`GioNetworkProbe`] in
/// production and by fakes in tests.
pub trait NetworkProbe {
    /// Whether the network is currently available.
    fn is_available(&self) -> bool;

    /// Whether the current connection is metered (cellular or a metered
    /// Wi-Fi). Unknown states report `false`.
    fn is_metered(&self) -> bool;

    /// Name of the Wi-Fi network currently connected, when known.
    fn wifi_ssid(&self) -> Option<String>;

    /// Subscribe to availability changes. Returns a token for [`Self::unsubscribe`].
    fn subscribe(&self, callback: Rc<dyn Fn(bool)>) -> u64;

    /// Stop a subscription started with [`Self::subscribe`].
    fn unsubscribe(&self, id: u64);
}

/// Parse `nmcli -t -f IN-USE,SSID dev wifi` output into the active SSID.
/// Lines look like `yes:HomeNet` / `no:Other`; only the `yes` row matters.
pub fn parse_active_ssid(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (used, ssid) = line.split_once(':')?;
        if used.trim() == "yes" && !ssid.trim().is_empty() {
            Some(ssid.trim().to_owned())
        } else {
            None
        }
    })
}

/// Parse `nmcli -t -f GENERAL.METERED dev status` output into whether any
/// device reports a metered connection. Each line is `device:metered` in
/// the common case, but tolerate extra fields (`device:state:metered`).
pub fn parse_metered(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.split(':').any(|field| field.trim() == "yes"))
}

/// Parse the raw `network.allowed_ssids` config value (comma separated).
/// Empty entries are dropped; comparison elsewhere is exact.
pub fn parse_allowed_ssids(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// [`NetworkProbe`] over the GLib `NetworkMonitor` singleton (the same signal
/// `network.py` connects to). Emits `network-changed` on the main loop.
#[derive(Debug, Default)]
pub struct GioNetworkProbe {
    monitor: Option<gio::NetworkMonitor>,
    handler: Rc<RefCell<Option<(gio::NetworkMonitor, glib::SignalHandlerId)>>>,
}

impl GioNetworkProbe {
    /// Resolve the default network monitor, if the platform provides one.
    pub fn new() -> Self {
        Self {
            monitor: Some(gio::NetworkMonitor::default()),
            handler: Rc::new(RefCell::new(None)),
        }
    }
}

impl NetworkProbe for GioNetworkProbe {
    fn is_available(&self) -> bool {
        self.monitor
            .as_ref()
            .map(|monitor| monitor.is_network_available())
            .unwrap_or(true)
    }

    fn is_metered(&self) -> bool {
        self.monitor
            .as_ref()
            .map(|monitor| monitor.is_network_metered())
            .unwrap_or(false)
    }

    fn wifi_ssid(&self) -> Option<String> {
        // GLib exposes availability and metering but not the network name;
        // NetworkManager is the source of truth on Linux desktops.
        let output = std::process::Command::new("nmcli")
            .args(["-t", "-f", "IN-USE,SSID", "dev", "wifi"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        parse_active_ssid(&text)
    }

    fn subscribe(&self, callback: Rc<dyn Fn(bool)>) -> u64 {
        let Some(monitor) = &self.monitor else {
            return 0;
        };
        let mut handler = self.handler.borrow_mut();
        if handler.is_some() {
            return 1;
        }
        let id = monitor.connect_network_changed(move |_monitor, available| {
            callback(available);
        });
        *handler = Some((monitor.clone(), id));
        1
    }

    fn unsubscribe(&self, _id: u64) {
        if let Some((monitor, id)) = self.handler.borrow_mut().take() {
            monitor.disconnect(id);
        }
    }
}

/// Watches network availability and forwards every change to a callback.
#[derive(Clone)]
pub struct NetworkWatcher {
    inner: Rc<RefCell<NetworkWatcherInner>>,
}

struct NetworkWatcherInner {
    probe: Box<dyn NetworkProbe>,
    on_change: Option<Rc<dyn Fn(bool)>>,
    online: bool,
    subscription: Option<u64>,
}

impl NetworkWatcher {
    /// Create a watcher around the given probe.
    pub fn new(probe: Box<dyn NetworkProbe>) -> Self {
        let online = probe.is_available();
        Self {
            inner: Rc::new(RefCell::new(NetworkWatcherInner {
                probe,
                on_change: None,
                online,
                subscription: None,
            })),
        }
    }

    /// The Gio probe for the default network monitor.
    pub fn gio() -> Self {
        Self::new(Box::new(GioNetworkProbe::new()))
    }

    /// Set the callback that receives availability changes. The current state
    /// is reported once on [`start`](Self::start).
    pub fn set_callback(&mut self, callback: impl Fn(bool) + 'static) {
        self.inner.borrow_mut().on_change = Some(Rc::new(callback));
    }

    /// Whether the network is currently available.
    pub fn is_online(&self) -> bool {
        self.inner.borrow().online
    }

    /// Subscribe to the probe and report the current state immediately, like
    /// `network.py`'s `start`.
    pub fn start(&mut self) {
        let weak = Rc::downgrade(&self.inner);
        let id = {
            let inner = self.inner.borrow();
            inner.probe.subscribe(Rc::new(move |available| {
                if let Some(inner) = weak.upgrade() {
                    let mut inner = inner.borrow_mut();
                    if inner.online != available {
                        inner.online = available;
                        if let Some(callback) = &inner.on_change {
                            callback(available);
                        }
                    }
                }
            }))
        };
        let mut inner = self.inner.borrow_mut();
        inner.subscription = if id == 0 { None } else { Some(id) };
        let online = inner.online;
        if let Some(callback) = &inner.on_change {
            callback(online);
        }
    }

    /// Unsubscribe and stop reporting changes.
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
    use std::cell::RefCell;

    #[derive(Clone, Default)]
    struct FakeNetworkProbe(Rc<RefCell<FakeNetworkProbeInner>>);

    #[derive(Default)]
    struct FakeNetworkProbeInner {
        available: bool,
        callback: Option<Rc<dyn Fn(bool)>>,
        subscribed: bool,
    }

    impl FakeNetworkProbe {
        fn simulate(&self, available: bool) {
            let callback = {
                let inner = self.0.borrow();
                inner.callback.clone()
            };
            if let Some(callback) = callback {
                callback(available);
            }
        }
    }

    impl NetworkProbe for FakeNetworkProbe {
        fn is_available(&self) -> bool {
            self.0.borrow().available
        }

        fn is_metered(&self) -> bool {
            false
        }

        fn wifi_ssid(&self) -> Option<String> {
            None
        }

        fn subscribe(&self, callback: Rc<dyn Fn(bool)>) -> u64 {
            let mut inner = self.0.borrow_mut();
            inner.callback = Some(callback);
            inner.subscribed = true;
            1
        }

        fn unsubscribe(&self, _id: u64) {
            let mut inner = self.0.borrow_mut();
            inner.callback = None;
            inner.subscribed = false;
        }
    }

    #[test]
    fn start_reports_the_current_state_immediately() {
        let probe = FakeNetworkProbe::default();
        probe.0.borrow_mut().available = true;
        let mut watcher = NetworkWatcher::new(Box::new(probe.clone()));
        let seen = Rc::new(RefCell::new(Vec::new()));
        watcher.set_callback({
            let seen = Rc::clone(&seen);
            move |online| seen.borrow_mut().push(online)
        });
        watcher.start();
        assert_eq!(*seen.borrow(), vec![true]);
        assert!(watcher.is_online());
        assert!(probe.0.borrow().subscribed);
    }

    #[test]
    fn probe_changes_are_forwarded_to_the_callback() {
        let probe = FakeNetworkProbe::default();
        probe.0.borrow_mut().available = true;
        let mut watcher = NetworkWatcher::new(Box::new(probe.clone()));
        let seen = Rc::new(RefCell::new(Vec::new()));
        watcher.set_callback({
            let seen = Rc::clone(&seen);
            move |online| seen.borrow_mut().push(online)
        });
        watcher.start();
        probe.simulate(false);
        assert_eq!(*seen.borrow(), vec![true, false]);
        assert!(!watcher.is_online());
        probe.simulate(true);
        assert_eq!(*seen.borrow(), vec![true, false, true]);
    }

    #[test]
    fn repeated_same_value_does_not_reemit() {
        let probe = FakeNetworkProbe::default();
        probe.0.borrow_mut().available = true;
        let mut watcher = NetworkWatcher::new(Box::new(probe.clone()));
        let seen = Rc::new(RefCell::new(Vec::new()));
        watcher.set_callback({
            let seen = Rc::clone(&seen);
            move |online| seen.borrow_mut().push(online)
        });
        watcher.start();
        probe.simulate(false);
        probe.simulate(false);
        assert_eq!(*seen.borrow(), vec![true, false]);
    }

    #[test]
    fn stop_unsubscribes() {
        let probe = FakeNetworkProbe::default();
        let mut watcher = NetworkWatcher::new(Box::new(probe.clone()));
        watcher.set_callback(|_| {});
        watcher.start();
        assert!(probe.0.borrow().subscribed);
        watcher.stop();
        assert!(!probe.0.borrow().subscribed);
    }

    #[test]
    fn initial_state_comes_from_the_probe() {
        let probe = FakeNetworkProbe::default();
        probe.0.borrow_mut().available = false;
        let watcher = NetworkWatcher::new(Box::new(probe));
        assert!(!watcher.is_online());
    }

    #[test]
    fn parse_active_ssid_finds_the_in_use_row() {
        assert_eq!(
            parse_active_ssid("no:CoffeeShop\nyes:HomeNet\nno:Other"),
            Some("HomeNet".to_owned())
        );
        assert_eq!(parse_active_ssid("no:CoffeeShop\nno:Other"), None);
        assert_eq!(parse_active_ssid("yes:"), None);
        assert_eq!(parse_active_ssid(""), None);
    }

    #[test]
    fn parse_metered_only_triggers_on_yes() {
        assert!(parse_metered("wlan0:connected:yes\neth0:connected:no"));
        assert!(!parse_metered("wlan0:connected:no"));
        assert!(!parse_metered("wlan0:connected:unknown"));
        assert!(!parse_metered(""));
    }

    #[test]
    fn parse_allowed_ssids_drops_empty_entries() {
        assert_eq!(
            parse_allowed_ssids(" Home , Work ,, Gym "),
            vec!["Home".to_owned(), "Work".to_owned(), "Gym".to_owned()]
        );
        assert!(parse_allowed_ssids("").is_empty());
        assert!(parse_allowed_ssids(" , ").is_empty());
    }
}
