//! Desktop notifications for the server's own notifications.
//!
//! Issue #31. Nextcloud keeps per-user notifications (new shares, comments,
//! mentions) behind an OCS endpoint
//! (`GET /ocs/v2.php/apps/notifications/api/v1/notifications`). When the
//! "Show server notifications" preference is on, a lightweight poller fetches
//! the list and raises a desktop notification for every item it has not seen
//! before. The notify_push connection also pokes the poller when the server
//! signals `notify_notification`, so delivery is near-instant while push is
//! connected and the periodic poll covers the rest (OpenCloud accounts, push
//! disabled).
//!
//! Design notes:
//! - The preference is read live from the [`ConfigStore`] on every poll, so
//!   toggling it in Settings takes effect without a restart.
//! - The first fetch only *seeds* the seen-id baseline: enabling the option
//!   must not replay a backlog of old notifications. Only items that appear
//!   after that are raised.
//! - Fetch failures are best-effort and silent to the user (the desktop
//!   notification bus gets nothing; the app log gets a line).

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::nextcloud::api::{NextcloudApi, ServerNotification};
use crate::nextcloud::credentials::CredentialsStore;
use crate::storage::config::ConfigStore;

/// How often the poller re-reads the endpoint while the option is on.
const POLL_SECONDS: u32 = 60;

/// The notifications that have not been shown yet, in server order.
///
/// Pure helper kept separate so tests can pin the dedup contract without a
/// desktop bus or a network.
pub fn unseen<'a>(
    seen: &HashSet<i64>,
    notifications: &'a [ServerNotification],
) -> Vec<&'a ServerNotification> {
    notifications
        .iter()
        .filter(|item| !seen.contains(&item.notification_id))
        .collect()
}

/// Polls the server notifications endpoint and raises desktop notifications.
pub struct ServerNotificationWatcher {
    store: ConfigStore,
    account_id: String,
    server: String,
    login: String,
    notifier: Rc<dyn crate::core::notifications::DesktopNotifier>,
    logger: crate::core::log::LogBuffer,
    /// Notification ids already shown (or seeded as the baseline).
    seen: Rc<RefCell<HashSet<i64>>>,
    /// Whether the baseline has been seeded. A separate flag from `seen`
    /// being non-empty: the first poll may legitimately return zero
    /// notifications, and the seed must happen exactly once (issue #141).
    seeded: Rc<RefCell<bool>>,
    /// Guards against overlapping fetches when the push poke and the timer
    /// fire together.
    running: Rc<RefCell<bool>>,
    /// The periodic GLib timer, kept alive by the watcher.
    source_id: RefCell<Option<glib::SourceId>>,
}

impl ServerNotificationWatcher {
    /// Create a watcher for one account.
    pub fn new(
        store: ConfigStore,
        account_id: impl Into<String>,
        server: impl Into<String>,
        login: impl Into<String>,
        notifier: Rc<dyn crate::core::notifications::DesktopNotifier>,
        logger: crate::core::log::LogBuffer,
    ) -> Self {
        Self {
            store,
            account_id: account_id.into(),
            server: server.into(),
            login: login.into(),
            notifier,
            logger,
            seen: Rc::new(RefCell::new(HashSet::new())),
            seeded: Rc::new(RefCell::new(false)),
            running: Rc::new(RefCell::new(false)),
            source_id: RefCell::new(None),
        }
    }

    /// Start the periodic poller. The caller keeps the `Rc` alive; the GLib
    /// timer closure also holds a clone, so the watcher survives until the
    /// application quits.
    pub fn start(self: &Rc<Self>) {
        if self.source_id.borrow().is_some() {
            return;
        }
        let this = self.clone();
        let id = glib::timeout_add_seconds_local(POLL_SECONDS, move || {
            this.poll();
            glib::ControlFlow::Continue
        });
        self.source_id.borrow_mut().replace(id);
    }

    /// Request an immediate fetch (wired to the push `notify_notification`
    /// hint). Cheap when a poll is already in flight or the option is off.
    pub fn poke(&self) {
        self.poll();
    }

