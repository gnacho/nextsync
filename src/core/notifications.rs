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

    /// Raise a critical desktop notification for a pending deletion review
    /// (issue #203). The notification explains synchronization was paused to
    /// protect the missing files and carries a "Review Now" action. `on_action`
    /// is fired on a worker thread with the action name (`"default"` for a body
    /// click, or `"__closed"` when the notification is dismissed).
    fn send_delete_review(
        &self,
        summary: &str,
        body: &str,
        on_action: Box<dyn Fn(&str) + Send + 'static>,
    );
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

    fn send_delete_review(
        &self,
        summary: &str,
        body: &str,
        on_action: Box<dyn Fn(&str) + Send + 'static>,
    ) {
        let action_label = crate::util::i18n::t("Review Now").to_string();
        let mut notification = notify_rust::Notification::new();
        notification
            .summary(summary)
            .body(body)
            .appname("nextsync")
            .urgency(notify_rust::Urgency::Critical)
            .action("default", &action_label);
        match notification.show() {
            Ok(handle) => {
                // `wait_for_action` blocks a worker thread until the user acts
                // on or dismisses the notification. Callers marshal back to the
                // GLib main loop before touching UI.
                std::thread::spawn(move || {
                    handle.wait_for_action(|action| on_action(action));
                });
            }
            Err(error) => eprintln!("notification failed: {error}"),
        }
    }
}

/// Test notifier recording every send.
#[derive(Default)]
pub struct CountingNotifier {
    pub sent: Cell<u32>,
    last_summary: Cell<Option<String>>,
    last_body: Cell<Option<String>>,
}

impl CountingNotifier {
    /// Summary of the most recent notification, if any.
    pub fn last_summary(&self) -> Option<String> {
        self.last_summary.take()
    }

    /// Body of the most recent notification, if any.
    pub fn last_body(&self) -> Option<String> {
        self.last_body.take()
    }
}

impl DesktopNotifier for CountingNotifier {
    fn send(&self, _summary: &str, _body: &str) {
        self.sent.set(self.sent.get() + 1);
    }

    fn send_delete_review(
        &self,
        summary: &str,
        body: &str,
        _on_action: Box<dyn Fn(&str) + Send + 'static>,
    ) {
        self.sent.set(self.sent.get() + 1);
        self.last_summary.set(Some(summary.to_string()));
        self.last_body.set(Some(body.to_string()));
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
        SyncOutcome::NoCredentials => Some(crate::util::i18n::t(
            "No credentials are saved for this account.",
        )),
        SyncOutcome::Failed => Some(crate::util::i18n::t("A synchronization failed.")),
        // A transport failure is a transient network condition (the server is
        // unreachable), not a problem the account needs a notification for:
        // it resolves on the next automatic trigger once the server answers.
        // Silence it like the healthy outcomes (issue #162).
        SyncOutcome::Success | SyncOutcome::Conflict | SyncOutcome::NetworkError => None,
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

    #[test]
    fn delete_review_notification_records_the_copy() {
        let sent = Rc::new(CountingNotifier::default());
        let notifier: Rc<dyn DesktopNotifier> = sent.clone();
        notifier.send_delete_review(
            "Review Deletions",
            "Synchronization was paused before 12 files could be deleted.",
            Box::new(|_| {}),
        );
        assert_eq!(sent.sent.get(), 1);
        assert_eq!(sent.last_summary(), Some("Review Deletions".to_string()),);
        assert_eq!(
            sent.last_body(),
            Some("Synchronization was paused before 12 files could be deleted.".to_string()),
        );
    }
}
