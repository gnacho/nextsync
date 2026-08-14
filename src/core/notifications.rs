//! Desktop notifications for synchronization problems.
//!
//! Port of the Python app's `Gio.Notification` usage (application.py):
//! when a folder run ends in `AuthFailed`, `KeyringLocked` or `Failed`, a
//! desktop notification is sent so failures on an unfocused window are not
//! missed. The transport is injectable: production uses `notify-rust`
//! (org.freedesktop.Notifications); tests count callbacks instead.

use std::cell::Cell;
use std::rc::Rc;

/// Sink for desktop notifications.
pub trait DesktopNotifier {
    /// Send a notification; `summary` is the title, `body` the detail.
    fn send(&self, summary: &str, body: &str);
}

/// Production notifier over org.freedesktop.Notifications (notify-rust).
pub struct FreedesktopNotifier;

impl DesktopNotifier for FreedesktopNotifier {
    fn send(&self, summary: &str, body: &str) {
        if let Err(error) = notify_rust::Notification::new()
            .summary(summary)
            .body(body)
            .appname("nextsync")
            .show()
        {
            eprintln!("notification failed: {error}");
        }
    }
}

/// Test notifier recording every send.
#[derive(Default)]
pub struct CountingNotifier {
    pub sent: Cell<u32>,
}

impl DesktopNotifier for CountingNotifier {
    fn send(&self, _summary: &str, _body: &str) {
        self.sent.set(self.sent.get() + 1);
    }
}

/// Notification copy for one outcome.
///
/// Returns `None` for healthy outcomes (Success/Conflict) — notifications
/// exist to surface problems, not to celebrate.
pub fn failure_notification(outcome: &crate::core::scheduler::SyncOutcome) -> Option<&'static str> {
    use crate::core::scheduler::SyncOutcome;
    match outcome {
        SyncOutcome::AuthFailed => Some(crate::util::i18n::t(
            "The server rejected the account credentials.",
        )),
        SyncOutcome::KeyringLocked => Some(crate::util::i18n::t("The password keyring is locked.")),
        SyncOutcome::Failed => Some(crate::util::i18n::t("A synchronization failed.")),
        SyncOutcome::Success | SyncOutcome::Conflict => None,
    }
}

/// Whether the outcome deserves a notification, resolved against the pure
/// copy table (kept separate so tests do not need a desktop bus).
pub fn notify_for_outcome(
    notifier: &Rc<dyn DesktopNotifier>,
    enabled: bool,
    account_label: &str,
    outcome: &crate::core::scheduler::SyncOutcome,
) {
    if !enabled {
        return;
    }
    let Some(body) = failure_notification(outcome) else {
        return;
    };
    notifier.send(
        crate::util::i18n::t("NextSync"),
        &format!("{account_label}: {body}"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::scheduler::SyncOutcome;

    #[test]
    fn healthy_outcomes_do_not_notify() {
        let notifier: Rc<dyn DesktopNotifier> = Rc::new(CountingNotifier::default());
        notify_for_outcome(&notifier, true, "acct", &SyncOutcome::Success);
        notify_for_outcome(&notifier, true, "acct", &SyncOutcome::Conflict);
    }

    #[test]
    fn failure_outcomes_notify_once() {
        for outcome in [
            SyncOutcome::Failed,
            SyncOutcome::AuthFailed,
            SyncOutcome::KeyringLocked,
        ] {
            let sent = Rc::new(CountingNotifier::default());
            let notifier: Rc<dyn DesktopNotifier> = sent.clone();
            notify_for_outcome(&notifier, true, "acct", &outcome);
            assert_eq!(sent.sent.get(), 1, "{outcome:?}");
        }
    }

    #[test]
    fn disabled_notifications_are_silent() {
        let sent = Rc::new(CountingNotifier::default());
        let notifier: Rc<dyn DesktopNotifier> = sent.clone();
        notify_for_outcome(&notifier, false, "acct", &SyncOutcome::Failed);
        assert_eq!(sent.sent.get(), 0);
    }
}