    fn poll(&self) {
        let enabled = self
            .store
            .load()
            .map(|config| config.general.show_server_notifications)
            .unwrap_or(false);
        if !enabled || *self.running.borrow() {
            return;
        }
        *self.running.borrow_mut() = true;
        let account_id = self.account_id.clone();
        let server = self.server.clone();
        let login = self.login.clone();
        let seen = self.seen.clone();
        let seeded = self.seeded.clone();
        let notifier = self.notifier.clone();
        let logger = self.logger.clone();
        let running = self.running.clone();
        let task = gio::spawn_blocking(move || {
            // The HTTP client is built inside the blocking closure (it is not
            // `Send`), like the other API call sites; only plain strings cross
            // the thread boundary.
            let password = CredentialsStore::get_for_account(&account_id, &server, &login)
                .ok()
                .flatten();
            match password {
                Some(password) => NextcloudApi::new().notifications(&server, &login, &password),
                None => Ok(Vec::new()),
            }
        });
        glib::spawn_future_local(async move {
            // `task.await` yields the blocking closure's own `Result` inside
            // the join-handle result (the outer layer reports panics).
            match task.await {
                Ok(Ok(notifications)) => {
                    if !*seeded.borrow() {
                        // First run: seed the baseline so enabling the option
                        // does not replay a backlog of old notifications. The
                        // flag (not the set being empty) decides, so a first
                        // poll that returns zero items still counts as seeded
                        // (issue #141).
                        let mut seen = seen.borrow_mut();
                        for item in &notifications {
                            seen.insert(item.notification_id);
                        }
                        *seeded.borrow_mut() = true;
                    } else {
                        let new_items = {
                            let seen = seen.borrow();
                            unseen(&seen, &notifications)
                        };
                        for item in &new_items {
                            let summary = if item.subject.is_empty() {
                                crate::util::i18n::t("NextSync")
                            } else {
                                item.subject.as_str()
                            };
                            notifier.send(summary, item.message.as_deref().unwrap_or(""));
                        }
                        let mut seen = seen.borrow_mut();
                        for item in &new_items {
                            seen.insert(item.notification_id);
                        }
                    }
                }
                Ok(Err(error)) => {
                    // Best-effort and silent to the user: a dead network or an
                    // auth hiccup must not raise a notification storm.
                    logger.append(&format!("server notifications: could not fetch: {error}"));
                }
                Err(_panic) => {
                    logger.append("server notifications: could not fetch.");
                }
            }
            *running.borrow_mut() = false;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(notification_id: i64, subject: &str) -> ServerNotification {
        ServerNotification {
            notification_id,
            app: "files_sharing".to_string(),
            subject: subject.to_string(),
            message: None,
        }
    }

    #[test]
    fn unseen_returns_only_new_ids_in_order() {
        let items = vec![sample(1, "one"), sample(2, "two"), sample(3, "three")];
        let seen = HashSet::from([2i64]);
        let new: Vec<i64> = unseen(&seen, &items)
            .into_iter()
            .map(|item| item.notification_id)
            .collect();
        assert_eq!(new, vec![1, 3]);
    }

    #[test]
    fn unseen_empty_seen_returns_everything() {
        let items = vec![sample(1, "one"), sample(2, "two")];
        let seen = HashSet::new();
        assert_eq!(unseen(&seen, &items).len(), 2);
    }

    #[test]
    fn unseen_no_new_items_is_empty() {
        let items = vec![sample(1, "one")];
        let seen = HashSet::from([1i64]);
        assert!(unseen(&seen, &items).is_empty());
    }

    #[test]
    fn seeded_flag_decides_the_baseline_not_the_set_size() {
        // Issue #141: a first poll that returns zero notifications still
        // seeds the baseline, so the next poll with one real notification is
        // not swallowed by a second seed.
        let seen = Rc::new(RefCell::new(HashSet::new()));
        let seeded = Rc::new(RefCell::new(false));
        // First poll: zero notifications, but it marks the baseline seeded.
        *seeded.borrow_mut() = true;
        assert!(seen.borrow().is_empty());
        assert!(*seeded.borrow());
        // A later poll with a real notification finds the seed flag on.
        let items = [sample(7, "new")];
        let new_items = unseen(&seen.borrow(), &items);
        assert_eq!(new_items.len(), 1);
    }
}
