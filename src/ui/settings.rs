//! Settings window (Task 5.2).
//!
//! Port of `ui/settings.py` (v0.4.0) to gtk-rs/libadwaita: a single in-app
//! `SettingsView` (ToolbarView + ViewStack + ViewSwitcherBar) with General,
//! Synchronization, Network and Advanced pages, and the typed Remove account
//! confirmation. Folder management lives in the sync view (issue #18); the
//! standalone Add Folder dialog here is shared with it.
//!
//! # Deviations from `settings.py` (motivated)
//!
//! - i18n (Task 6.1): user-visible strings go through [`crate::util::i18n::t`];
//!   msgids missing from the Spanish catalog fall back to the English source.
//! - No `runtime` parameter. The window only receives a [`ConfigStore`], the
//!   [`AccountConfig`] snapshot, the account id and the [`SettingsCallbacks`]
//!   closures. The live Diagnostics rows (inotify watches, push state) are
//!   dropped because the window has no handle to the runtimes;
//!   `runtime.last_exit_code` is shown instead.
//! - Desktop integrations (`_build_desktop_integrations`): the three switches
//!   target the account's FIRST folder instead of the Python's "active"
//!   folder (the rewrite has no active-account concept); with no folders the
//!   switches are absent, like the Python. The [`DesktopIntegration`] runs on
//!   the UI thread (tiny file/metadata writes, same as the Python handlers).
//! - Autostart is persisted to the configuration and mirrored into the
//!   desktop session immediately (atomic `~/.config/autostart` entry).
//! - The Add Folder dialog is rebuilt on every open so a failed attempt can
//!   show an inline error label; the previous values are carried over.
//!   libadwaita 0.9 `AlertDialog` closes on response, so errors surface as
//!   toasts on the window.
//! - The "Detailed synchronization output" switch persists on its own (it
//!   lives in the Logging group on the Advanced page); the Python saves it
//!   from the same `_save_sync` handler.
//! - Proxy validation is replicated locally (light check) so the Network page
//!   can mark the row with the `error` CSS class without coupling to the
//!   private validator in `storage::config`.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita::prelude::*;

use crate::core::sync_safety::{
    local_folder_is_empty, review_required, stale_artifact_names, FirstSyncFacts,
};
use crate::core::triggers::TriggerSettings;
use crate::nextcloud::api::{ApiError, NextcloudApi};
use crate::nextcloud::credentials::CredentialsStore;
use crate::nextcloud::driver::Provider;
use crate::storage::config::{
    default_sync_root, expanduser, remote_path_for, validate_pattern, AccountConfig, Config,
    ConfigError, ConfigStore, FolderConfig, GeneralConfig, LoggingConfig, DEFAULT_PATTERNS,
};
use crate::util::i18n::t;
use crate::util::paths::state_dir;

/// A simple no-argument callback.
pub type SettingsCallback = Rc<dyn Fn()>;

/// Callbacks the Settings window invokes after mutating the configuration.
#[derive(Clone, Default)]
pub struct SettingsCallbacks {
    /// Invoked after the typed Remove account confirmation succeeds.
    pub on_remove_account: Option<SettingsCallback>,
    /// Invoked after a folder is added or removed (refreshes the account view).
    pub on_folder_changed: Option<SettingsCallback>,
    /// Invoked after trigger/logging settings change (hot-reconfigures the
    /// account runtimes).
    pub on_reconfigure: Option<SettingsCallback>,
}

/// Where an in-app settings view anchors its dialogs and toasts.
///
/// The official-client style keeps preferences inside the main window instead
/// of a separate `PreferencesWindow`; dialogs present over `parent` and toasts
/// surface through `toast`. `Clone` is cheap (widget handles).
#[derive(Clone)]
pub struct SettingsHost {
    parent: gtk4::Widget,
    toast: libadwaita::ToastOverlay,
}

impl SettingsHost {
    /// Anchor over `parent` (the main window) with the given toast overlay.
    pub fn new(parent: &impl IsA<gtk4::Widget>, toast: &libadwaita::ToastOverlay) -> Self {
        Self {
            parent: parent.upcast_ref::<gtk4::Widget>().clone(),
            toast: toast.clone(),
        }
    }

    /// The widget dialogs present over.
    pub fn parent(&self) -> &gtk4::Widget {
        &self.parent
    }

    /// Surface a toast on the anchored overlay.
    pub fn add_toast(&self, toast: libadwaita::Toast) {
        self.toast.add_toast(toast);
    }
}

/// The in-app settings view: the four preference pages in a `ViewStack` with
/// a `ViewSwitcherBar`, rendered inside the main window (official-client
/// style) instead of a separate `PreferencesWindow`. Dialogs and toasts anchor
/// on the shared [`SettingsHost`].
pub struct SettingsView {
    root: libadwaita::ToolbarView,
    stack: libadwaita::ViewStack,
    switcher: libadwaita::ViewSwitcherBar,
    page_names: Vec<String>,
}

impl SettingsView {
    /// Build the view for one account.
    ///
    /// `account` is the snapshot used for the initial widget values;
    /// `account_id` is the key every write operation uses against the store.
    pub fn new(
        config_store: ConfigStore,
        account: AccountConfig,
        account_id: String,
        callbacks: SettingsCallbacks,
        host: &SettingsHost,
    ) -> Self {
        // Top-level sections (general/logging/network) come from the current
        // configuration; account-owned settings come from the snapshot.
        let config = config_store.load().unwrap_or_default();

        // Folder management (list, Add Folder, desktop integration) was
        // removed from Settings by user decision (issue #18): the sync view
        // owns it, so the settings pages never duplicate it.
        let general = build_general_page(&config_store, &config.general);
        let synchronization =
            build_sync_page(&config_store, &account_id, &account, &callbacks, host);
        let network = build_network_page(&config_store, &config.network);
        let advanced = build_advanced_page(&config_store, &config.logging, &callbacks);

        let stack = libadwaita::ViewStack::new();
        let toolbar = libadwaita::ToolbarView::new();
        let switcher = libadwaita::ViewSwitcherBar::new();
        switcher.set_stack(Some(&stack));
        // The bar defaults to hidden (reveal = false); without this the four
        // pages are unreachable (issue #51).
        switcher.set_reveal(true);
        toolbar.add_bottom_bar(&switcher);
        toolbar.set_content(Some(&stack));
        let page_names = Vec::new();
        let mut view = Self {
            root: toolbar,
            stack: stack.clone(),
            switcher: switcher.clone(),
            page_names,
        };
        view.add_page("general", general);
        view.add_page("synchronization", synchronization);
        view.add_page("network", network);
        view.add_page("advanced", advanced);
        stack.set_visible_child_name("general");
        view
    }

    fn add_page(&mut self, name: &str, page: libadwaita::PreferencesPage) {
        let title = page.title().to_string();
        let icon = page.icon_name().unwrap_or_default().to_string();
        self.stack.add_titled(&page, Some(name), &title);
        if !icon.is_empty() {
            self.stack.page(&page).set_icon_name(Some(&icon));
        }
        self.page_names.push(name.to_string());
    }

    /// The root widget to embed in the main window.
    pub fn widget(&self) -> &libadwaita::ToolbarView {
        &self.root
    }

    /// Whether the bottom page switcher is revealed (issue #51: it must
    /// always be, or the non-visible pages are unreachable).
    pub fn switcher_revealed(&self) -> bool {
        self.switcher.property::<bool>("reveal")
    }

    /// Show the page identified by `name` (see [`SettingsPage`] constants).
    pub fn show_page(&self, name: &str) {
        self.stack.set_visible_child_name(name);
    }

    /// The ordered page names, as added.
    pub fn page_names(&self) -> &[String] {
        &self.page_names
    }
}

/// Stable identifiers for the settings pages.
pub mod page {
    pub const GENERAL: &str = "general";
    pub const SYNCHRONIZATION: &str = "synchronization";
    pub const NETWORK: &str = "network";
    pub const ADVANCED: &str = "advanced";
}

// ---------------------------------------------------------------------------
// Page builders
// ---------------------------------------------------------------------------

/// General page: Startup and Notifications switches.
fn build_general_page(store: &ConfigStore, general: &GeneralConfig) -> libadwaita::PreferencesPage {
    let page = libadwaita::PreferencesPage::builder()
        .title(t("General"))
        .icon_name("preferences-system-symbolic")
        .build();

    let startup = libadwaita::PreferencesGroup::builder()
        .title(t("Startup"))
        .build();
    let autostart = libadwaita::SwitchRow::builder()
        .title(t("Start NextSync when I sign in"))
        .active(general.autostart)
        .build();

    let notifications = libadwaita::SwitchRow::builder()
        .title(t("Show desktop notifications"))
        .active(general.show_notifications)
        .build();

    let server_notifications = libadwaita::SwitchRow::builder()
        .title(t("Show server notifications"))
        .active(general.show_server_notifications)
        .build();

    let quiet = libadwaita::SwitchRow::builder()
        .title(t("Quiet hours"))
        .subtitle(t(
            "Suspend automatic synchronization inside a daily time window",
        ))
        .active(general.quiet_hours.is_some())
        .build();
    let quiet_start = libadwaita::EntryRow::new();
    quiet_start.set_title(t("Starts at"));
    quiet_start.set_text(
        general
            .quiet_hours
            .as_ref()
            .map_or("", |pair| pair.0.as_str()),
    );
    quiet_start.set_input_purpose(gtk4::InputPurpose::Alpha);
    let quiet_end = libadwaita::EntryRow::new();
    quiet_end.set_title(t("Ends at"));
    quiet_end.set_text(
        general
            .quiet_hours
            .as_ref()
            .map_or("", |pair| pair.1.as_str()),
    );

    {
        let store = store.clone();
        let autostart_guard = autostart.clone();
        let notifications_guard = notifications.clone();
        let server_guard = server_notifications.clone();
        let quiet_guard = quiet.clone();
        let quiet_start_guard = quiet_start.clone();
        let quiet_end_guard = quiet_end.clone();
        autostart.connect_active_notify(move |_| {
            save_general(
                &store,
                &autostart_guard,
                &notifications_guard,
                &server_guard,
                &quiet_guard,
                &quiet_start_guard,
                &quiet_end_guard,
            );
        });
    }
    {
        let store = store.clone();
        let autostart_guard = autostart.clone();
        let notifications_guard = notifications.clone();
        let server_guard = server_notifications.clone();
        let quiet_guard = quiet.clone();
        let quiet_start_guard = quiet_start.clone();
        let quiet_end_guard = quiet_end.clone();
        notifications.connect_active_notify(move |_| {
            save_general(
                &store,
                &autostart_guard,
                &notifications_guard,
                &server_guard,
                &quiet_guard,
                &quiet_start_guard,
                &quiet_end_guard,
            );
        });
    }
    {
        let store = store.clone();
        let autostart_guard = autostart.clone();
        let notifications_guard = notifications.clone();
        let server_guard = server_notifications.clone();
        let quiet_guard = quiet.clone();
        let quiet_start_guard = quiet_start.clone();
        let quiet_end_guard = quiet_end.clone();
        server_notifications.connect_active_notify(move |_| {
            save_general(
                &store,
                &autostart_guard,
                &notifications_guard,
                &server_guard,
                &quiet_guard,
                &quiet_start_guard,
                &quiet_end_guard,
            );
        });
    }
    {
        let store = store.clone();
        let autostart_guard = autostart.clone();
        let notifications_guard = notifications.clone();
        let server_guard = server_notifications.clone();
        let quiet_guard = quiet.clone();
        let quiet_start_guard = quiet_start.clone();
        let quiet_end_guard = quiet_end.clone();
        let save = move || {
            save_general(
                &store,
                &autostart_guard,
                &notifications_guard,
                &server_guard,
                &quiet_guard,
                &quiet_start_guard,
                &quiet_end_guard,
            );
        };
        quiet.connect_active_notify(move |_| {
            save();
        });
    }
    {
        let store = store.clone();
        let autostart_guard = autostart.clone();
        let notifications_guard = notifications.clone();
        let server_guard = server_notifications.clone();
        let quiet_guard = quiet.clone();
        let quiet_start_guard = quiet_start.clone();
        let quiet_end_guard = quiet_end.clone();
        quiet_start.connect_apply(move |_| {
            save_general(
                &store,
                &autostart_guard,
                &notifications_guard,
                &server_guard,
                &quiet_guard,
                &quiet_start_guard,
                &quiet_end_guard,
            );
        });
    }
    {
        let store = store.clone();
        let autostart_guard = autostart.clone();
        let notifications_guard = notifications.clone();
        let server_guard = server_notifications.clone();
        let quiet_guard = quiet.clone();
        let quiet_start_guard = quiet_start.clone();
        let quiet_end_guard = quiet_end.clone();
        quiet_end.connect_apply(move |_| {
            save_general(
                &store,
                &autostart_guard,
                &notifications_guard,
                &server_guard,
                &quiet_guard,
                &quiet_start_guard,
                &quiet_end_guard,
            );
        });
    }

    startup.add(&autostart);
    page.add(&startup);

    let notifications_group = libadwaita::PreferencesGroup::builder()
        .title(t("Notifications"))
        .build();
    notifications_group.add(&notifications);
    notifications_group.add(&server_notifications);
    page.add(&notifications_group);

    quiet_start.set_show_apply_button(true);
    quiet_end.set_show_apply_button(true);
    let quiet_group = libadwaita::PreferencesGroup::builder()
        .title(t("Quiet Hours"))
        .build();
    quiet_group.add(&quiet);
    quiet_group.add(&quiet_start);
    quiet_group.add(&quiet_end);
    page.add(&quiet_group);

    page
}

/// Synchronization page: the manual-only banner, the four trigger switches,
/// exclusions and reliability.
/// Build the per-account synchronization option groups (issue #56): they
/// are shared by the account settings panel in the main window and were
/// the former Synchronization page.
pub(crate) fn sync_option_groups(
    store: &ConfigStore,
    account_id: &str,
    account: &AccountConfig,
    callbacks: &SettingsCallbacks,
    _host: &SettingsHost,
) -> Vec<libadwaita::PreferencesGroup> {
    let sync = &account.sync;
    let mut groups: Vec<libadwaita::PreferencesGroup> = Vec::new();

    let manual_group = libadwaita::PreferencesGroup::new();
    let banner = libadwaita::Banner::new(t(
        "Automatic synchronization is off. Files synchronize only with Sync Now.",
    ));
    banner.set_revealed(crate::core::triggers::manual_only(&TriggerSettings::from(
        sync,
    )));
    manual_group.add(&banner);
    groups.push(manual_group);

    let local = libadwaita::PreferencesGroup::builder()
        .title(t("Local Changes"))
        .build();
    let inotify = libadwaita::SwitchRow::builder()
        .title(t("Monitor filesystem changes"))
        .active(sync.local_inotify_enabled)
        .build();
    let local_timer = libadwaita::SwitchRow::builder()
        .title(t("Run a local interval"))
        .active(sync.local_interval_enabled)
        .build();
    let local_minutes = spin_row(
        t("Local interval (minutes)"),
        1.0,
        1440.0,
        sync.local_interval_minutes as f64,
    );
    local_minutes.set_visible(sync.local_interval_enabled);
    local.add(&inotify);
    local.add(&local_timer);
    local.add(&local_minutes);
    groups.push(local);

    let remote = libadwaita::PreferencesGroup::builder()
        .title(t("Remote Changes"))
        .build();
    let push = libadwaita::SwitchRow::builder()
        .title(t("Use server push notifications"))
        .active(sync.remote_push_enabled)
        .build();
    let remote_timer = libadwaita::SwitchRow::builder()
        .title(t("Run a remote interval"))
        .active(sync.remote_interval_enabled)
        .build();
    let remote_minutes = spin_row(
        t("Remote interval (minutes)"),
        1.0,
        1440.0,
        sync.remote_interval_minutes as f64,
    );
    remote_minutes.set_visible(sync.remote_interval_enabled);
    remote.add(&push);
    remote.add(&remote_timer);
    remote.add(&remote_minutes);
    groups.push(remote);

    let excluded = libadwaita::PreferencesGroup::builder()
        .title(t("Excluded Files"))
        .build();
    let exclusions_enabled = libadwaita::SwitchRow::builder()
        .title(t("Exclude disposable files"))
        .active(sync.exclude_patterns_enabled)
        .build();
    excluded.add(&exclusions_enabled);
    let edit_row = libadwaita::ActionRow::builder()
        .title(t("File patterns"))
        .subtitle(t("Names, extensions, and wildcard patterns"))
        .activatable(true)
        .build();
    let next = gtk4::Image::builder()
        .icon_name("go-next-symbolic")
        .pixel_size(16)
        .build();
    edit_row.add_suffix(&next);
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let row = edit_row.clone();
        edit_row.connect_activated(move |_| {
            let dialog =
                ExclusionsDialog::new(store.clone(), account_id.clone(), callbacks.clone());
            dialog.present(Some(row.upcast_ref::<gtk4::Widget>()));
        });
    }
    excluded.add(&edit_row);
    groups.push(excluded);

    let reliability = libadwaita::PreferencesGroup::builder()
        .title(t("Reliability"))
        .build();
    let retries = spin_row(
        t("Maximum sync retries"),
        1.0,
        10.0,
        sync.max_sync_retries as f64,
    );
    reliability.add(&retries);
    groups.push(reliability);

    let widgets = SyncWidgets {
        banner,
        inotify,
        local_timer,
        local_minutes,
        push,
        remote_timer,
        remote_minutes,
        exclusions_enabled,
        retries,
    };

    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let widgets_guard = widgets.clone();
        widgets.inotify.connect_active_notify(move |_| {
            save_sync(&store, &account_id, &callbacks, &widgets_guard);
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let widgets_guard = widgets.clone();
        widgets.local_timer.connect_active_notify(move |_| {
            save_sync(&store, &account_id, &callbacks, &widgets_guard);
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let widgets_guard = widgets.clone();
        widgets.local_minutes.connect_value_notify(move |_| {
            save_sync(&store, &account_id, &callbacks, &widgets_guard);
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let widgets_guard = widgets.clone();
        widgets.push.connect_active_notify(move |_| {
            save_sync(&store, &account_id, &callbacks, &widgets_guard);
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let widgets_guard = widgets.clone();
        widgets.remote_timer.connect_active_notify(move |_| {
            save_sync(&store, &account_id, &callbacks, &widgets_guard);
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let widgets_guard = widgets.clone();
        widgets.remote_minutes.connect_value_notify(move |_| {
            save_sync(&store, &account_id, &callbacks, &widgets_guard);
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let widgets_guard = widgets.clone();
        widgets.exclusions_enabled.connect_active_notify(move |_| {
            save_sync(&store, &account_id, &callbacks, &widgets_guard);
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let widgets_guard = widgets.clone();
        widgets.retries.connect_value_notify(move |_| {
            save_sync(&store, &account_id, &callbacks, &widgets_guard);
        });
    }

    let _ = widgets;
    groups
}

/// Synchronization page: the per-account trigger options, exclusions,
/// reliability, deletion guard and detailed output (issue #63 moves them
/// here from the account panel).
fn build_sync_page(
    store: &ConfigStore,
    account_id: &str,
    account: &AccountConfig,
    callbacks: &SettingsCallbacks,
    host: &SettingsHost,
) -> libadwaita::PreferencesPage {
    let page = libadwaita::PreferencesPage::builder()
        .title(t("Synchronization"))
        .icon_name("emblem-synchronizing-symbolic")
        .build();
    for group in sync_option_groups(store, account_id, account, callbacks, host) {
        page.add(&group);
    }
    let detailed_group = libadwaita::PreferencesGroup::new();
    detailed_group.add(&detailed_output_row(store, account_id, callbacks, account));
    page.add(&detailed_group);
    page.add(&deletion_guard_group(store, account_id, account));
    page
}

/// Global folder-size confirmation threshold (issue #36 / #56): `0` disables.
pub(crate) fn size_confirmation_group(store: &ConfigStore) -> libadwaita::PreferencesGroup {
    let size_group = libadwaita::PreferencesGroup::builder()
        .title(t("Folder Size Confirmation"))
        .build();
    let threshold_mb = store
        .load()
        .map(|config| config.general.size_confirm_threshold_mb)
        .unwrap_or(500);
    let size_row = spin_row(
        t("Ask before syncing folders larger than"),
        0.0,
        1_000_000.0,
        threshold_mb as f64,
    );
    let unit = gtk4::Label::new(Some(t("MB")));
    unit.set_valign(gtk4::Align::Center);
    size_row.add_suffix(&unit);
    size_group.add(&size_row);
    let store = store.clone();
    size_row.connect_value_notify(move |row| {
        let megabytes = row.value() as i64;
        if let Err(error) = persist_config(&store, |config| {
            config.general.size_confirm_threshold_mb = megabytes;
        }) {
            eprintln!("Settings: could not save the size threshold: {error}");
        }
    });
    size_group
}

/// Network page: global network-wide settings (Wi-Fi allowlist and transfer
/// impact, issue #56). The proxy and TLS trust moved to the per-account
/// panel in the main window.
fn build_network_page(
    store: &ConfigStore,
    network: &crate::storage::config::NetworkConfig,
) -> libadwaita::PreferencesPage {
    let page = libadwaita::PreferencesPage::builder()
        .title(t("Network"))
        .icon_name("network-wired-symbolic")
        .build();

    let impact = libadwaita::SwitchRow::builder()
        .title(t("Reduce transfer impact"))
        .subtitle(t(
            "Runs the sync engine with idle IO priority and low CPU priority so transfers do not saturate the machine. It is a priority hint, not a speed limit.",
        ))
        .active(network.reduce_transfer_impact)
        .build();

    let ssids = libadwaita::EntryRow::new();
    ssids.set_title(t("Only sync on these Wi-Fi networks"));
    ssids.set_text(network.allowed_ssids.as_deref().unwrap_or(""));
    ssids.set_show_apply_button(true);
    ssids.set_tooltip_text(Some(t(
        "Comma-separated network names. Leave empty to sync on any network.",
    )));

    {
        let store = store.clone();
        let impact_guard = impact.clone();
        let ssids_guard = ssids.clone();
        impact.connect_active_notify(move |_| {
            save_network(&store, &impact_guard, &ssids_guard);
        });
    }
    {
        let store = store.clone();
        let impact_guard = impact.clone();
        let ssids_guard = ssids.clone();
        ssids.connect_apply(move |_| {
            save_network(&store, &impact_guard, &ssids_guard);
        });
    }

    let wifi = libadwaita::PreferencesGroup::builder()
        .title(t("Wi-Fi"))
        .build();
    wifi.add(&ssids);
    page.add(&wifi);

    let transfers = libadwaita::PreferencesGroup::builder()
        .title(t("Transfers"))
        .build();
    transfers.add(&impact);
    page.add(&transfers);

    page
}

/// Advanced page: logging, detailed output, deletion guard, diagnostics and
/// the typed account removal.
#[allow(clippy::too_many_arguments)]
fn build_advanced_page(
    store: &ConfigStore,
    logging: &LoggingConfig,
    callbacks: &SettingsCallbacks,
) -> libadwaita::PreferencesPage {
    let page = libadwaita::PreferencesPage::builder()
        .title(t("Advanced"))
        .icon_name("applications-system-symbolic")
        .build();

    // Logging (top-level config section).
    let logging_group = libadwaita::PreferencesGroup::builder()
        .title(t("Logging"))
        .build();
    let save_logs = libadwaita::SwitchRow::builder()
        .title(t("Save log files"))
        .subtitle(t(
            "Live activity remains available when file logging is off.",
        ))
        .active(logging.save_logs)
        .build();
    let log_retention = spin_row(
        t("Keep daily logs (days)"),
        1.0,
        365.0,
        logging.retention_days as f64,
    );
    log_retention.set_sensitive(save_logs.is_active());

    {
        let store = store.clone();
        let save_logs_guard = save_logs.clone();
        let retention_guard = log_retention.clone();
        save_logs.connect_active_notify(move |_| {
            save_logging(&store, &save_logs_guard, &retention_guard);
        });
    }
    {
        let store = store.clone();
        let save_logs_guard = save_logs.clone();
        let retention_guard = log_retention.clone();
        log_retention.connect_value_notify(move |_| {
            save_logging(&store, &save_logs_guard, &retention_guard);
        });
    }

    logging_group.add(&save_logs);
    logging_group.add(&log_retention);

    let log_dir = state_dir();
    let log_folder = libadwaita::ActionRow::builder()
        .title(t("Log folder"))
        .subtitle(log_dir.to_string_lossy().into_owned())
        .activatable(true)
        .build();
    let folder_icon = gtk4::Image::builder()
        .icon_name("folder-symbolic")
        .pixel_size(16)
        .build();
    log_folder.add_prefix(&folder_icon);
    let next = gtk4::Image::builder()
        .icon_name("go-next-symbolic")
        .pixel_size(16)
        .build();
    log_folder.add_suffix(&next);
    log_folder.connect_activated(|_| open_log_folder());
    logging_group.add(&log_folder);

    logging_group.add(
        &libadwaita::ActionRow::builder()
            .title(t("Daily file naming"))
            .subtitle("nextsync-YYYY-MM-DD.log")
            .build(),
    );
    page.add(&logging_group);

    page.add(&size_confirmation_group(store));

    // Configuration backup (issue #47): export/import the whole settings
    // file. Keyring secrets are never part of it.
    let backup_group = libadwaita::PreferencesGroup::builder()
        .title(t("Backup"))
        .build();
    let export_row = libadwaita::ActionRow::builder()
        .title(t("Export configuration…"))
        .subtitle(t(
            "Save every account, folder and preference to a JSON file",
        ))
        .activatable(true)
        .build();
    let import_row = libadwaita::ActionRow::builder()
        .title(t("Import configuration…"))
        .subtitle(t("Replace the current configuration from a backup file"))
        .activatable(true)
        .build();
    for (row, icon) in [
        (&export_row, "document-save-symbolic"),
        (&import_row, "document-open-symbolic"),
    ] {
        let glyph = gtk4::Image::builder()
            .icon_name(icon)
            .pixel_size(16)
            .build();
        row.add_prefix(&glyph);
        let next = gtk4::Image::builder()
            .icon_name("go-next-symbolic")
            .pixel_size(16)
            .build();
        row.add_suffix(&next);
    }
    {
        let store = store.clone();
        export_row.connect_activated(move |_| export_configuration(&store));
    }
    {
        let store = store.clone();
        let callbacks = callbacks.clone();
        import_row.connect_activated(move |_| import_configuration(&store, &callbacks));
    }
    backup_group.add(&export_row);
    backup_group.add(&import_row);
    page.add(&backup_group);

    page
}

/// Detailed-output switch row (account-owned, issue #56): reused by the
/// account settings panel.
pub(crate) fn detailed_output_row(
    store: &ConfigStore,
    account_id: &str,
    callbacks: &SettingsCallbacks,
    account: &AccountConfig,
) -> libadwaita::SwitchRow {
    let detailed = libadwaita::SwitchRow::builder()
        .title(t("Detailed synchronization output"))
        .active(account.sync.detailed_output)
        .build();
    let store = store.clone();
    let account_id = account_id.to_string();
    let callbacks = callbacks.clone();
    let detailed_guard = detailed.clone();
    detailed.connect_active_notify(move |_| {
        if let Err(error) = persist_account(&store, &account_id, |account| {
            account.sync.detailed_output = detailed_guard.is_active();
        }) {
            eprintln!("Settings: could not save detailed output: {error}");
        }
        invoke(&callbacks.on_reconfigure);
    });
    detailed
}

/// Deletion-guard group (account-owned, issue #56): reused by the account
/// settings panel.
pub(crate) fn deletion_guard_group(
    store: &ConfigStore,
    account_id: &str,
    account: &AccountConfig,
) -> libadwaita::PreferencesGroup {
    let guard = libadwaita::PreferencesGroup::builder()
        .title(t("Deletion Guard"))
        .build();
    let guard_enabled = libadwaita::SwitchRow::builder()
        .title(t("Protect against mass local deletion"))
        .subtitle(t(
            "Recommended. Stops sync when the local folder loses many files at once.",
        ))
        .active(account.delete_guard.enabled)
        .build();
    let guard_count = spin_row(
        t("Review after this many missing files"),
        1.0,
        100_000.0,
        account.delete_guard.count_threshold as f64,
    );
    let guard_percent = spin_row(
        t("Review after this percentage is missing"),
        1.0,
        100.0,
        account.delete_guard.percent_threshold as f64,
    );

    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let enabled_guard = guard_enabled.clone();
        let count_guard = guard_count.clone();
        let percent_guard = guard_percent.clone();
        guard_enabled.connect_active_notify(move |_| {
            save_delete_guard(
                &store,
                &account_id,
                &enabled_guard,
                &count_guard,
                &percent_guard,
            );
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let enabled_guard = guard_enabled.clone();
        let count_guard = guard_count.clone();
        let percent_guard = guard_percent.clone();
        guard_count.connect_value_notify(move |_| {
            save_delete_guard(
                &store,
                &account_id,
                &enabled_guard,
                &count_guard,
                &percent_guard,
            );
        });
    }
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let enabled_guard = guard_enabled.clone();
        let count_guard = guard_count.clone();
        let percent_guard = guard_percent.clone();
        guard_percent.connect_value_notify(move |_| {
            save_delete_guard(
                &store,
                &account_id,
                &enabled_guard,
                &count_guard,
                &percent_guard,
            );
        });
    }

    guard.add(&guard_enabled);
    guard.add(&guard_count);
    guard.add(&guard_percent);
    guard
}

/// Authentication and account-removal groups (issue #56): reused by the
/// account settings panel.
pub(crate) fn account_action_groups(
    store: &ConfigStore,
    account_id: &str,
    account: &AccountConfig,
    callbacks: &SettingsCallbacks,
    host: &SettingsHost,
) -> Vec<libadwaita::PreferencesGroup> {
    let auth_group = libadwaita::PreferencesGroup::builder()
        .title(t("Authentication"))
        .build();
    let sign_in_again = libadwaita::ActionRow::builder()
        .title(t("Sign in again"))
        .tooltip_text(t("Re-enter credentials without removing the account"))
        .activatable(true)
        .build();
    let auth_icon = gtk4::Image::builder()
        .icon_name("dialog-password-symbolic")
        .pixel_size(16)
        .build();
    sign_in_again.add_prefix(&auth_icon);
    let auth_next = gtk4::Image::builder()
        .icon_name("go-next-symbolic")
        .pixel_size(16)
        .build();
    sign_in_again.add_suffix(&auth_next);
    let store_for_signin = store.clone();
    let account_id_for_signin = account_id.to_string();
    let account_for_signin = account.clone();
    let host_for_signin = host.clone();
    sign_in_again.connect_activated(move |_| {
        present_sign_in_again_dialog(
            &store_for_signin,
            &account_id_for_signin,
            &account_for_signin,
            &host_for_signin,
        );
    });
    auth_group.add(&sign_in_again);

    let account_group = libadwaita::PreferencesGroup::builder()
        .title(t("Account"))
        .build();
    let remove = libadwaita::ActionRow::builder()
        .title(t("Remove account"))
        .tooltip_text(t("Disconnect this account; local files are kept"))
        .activatable(true)
        .build();
    remove.add_css_class("error");
    let login_name = account.login_name.clone();
    let callbacks = callbacks.clone();
    let host = host.clone();
    remove.connect_activated(move |_| {
        present_remove_account(&login_name, &host, &callbacks);
    });
    account_group.add(&remove);

    vec![auth_group, account_group]
}

// ---------------------------------------------------------------------------
// Save helpers
// ---------------------------------------------------------------------------

/// Load → mutate → save the top-level configuration.
fn persist_config(
    store: &ConfigStore,
    mutate: impl FnOnce(&mut Config),
) -> Result<(), ConfigError> {
    let mut config = store.load()?;
    mutate(&mut config);
    store.save(&config)
}

/// Load → mutate → save one account (mirrors the Python `_sync_back` + save).
fn persist_account(
    store: &ConfigStore,
    account_id: &str,
    mutate: impl FnOnce(&mut AccountConfig),
) -> Result<(), ConfigError> {
    let Some(mut account) = store.account(account_id)? else {
        return Err(ConfigError::new("Account not found."));
    };
    mutate(&mut account);
    store.update_account(&account)
}

fn invoke(callback: &Option<SettingsCallback>) {
    if let Some(callback) = callback {
        callback();
    }
}

/// Persist an account's proxy + TLS-trust overrides and refresh the runtime
/// network config (issue #56). Shared by the account settings panel.
pub(crate) fn save_account_network(
    store: &ConfigStore,
    account_id: &str,
    proxy: &libadwaita::EntryRow,
    trust: &libadwaita::SwitchRow,
    callbacks: &SettingsCallbacks,
) {
    let value = proxy.text().trim().to_string();
    if !value.is_empty() && !valid_proxy_url(&value) {
        proxy.set_title(t("Invalid HTTP proxy URL"));
        proxy.add_css_class("error");
        return;
    }
    if let Err(error) = persist_account(store, account_id, |account| {
        account.custom_proxy = if value.is_empty() {
            None
        } else {
            Some(value.clone())
        };
        account.trust_invalid_certificates = trust.is_active();
    }) {
        eprintln!("Settings: could not save the account connection settings: {error}");
        return;
    }
    proxy.set_title(t("Custom HTTP proxy"));
    proxy.remove_css_class("error");
    invoke(&callbacks.on_reconfigure);
}

#[allow(clippy::too_many_arguments)]
fn save_general(
    store: &ConfigStore,
    autostart: &libadwaita::SwitchRow,
    notifications: &libadwaita::SwitchRow,
    server_notifications: &libadwaita::SwitchRow,
    quiet: &libadwaita::SwitchRow,
    quiet_start: &libadwaita::EntryRow,
    quiet_end: &libadwaita::EntryRow,
) {
    let hours = if quiet.is_active() {
        let start = quiet_start.text().trim().to_string();
        let end = quiet_end.text().trim().to_string();
        if crate::storage::config::valid_hhmm_public(&start)
            && crate::storage::config::valid_hhmm_public(&end)
        {
            Some((start, end))
        } else {
            quiet_start.add_css_class("error");
            quiet_end.add_css_class("error");
            None
        }
    } else {
        None
    };
    let hours_valid = quiet.is_active() == hours.is_some() || !quiet.is_active();
    if let Err(error) = persist_config(store, |config| {
        config.general.autostart = autostart.is_active();
        config.general.show_notifications = notifications.is_active();
        config.general.show_server_notifications = server_notifications.is_active();
        config.general.quiet_hours = hours.clone();
    }) {
        eprintln!("Settings: could not save general settings: {error}");
        return;
    }
    if hours_valid || !quiet.is_active() {
        quiet_start.remove_css_class("error");
        quiet_end.remove_css_class("error");
    }
    // Reflect the startup preference in the desktop session immediately
    // (atomic desktop-entry write under ~/.config/autostart).
    let enabled = autostart.is_active();
    if let Err(error) = crate::core::autostart::AutostartManager::new(None).set_enabled(enabled) {
        eprintln!("Settings: could not update the autostart entry: {error}");
    }
}

fn save_sync(
    store: &ConfigStore,
    account_id: &str,
    callbacks: &SettingsCallbacks,
    widgets: &SyncWidgets,
) {
    widgets
        .local_minutes
        .set_visible(widgets.local_timer.is_active());
    widgets
        .remote_minutes
        .set_visible(widgets.remote_timer.is_active());
    let local_minutes = widgets.local_minutes.value() as i64;
    let remote_minutes = widgets.remote_minutes.value() as i64;
    let retries = widgets.retries.value() as i64;
    if let Err(error) = persist_account(store, account_id, |account| {
        account.sync.local_inotify_enabled = widgets.inotify.is_active();
        account.sync.local_interval_enabled = widgets.local_timer.is_active();
        account.sync.local_interval_minutes = local_minutes;
        account.sync.remote_push_enabled = widgets.push.is_active();
        account.sync.remote_interval_enabled = widgets.remote_timer.is_active();
        account.sync.remote_interval_minutes = remote_minutes;
        account.sync.exclude_patterns_enabled = widgets.exclusions_enabled.is_active();
        account.sync.max_sync_retries = retries;
    }) {
        eprintln!("Settings: could not save sync settings: {error}");
        return;
    }
    let manual = crate::core::triggers::manual_only(&TriggerSettings {
        local_inotify_enabled: widgets.inotify.is_active(),
        local_interval_enabled: widgets.local_timer.is_active(),
        local_interval_minutes: local_minutes,
        remote_push_enabled: widgets.push.is_active(),
        remote_interval_enabled: widgets.remote_timer.is_active(),
        remote_interval_minutes: remote_minutes,
    });
    widgets.banner.set_revealed(manual);
    invoke(&callbacks.on_reconfigure);
}

fn save_logging(
    store: &ConfigStore,
    save_logs: &libadwaita::SwitchRow,
    retention: &libadwaita::SpinRow,
) {
    let enabled = save_logs.is_active();
    let days = retention.value() as i64;
    retention.set_sensitive(enabled);
    if let Err(error) = persist_config(store, |config| {
        config.logging.save_logs = enabled;
        config.logging.retention_days = days;
    }) {
        eprintln!("Settings: could not save logging settings: {error}");
    }
}

fn save_delete_guard(
    store: &ConfigStore,
    account_id: &str,
    enabled: &libadwaita::SwitchRow,
    count: &libadwaita::SpinRow,
    percent: &libadwaita::SpinRow,
) {
    if let Err(error) = persist_account(store, account_id, |account| {
        account.delete_guard.enabled = enabled.is_active();
        account.delete_guard.count_threshold = count.value() as i64;
        account.delete_guard.percent_threshold = percent.value() as i64;
    }) {
        eprintln!("Settings: could not save deletion guard: {error}");
    }
}

fn save_network(store: &ConfigStore, impact: &libadwaita::SwitchRow, ssids: &libadwaita::EntryRow) {
    let ssid_value = ssids.text().trim().to_string();
    if let Err(error) = persist_config(store, |config| {
        config.network.reduce_transfer_impact = impact.is_active();
        config.network.allowed_ssids = if ssid_value.is_empty() {
            None
        } else {
            Some(ssid_value.clone())
        };
    }) {
        eprintln!("Settings: could not save network settings: {error}");
    }
}

// ---------------------------------------------------------------------------
// Add Folder dialog
// ---------------------------------------------------------------------------

/// Present the Add Folder dialog for an account against a config store.
///
/// This is the single construction site for the dialog, shared by the
/// Settings window and the main window's account view. `parent`
/// is the transient-for widget — the Settings `PreferencesWindow` or the main
/// `ApplicationWindow`. `on_folder_added` runs after a folder is committed;
/// `on_error` is invoked with the validation message (the caller usually
/// surfaces it as a toast) in addition to the dialog's own inline re-present.
///
/// The remote-folder picker is always sensitive (see `populate_remote_picker`):
/// when the lookup fails or returns nothing the user can still type a remote
/// path into the adjacent entry, which is the actual source of truth.
pub fn present_add_folder_dialog(
    store: ConfigStore,
    account_id: String,
    parent: &gtk4::Widget,
    on_folder_added: Rc<dyn Fn()>,
    on_error: Rc<dyn Fn(String)>,
    previous: Option<(String, String)>,
    error: Option<String>,
) {
    let (previous_local, previous_remote) = previous.unwrap_or_default();
    let local_default = if previous_local.is_empty() {
        default_sync_root().to_string_lossy().into_owned()
    } else {
        previous_local
    };

    let dialog = libadwaita::AlertDialog::new(
        Some(t("Add Folder")),
        Some(t(
            "Choose a local folder and an optional remote folder to mirror from this account.",
        )),
    );
    let entry_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    let local_entry = libadwaita::EntryRow::new();
    local_entry.set_title(t("Local folder"));
    local_entry.set_text(&local_default);

    let choose = gtk4::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text(t("Choose a local folder to synchronize"))
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    let entry_for_picker = local_entry.clone();
    choose.connect_clicked(move |_| {
        choose_local_folder(entry_for_picker.clone());
    });
    local_entry.add_suffix(&choose);
    entry_box.append(&local_entry);

    let remote_entry = libadwaita::EntryRow::new();
    remote_entry.set_title(t("Remote folder (empty: use the local folder name)"));
    if !previous_remote.is_empty() {
        remote_entry.set_text(&previous_remote);
    }
    let remote_list = gtk4::StringList::new(&[]);
    let picker = gtk4::DropDown::from_strings(&[]);
    picker.set_model(Some(&remote_list));
    picker.set_selected(u32::MAX);
    picker.set_tooltip_text(Some(t("Choose an existing remote folder")));
    let entry_for_pick = remote_entry.clone();
    picker.connect_selected_notify(move |picker| {
        if let Some(item) = picker.selected_item() {
            if let Ok(item) = item.downcast::<gtk4::StringObject>() {
                entry_for_pick.set_text(&item.string());
                picker.set_selected(u32::MAX);
            }
        }
    });
    remote_entry.add_suffix(&picker);
    entry_box.append(&remote_entry);

    let picker_status = gtk4::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["caption"])
        .build();
    entry_box.append(&picker_status);

    if let Some(message) = error {
        let label = gtk4::Label::builder()
            .label(message)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["error"])
            .build();
        entry_box.append(&label);
    }

    dialog.set_extra_child(Some(&entry_box));
    dialog.add_response("cancel", t("Cancel"));
    dialog.add_response("add", t("Add"));
    dialog.set_response_appearance("add", libadwaita::ResponseAppearance::Suggested);

    if let Ok(Some(account)) = store.account(&account_id) {
        populate_remote_picker(
            &account_id,
            &account.server_url,
            &account.login_name,
            &remote_list,
            &picker_status,
        );
    }

    let store_for_response = store;
    let account_id_for_response = account_id;
    let on_folder_added_for_response = on_folder_added.clone();
    let on_error_for_response = on_error.clone();
    let parent_for_response = parent.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response != "add" {
            return;
        }
        let local_root = local_entry.text().to_string();
        let remote_text = remote_entry.text().to_string();
        let root = expanduser(&local_root);
        let outcome = if !root.is_absolute() {
            Err(ConfigError::new(t("Choose an absolute local folder.")))
        } else {
            remote_path_for(&local_root, &remote_text)
        };
        match outcome {
            Ok(remote) => {
                // Issue #35: probe both sides and review the facts (merge,
                // previously synchronized folder) before committing.
                review_then_add_folder(
                    store_for_response.clone(),
                    account_id_for_response.clone(),
                    root.to_string_lossy().into_owned(),
                    remote,
                    &parent_for_response,
                    on_folder_added_for_response.clone(),
                    on_error_for_response.clone(),
                );
            }
            Err(error) => {
                let message = error.to_string();
                on_error_for_response(message.clone());
                // Re-open with the typed values and an inline error so the user
                // can correct them without losing what was entered.
                present_add_folder_dialog(
                    store_for_response.clone(),
                    account_id_for_response.clone(),
                    &parent_for_response,
                    on_folder_added_for_response.clone(),
                    on_error_for_response.clone(),
                    Some((local_root, remote_text)),
                    Some(message),
                );
            }
        }
    });

    dialog.present(Some(parent));
}

/// Probe the folder pair and run the blocking first-sync review (issue #35)
/// before committing a new folder added through the Add Folder dialog.
///
/// The probe mirrors the wizard's: remote emptiness over a shallow WebDAV
/// PROPFIND (or the OpenCloud space probe, reusing the space of the
/// account's existing folders when the dialog carried none). Unknown
/// remote state or missing credentials degrade to a direct add, exactly
/// like the wizard falls through to `finish_setup`.
fn review_then_add_folder(
    store: ConfigStore,
    account_id: String,
    local_root: String,
    remote_path: String,
    parent: &gtk4::Widget,
    on_folder_added: Rc<dyn Fn()>,
    on_error: Rc<dyn Fn(String)>,
) {
    let Some(account) = store.account(&account_id).ok().flatten() else {
        return;
    };
    let server = account.server_url.clone();
    let login = account.login_name.clone();
    let provider = account.provider;
    // The Add Folder dialog does not edit spaces; an OpenCloud folder can
    // only reuse the space already configured for the account.
    let space_id = if provider == Provider::OpenCloud {
        account.folders.iter().find_map(|f| f.space_id.clone())
    } else {
        None
    };
    let remote_for_probe = remote_path.clone();
    let account_id_for_probe = account_id.clone();
    // Issue #36: the probe also estimates the remote size.
    let space_id_for_probe = space_id.clone();
    let probe = gio::spawn_blocking(move || -> Option<(bool, Option<u64>)> {
        let password = CredentialsStore::get_for_account(&account_id_for_probe, &server, &login)
            .ok()
            .flatten()?;
        let api = NextcloudApi::new();
        match provider {
            Provider::OpenCloud => {
                let space = space_id_for_probe?;
                let size = api
                    .opencloud_space_size(&server, &login, &password, &space)
                    .ok()
                    .flatten();
                api.probe_opencloud_space(&server, &login, &password, &space)
                    .ok()
                    .map(|has_children| (has_children, size))
            }
            Provider::Nextcloud => {
                let size = api
                    .remote_size(&server, &login, &password, &remote_for_probe)
                    .ok()
                    .flatten();
                api.probe_remote(&server, &login, &password, &remote_for_probe)
                    .ok()
                    .map(|has_children| (has_children, size))
            }
        }
    });

    // Issue #36: the confirmation threshold lives in the general settings.
    let threshold_bytes = size_threshold_bytes(&store);
    let local_root_for_facts = local_root.clone();
    let parent = parent.clone();
    let store_for_commit = store.clone();
    let account_id_for_commit = account_id.clone();
    let on_folder_added_for_commit = on_folder_added.clone();
    glib::spawn_future_local(async move {
        let (remote_empty, remote_size): (Option<bool>, Option<u64>) = match probe.await {
            Ok(Some((has_children, size))) => (Some(!has_children), size),
            Ok(None) | Err(_) => (None, None),
        };
        let facts = FirstSyncFacts {
            local_empty: local_folder_is_empty(&local_root_for_facts),
            remote_empty,
            remote_size,
            journal_names: stale_artifact_names(&expanduser(&local_root_for_facts)),
        };
        // Issue #36: remember that this folder's size was explicitly
        // accepted so the prompt is not repeated for it.
        let oversized = crate::core::sync_safety::first_sync_warnings(&facts, threshold_bytes)
            .contains(&crate::core::sync_safety::FirstSyncWarning::Oversized);
        let commit = {
            let store = store_for_commit.clone();
            let account_id = account_id_for_commit.clone();
            let local_root = local_root_for_facts.clone();
            let remote_path = remote_path.clone();
            let space_id = space_id.clone();
            let on_folder_added = on_folder_added_for_commit.clone();
            let on_error = on_error.clone();
            let parent = parent.clone();
            Rc::new(move |fresh: crate::ui::safety_review::FreshStart| {
                crate::ui::safety_review::apply_fresh_start(&local_root, fresh);
                match store.add_folder(
                    &account_id,
                    &FolderConfig {
                        id: String::new(),
                        local_root: local_root.clone(),
                        remote_path: remote_path.clone(),
                        space_id: space_id.clone(),
                        size_confirmed: oversized,
                    },
                ) {
                    Ok(_) => on_folder_added(),
                    Err(error) => {
                        let message = error.to_string();
                        on_error(message.clone());
                        present_add_folder_dialog(
                            store.clone(),
                            account_id.clone(),
                            &parent,
                            on_folder_added.clone(),
                            on_error.clone(),
                            Some((local_root.clone(), remote_path.clone())),
                            Some(message),
                        );
                    }
                }
            })
        };
        if review_required(&facts, threshold_bytes) {
            crate::ui::safety_review::present_first_sync_review(
                &parent,
                crate::ui::safety_review::FirstSyncReview {
                    title: t("Add Folder"),
                    base_body: t("Start synchronizing this folder now?"),
                    facts: &facts,
                    threshold_bytes,
                    size_target: &local_root_for_facts,
                    cancel_label: t("Cancel"),
                },
                commit,
                Rc::new(|| {}),
            );
        } else {
            commit(crate::ui::safety_review::FreshStart::No);
        }
    });
}

/// The folder-size confirmation threshold in bytes; `0` (disabled) and a
/// missing configuration map to `None` (issue #36).
fn size_threshold_bytes(store: &ConfigStore) -> Option<u64> {
    let megabytes = store
        .load()
        .map(|config| config.general.size_confirm_threshold_mb)
        .unwrap_or(500);
    if megabytes <= 0 {
        return None;
    }
    Some(megabytes as u64 * 1024 * 1024)
}

/// Present a folder chooser and write the selection into the entry row.
fn choose_local_folder(entry: libadwaita::EntryRow) {
    let dialog = gtk4::FileDialog::builder()
        .title(t("Choose NextCloud Folder"))
        .modal(true)
        .build();
    dialog.select_folder(
        None::<&gtk4::Window>,
        None::<&gio::Cancellable>,
        move |result| {
            if let Ok(file) = result {
                if let Some(path) = file.path() {
                    entry.set_text(&path.to_string_lossy());
                }
            }
        },
    );
}

/// Outcome of resolving the remote-folder list for the Add Folder picker.
///
/// Every error path maps to one of these variants so the UI can surface a
/// short, translated hint instead of silently graying out the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteFolderLookup {
    /// Folders resolved successfully (possibly empty).
    Ok(Vec<String>),
    /// No usable credential was found in the keyring.
    NoCredentials,
    /// The server rejected the stored credentials.
    AuthFailed,
    /// The folder list could not be retrieved (network, HTTP or protocol).
    Network,
}

/// Map the resolved keyring credential and the remote-folder fetch into a
/// single picker outcome.
///
/// Pure on purpose: the blocking I/O (Secret Service lookup + WebDAV PROPFIND)
/// happens in the caller, which feeds the already-resolved results here. This
/// keeps the three error paths unit-testable without a keyring or a live
/// server. `None` covers both "no item stored" and "the keyring itself was
/// unavailable" — in either case there is no password to authenticate with.
fn classify_remote_lookup(
    resolved: Option<(String, Result<Vec<String>, ApiError>)>,
) -> RemoteFolderLookup {
    match resolved {
        None => RemoteFolderLookup::NoCredentials,
        Some((_password, folders)) => match folders {
            Ok(list) => RemoteFolderLookup::Ok(list),
            Err(ApiError::AuthRejected) => RemoteFolderLookup::AuthFailed,
            Err(_) => RemoteFolderLookup::Network,
        },
    }
}

/// Fill the remote picker with folders that already exist on the server, and
/// report why when that is not possible.
///
/// The keyring lookup and the PROPFIND run off the UI thread (blocking Secret
/// Service + network); the model and the status label are updated back on the
/// main loop. The picker itself is left always-sensitive by the caller: an
/// empty model is harmless because the adjacent remote EntryRow is the source
/// of truth.
fn populate_remote_picker(
    account_id: &str,
    server: &str,
    username: &str,
    list: &gtk4::StringList,
    status: &gtk4::Label,
) {
    let account_id = account_id.to_string();
    let server = server.to_string();
    let username = username.to_string();
    let list = list.clone();
    let status = status.clone();
    let handle = gio::spawn_blocking(move || -> RemoteFolderLookup {
        let server_for_lookup = server.clone();
        let username_for_lookup = username.clone();
        let password = match CredentialsStore::get_for_account(
            &account_id,
            &server_for_lookup,
            &username_for_lookup,
        ) {
            Ok(Some(password)) => Some(password),
            Ok(None) => None,
            Err(_) => None,
        };
        let Some(password) = password else {
            return classify_remote_lookup(None);
        };
        let folders = NextcloudApi::new().list_remote_folders(&server, &username, &password);
        classify_remote_lookup(Some((password, folders)))
    });
    glib::spawn_future_local(async move {
        let Ok(outcome) = handle.await else {
            return;
        };
        match outcome {
            RemoteFolderLookup::Ok(folders) => {
                for folder in folders {
                    list.append(&folder);
                }
                // An empty list is fine: the user can still type a remote path.
                status.set_text("");
            }
            RemoteFolderLookup::NoCredentials => {
                status.set_text(t("No saved credentials for this account."));
            }
            RemoteFolderLookup::AuthFailed => {
                status.set_text(t("Could not authenticate with the server."));
            }
            RemoteFolderLookup::Network => {
                status.set_text(t("Could not reach the server."));
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Desktop integration switches
// ---------------------------------------------------------------------------

/// The folder the desktop integration switches target: the account's first
/// folder (the Python used the "active" folder; the rewrite has none).
/// Build the three desktop integration switches for the first folder of the
/// account, replicating `_build_desktop_integrations`: "Show in Files
/// sidebar" (Nautilus bookmark), "Show on Desktop" (shortcut) and "Use
/// special folder icon". Returns an empty list when the account has no
/// folders (the Python also hides the rows in that case).
///
/// Each switch applies its [`DesktopIntegration`] setter on toggle; a `false`
/// result (e.g. a missing icon asset) reverts the switch to the real state
/// and surfaces a toast.
/// Open the log folder in the file manager, creating it when missing.
fn open_log_folder() {
    let directory = state_dir();
    if let Err(error) = std::fs::create_dir_all(&directory) {
        eprintln!("Settings: could not create log folder: {error}");
        return;
    }
    let _ = gio::AppInfo::launch_default_for_uri(
        &format!("file://{}", directory.display()),
        None::<&gio::AppLaunchContext>,
    );
}

// ---------------------------------------------------------------------------
// ExclusionsDialog
// ---------------------------------------------------------------------------

/// Dialog to edit the account exclusion patterns.
///
/// Mirrors `ExclusionsDialog` from `settings.py`: a list of patterns with an
/// add row, per-pattern removal and a Restore Defaults action. Every mutation
/// persists through `update_account` and invokes `on_reconfigure`.
pub struct ExclusionsDialog {
    dialog: libadwaita::Dialog,
    store: ConfigStore,
    account_id: String,
    callbacks: SettingsCallbacks,
    listbox: gtk4::ListBox,
    entry: gtk4::Entry,
    error_label: gtk4::Label,
}

impl ExclusionsDialog {
    /// Build the dialog. Call [`present`](Self::present) to show it.
    pub fn new(store: ConfigStore, account_id: String, callbacks: SettingsCallbacks) -> Self {
        let dialog = libadwaita::Dialog::new();
        dialog.set_title(t("Excluded Files"));
        dialog.set_content_width(520);
        dialog.set_content_height(580);

        let toolbar = libadwaita::ToolbarView::new();
        let header = gtk4::HeaderBar::new();
        let done = gtk4::Button::builder()
            .label(t("Done"))
            .css_classes(["suggested-action"])
            .build();
        let dialog_guard = dialog.clone();
        done.connect_clicked(move |_| {
            dialog_guard.close();
        });
        header.pack_end(&done);
        toolbar.add_top_bar(&header);

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        let explanation = gtk4::Label::builder()
            .label(t("Only file names, extensions, and wildcard patterns are allowed. Folders and paths cannot be excluded."))
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        content.append(&explanation);

        let listbox = gtk4::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        content.append(&listbox);

        let entry_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let entry = gtk4::Entry::new();
        entry.set_placeholder_text(Some(t("Example: *.swp")));
        entry.set_hexpand(true);
        entry_box.append(&entry);
        let add = gtk4::Button::builder()
            .label(t("Add Pattern"))
            .css_classes(["suggested-action"])
            .build();
        entry_box.append(&add);
        content.append(&entry_box);

        let error_label = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["error"])
            .build();
        content.append(&error_label);

        let restore = gtk4::Button::builder()
            .label(t("Restore Defaults"))
            .tooltip_text(t(
                "Reset the exclusion patterns to the recommended defaults",
            ))
            .halign(gtk4::Align::Start)
            .build();
        content.append(&restore);

        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_vexpand(true);
        scroller.set_child(Some(&content));
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));

        let dialog_self = Self {
            dialog,
            store,
            account_id,
            callbacks,
            listbox,
            entry,
            error_label,
        };

        let state = dialog_self.handle();
        let entry_state = Rc::clone(&state);
        let entry_handle = dialog_self.entry.clone();
        entry_handle.connect_activate(move |_| {
            entry_state.borrow_mut().add();
        });
        let add_state = Rc::clone(&state);
        add.connect_clicked(move |_| {
            add_state.borrow_mut().add();
        });
        let restore_state = Rc::clone(&state);
        restore.connect_clicked(move |_| {
            restore_state.borrow_mut().restore();
        });

        dialog_self.refresh();
        dialog_self
    }

    /// An independent handle to the same underlying widgets, so closures can
    /// mutate the dialog without borrowing `self`.
    fn handle(&self) -> Rc<RefCell<ExclusionsDialog>> {
        Rc::new(RefCell::new(ExclusionsDialog {
            dialog: self.dialog.clone(),
            store: self.store.clone(),
            account_id: self.account_id.clone(),
            callbacks: self.callbacks.clone(),
            listbox: self.listbox.clone(),
            entry: self.entry.clone(),
            error_label: self.error_label.clone(),
        }))
    }

    /// Present the dialog as a child of `parent`.
    pub fn present(&self, parent: Option<&gtk4::Widget>) {
        self.dialog.present(parent);
    }

    fn patterns(&self) -> Vec<String> {
        match self.store.account(&self.account_id) {
            Ok(Some(account)) => account.sync.exclude_patterns.clone(),
            _ => Vec::new(),
        }
    }

    fn save_patterns(&self, patterns: &[String]) {
        if let Err(error) = persist_account(&self.store, &self.account_id, |account| {
            account.sync.exclude_patterns = patterns.to_vec();
        }) {
            eprintln!("Settings: could not save exclusion patterns: {error}");
            return;
        }
        invoke(&self.callbacks.on_reconfigure);
    }

    fn refresh(&self) {
        self.listbox.remove_all();
        for pattern in self.patterns() {
            let row = libadwaita::ActionRow::builder()
                .title(pattern.as_str())
                .build();
            let remove = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk4::Align::Center)
                .tooltip_text(t("Remove pattern"))
                .css_classes(["flat"])
                .build();
            let state = self.handle();
            let pattern = pattern.clone();
            remove.connect_clicked(move |_| {
                state.borrow_mut().remove(&pattern);
            });
            row.add_suffix(&remove);
            self.listbox.append(&row);
        }
    }

    fn add(&mut self) {
        match validate_pattern(&self.entry.text()) {
            Ok(pattern) => {
                let mut patterns = self.patterns();
                if !patterns.iter().any(|item| item == &pattern) {
                    patterns.push(pattern);
                    self.save_patterns(&patterns);
                }
                self.entry.set_text("");
                self.error_label.set_text("");
                self.refresh();
            }
            Err(error) => {
                self.error_label.set_text(&error.to_string());
            }
        }
    }

    fn remove(&mut self, pattern: &str) {
        let mut patterns = self.patterns();
        patterns.retain(|item| item != pattern);
        self.save_patterns(&patterns);
        self.refresh();
    }

    fn restore(&mut self) {
        let defaults = DEFAULT_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        self.save_patterns(&defaults);
        self.refresh();
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Widgets touched by the Synchronization page save handler.
#[derive(Clone)]
struct SyncWidgets {
    banner: libadwaita::Banner,
    inotify: libadwaita::SwitchRow,
    local_timer: libadwaita::SwitchRow,
    local_minutes: libadwaita::SpinRow,
    push: libadwaita::SwitchRow,
    remote_timer: libadwaita::SwitchRow,
    remote_minutes: libadwaita::SpinRow,
    exclusions_enabled: libadwaita::SwitchRow,
    retries: libadwaita::SpinRow,
}

/// Build an integer `SpinRow` (mirrors the Python `_spin_row`).
fn spin_row(title: &str, lower: f64, upper: f64, value: f64) -> libadwaita::SpinRow {
    let row = libadwaita::SpinRow::with_range(lower, upper, 1.0);
    row.set_title(title);
    row.set_value(value);
    row
}

/// Light proxy validation (scheme + non-empty authority, no userinfo).
fn valid_proxy_url(value: &str) -> bool {
    let value = value.trim();
    let Some(rest) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    else {
        return false;
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !netloc.is_empty() && !netloc.contains('@')
}

/// The typed "remove" confirmation matches case-insensitively, like Python.
fn typed_confirmation_matches(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("remove")
}

/// Re-enter the credentials for an account without removing it. The dialog
/// validates against the server, then stores the new password in the keyring
/// and updates the login name if it changed. Stays open with an inline error
/// on failure so the user can retry without losing what they typed.
fn present_sign_in_again_dialog(
    store: &ConfigStore,
    account_id: &str,
    account: &AccountConfig,
    host: &SettingsHost,
) {
    let dialog = libadwaita::AlertDialog::new(
        Some(t("Sign in again")),
        Some(t(
            "Re-enter the credentials for this account. The previous password is replaced.",
        )),
    );
    let entry_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    let username = libadwaita::EntryRow::builder().title(t("Username")).build();
    username.set_text(&account.login_name);

    let password = libadwaita::PasswordEntryRow::new();
    password.set_title(t("Password"));

    let status = gtk4::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["caption"])
        .build();

    entry_box.append(&username);
    entry_box.append(&password);
    entry_box.append(&status);
    dialog.set_extra_child(Some(&entry_box));

    dialog.add_response("cancel", t("Cancel"));
    dialog.add_response("signin", t("Sign In"));
    dialog.set_response_appearance("signin", libadwaita::ResponseAppearance::Suggested);
    dialog.set_can_close(false);

    let store = store.clone();
    let account_id = account_id.to_string();
    let server = account.server_url.clone();
    let username_w = username.clone();
    let password_w = password.clone();
    let status_w = status.clone();
    let parent_w = host.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response == "cancel" {
            dialog.force_close();
            return;
        }
        if response != "signin" {
            return;
        }
        let user = username_w.text().to_string();
        let pass = password_w.text().to_string();
        if user.is_empty() || pass.is_empty() {
            status_w.set_text(t("Username and password are required."));
            return;
        }
        status_w.set_text(t("Checking credentials…"));

        let server_c = server.clone();
        let user_c = user.clone();
        let pass_c = pass.clone();
        let handle = gio::spawn_blocking(move || {
            NextcloudApi::new().validate_credentials(&server_c, &user_c, &pass_c)
        });

        let store = store.clone();
        let account_id = account_id.clone();
        let user_save = user.clone();
        let pass_save = pass.clone();
        let status_w2 = status_w.clone();
        let parent_w2 = parent_w.clone();
        let dialog_w = dialog.clone();
        glib::spawn_future_local(async move {
            let result = match handle.await {
                Ok(r) => r,
                Err(_) => {
                    status_w2.set_text(t("Could not verify the credentials."));
                    return;
                }
            };
            match result {
                Ok(_) => {
                    if let Err(err) = CredentialsStore::set(&account_id, &pass_save) {
                        status_w2.set_text(&format!("{} {err}", t("Could not save credentials.")));
                        return;
                    }
                    if let Err(err) = persist_account(&store, &account_id, |account| {
                        account.login_name = user_save.clone();
                    }) {
                        status_w2
                            .set_text(&format!("{} {err}", t("Could not update the account.")));
                        return;
                    }
                    let toast = libadwaita::Toast::new(t("Signed in."));
                    parent_w2.add_toast(toast);
                    dialog_w.force_close();
                }
                Err(ApiError::AuthRejected) => {
                    status_w2.set_text(t("Could not authenticate with the server."));
                }
                Err(other) => {
                    status_w2.set_text(&format!("{other}"));
                }
            }
        });
    });

    dialog.present(Some(host.parent()));
}

/// The two-step Remove account flow (issue #35).
fn present_remove_account(login_name: &str, host: &SettingsHost, callbacks: &SettingsCallbacks) {
    let dialog = libadwaita::AlertDialog::new(
        Some(t("Remove Nextcloud Account?")),
        Some(t("The account credential will be removed from the password keyring. Your local NextCloud folder and all files inside it will remain untouched.")),
    );
    dialog.add_response("cancel", t("Cancel"));
    dialog.add_response("remove", t("Continue"));
    dialog.set_response_appearance("remove", libadwaita::ResponseAppearance::Destructive);

    let login_name = login_name.to_string();
    let host_for_response = host.clone();
    let callbacks_for_response = callbacks.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response == "remove" {
            present_remove_account_step_two(
                &login_name,
                &host_for_response,
                &callbacks_for_response,
            );
        }
    });
    dialog.present(Some(host.parent()));
}

/// Second step: the user must type “remove” to proceed.
fn present_remove_account_step_two(
    login_name: &str,
    host: &SettingsHost,
    callbacks: &SettingsCallbacks,
) {
    let dialog = libadwaita::AlertDialog::new(
        Some(t(&format!("Remove {login_name}?"))),
        Some(t("Type “remove” to confirm. This cannot be undone and stops synchronization immediately.")),
    );
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some(t("Type “remove”")));
    let entry_box = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    entry_box.append(&entry);
    dialog.set_extra_child(Some(&entry_box));
    dialog.add_response("cancel", t("Cancel"));
    dialog.add_response("remove", t("Remove account"));
    dialog.set_response_appearance("remove", libadwaita::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));

    let host_for_response = host.clone();
    let callbacks_for_response = callbacks.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response != "remove" {
            return;
        }
        if !typed_confirmation_matches(&entry.text()) {
            let toast = libadwaita::Toast::new("Type “remove” to confirm account removal.");
            host_for_response.add_toast(toast);
            return;
        }
        invoke(&callbacks_for_response.on_remove_account);
    });
    dialog.present(Some(host.parent()));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Export the full configuration to a user-chosen JSON file (issue #47).
fn export_configuration(store: &ConfigStore) {
    let dialog = gtk4::FileDialog::new();
    dialog.set_title(t("Export configuration"));
    dialog.set_initial_name(Some("nextsync-config.json"));
    let config = match store.load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Settings: export could not read the configuration: {error}");
            return;
        }
    };
    dialog.save(
        None::<&gtk4::Window>,
        None::<&gio::Cancellable>,
        move |result| {
            let Ok(file) = result else {
                // A dismissed dialog is a plain cancel, not a failure.
                return;
            };
            let body = match serde_json::to_string_pretty(&config) {
                Ok(body) => body,
                Err(error) => {
                    eprintln!("Settings: export serialization failed: {error}");
                    return;
                }
            };
            if let Err(error) = std::fs::write(file.path().unwrap_or_default(), body) {
                eprintln!("Settings: export write failed: {error}");
            }
        },
    );
}

/// Import a configuration backup, validating it through the same loader
/// before replacing the current settings atomically (issue #47).
fn import_configuration(store: &ConfigStore, callbacks: &SettingsCallbacks) {
    let dialog = gtk4::FileDialog::new();
    dialog.set_title(t("Import configuration"));
    let store = store.clone();
    let callbacks = callbacks.clone();
    dialog.open(
        None::<&gtk4::Window>,
        None::<&gio::Cancellable>,
        move |result| {
            let Ok(file) = result else {
                return;
            };
            let path = file.path().unwrap_or_default();
            let imported = std::fs::read_to_string(&path).ok().and_then(|body| {
                serde_json::from_str::<crate::storage::config::Config>(&body).ok()
            });
            match imported {
                Some(config) => {
                    // `save` performs the same schema validation and
                    // atomic replace as every other write.
                    if let Err(error) = store.save(&config) {
                        eprintln!("Settings: import save failed: {error}");
                        return;
                    }
                    invoke(&callbacks.on_reconfigure);
                }
                None => {
                    eprintln!("Settings: import file is not a valid NextSync configuration");
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{reset_locale, set_locale, Locale};
    use tempfile::tempdir;

    fn sample_account() -> AccountConfig {
        AccountConfig {
            server_url: "https://cloud.example.com".to_string(),
            login_name: "alice".to_string(),
            authentication_type: "manual".to_string(),
            folders: vec![FolderConfig {
                id: "folder-1".to_string(),
                local_root: "/tmp/nsync-sample".to_string(),
                remote_path: "/docs".to_string(),
                space_id: None,
                size_confirmed: false,
            }],
            ..AccountConfig::default()
        }
    }

    // ---- pure helpers ------------------------------------------------------

    #[test]
    fn typed_confirmation_requires_literal_remove() {
        assert!(typed_confirmation_matches("remove"));
        assert!(typed_confirmation_matches(" REMOVE "));
        assert!(!typed_confirmation_matches("remov"));
        assert!(!typed_confirmation_matches(""));
        assert!(!typed_confirmation_matches("remove now"));
    }

    #[test]
    fn proxy_validation_is_light() {
        assert!(valid_proxy_url("http://proxy.example.com:8080"));
        assert!(valid_proxy_url("https://proxy.example.com"));
        assert!(!valid_proxy_url(""));
        assert!(!valid_proxy_url("ftp://proxy.example.com"));
        assert!(!valid_proxy_url("proxy.example.com"));
        assert!(!valid_proxy_url("http://user@proxy.example.com"));
    }

    #[test]
    fn classify_lookup_success_keeps_the_folder_list() {
        assert_eq!(
            classify_remote_lookup(Some((
                "pw".to_string(),
                Ok(vec!["/Documents".to_string(), "/Photos".to_string()])
            ))),
            RemoteFolderLookup::Ok(vec!["/Documents".to_string(), "/Photos".to_string()])
        );
    }

    #[test]
    fn classify_lookup_empty_success_is_still_ok() {
        // An empty folder list is a legitimate result, not an error: the user
        // can still type a remote path next to the picker.
        assert_eq!(
            classify_remote_lookup(Some(("pw".to_string(), Ok(Vec::new())))),
            RemoteFolderLookup::Ok(Vec::new())
        );
    }

    #[test]
    fn classify_lookup_auth_rejection_maps_to_auth_failed() {
        assert_eq!(
            classify_remote_lookup(Some(("pw".to_string(), Err(ApiError::AuthRejected)))),
            RemoteFolderLookup::AuthFailed
        );
    }

    #[test]
    fn classify_lookup_non_auth_api_errors_map_to_network() {
        // Transport, unexpected HTTP status and a malformed body are all
        // "could not reach the server" from the user's point of view.
        assert_eq!(
            classify_remote_lookup(Some(("pw".to_string(), Err(ApiError::Transport)))),
            RemoteFolderLookup::Network
        );
        assert_eq!(
            classify_remote_lookup(Some((
                "pw".to_string(),
                Err(ApiError::Http { status: 500 })
            ))),
            RemoteFolderLookup::Network
        );
        assert_eq!(
            classify_remote_lookup(Some(("pw".to_string(), Err(ApiError::InvalidResponse)))),
            RemoteFolderLookup::Network
        );
    }

    #[test]
    fn classify_lookup_missing_credential_maps_to_no_credentials() {
        assert_eq!(
            classify_remote_lookup(None),
            RemoteFolderLookup::NoCredentials
        );
    }

    #[test]
    fn manual_only_predicate_drives_the_banner() {
        let sync = crate::storage::config::SyncConfig {
            local_inotify_enabled: false,
            local_interval_enabled: false,
            remote_push_enabled: false,
            remote_interval_enabled: false,
            ..crate::storage::config::SyncConfig::default()
        };
        assert!(crate::core::triggers::manual_only(&TriggerSettings::from(
            &sync
        )));
        let mut enabled = sync;
        enabled.local_inotify_enabled = true;
        assert!(!crate::core::triggers::manual_only(&TriggerSettings::from(
            &enabled
        )));
    }

    #[test]
    fn validate_pattern_rejects_paths_and_broad_glob() {
        assert!(validate_pattern("a/b").is_err());
        assert!(validate_pattern("*").is_err());
        assert_eq!(validate_pattern("*.swp").unwrap(), "*.swp");
    }

    // ---- persistence without GTK ------------------------------------------
    #[test]
    fn add_and_remove_folder_persist_on_disk() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        let account_id = store.add_account(&sample_account()).unwrap();

        let folder = FolderConfig {
            id: String::new(),
            local_root: "/tmp/nsync-settings-a".to_string(),
            remote_path: "/A".to_string(),
            space_id: None,
            size_confirmed: false,
        };
        let folder_id = store.add_folder(&account_id, &folder).unwrap();
        assert!(!folder_id.is_empty());

        let loaded = store.account(&account_id).unwrap().unwrap();
        assert_eq!(loaded.folders.len(), 2);
        assert!(loaded
            .folders
            .iter()
            .any(|item| item.local_root == "/tmp/nsync-settings-a"));

        assert!(store.remove_folder(&account_id, &folder_id).unwrap());
        let loaded = store.account(&account_id).unwrap().unwrap();
        assert_eq!(loaded.folders.len(), 1);
    }

    #[test]
    fn duplicate_folder_is_rejected_by_the_store() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_path(dir.path().join("settings.json"));
        let account_id = store.add_account(&sample_account()).unwrap();
        let folder = FolderConfig {
            id: String::new(),
            local_root: "/tmp/nsync-settings-b".to_string(),
            remote_path: "/B".to_string(),
            space_id: None,
            size_confirmed: false,
        };
        store.add_folder(&account_id, &folder).unwrap();
        let error = store.add_folder(&account_id, &folder).unwrap_err();
        assert!(error.message.contains("already configured"));
    }

    // ---- i18n --------------------------------------------------------------

    #[test]
    fn known_settings_strings_translate_to_spanish_and_back() {
        set_locale(Locale::Spanish);
        assert_eq!(t("Settings"), "Configuración");
        assert_eq!(t("Synchronization"), "Sincronización");
        assert_eq!(
            t("Start NextSync when I sign in"),
            "Iniciar NextSync al iniciar sesión"
        );
        set_locale(Locale::English);
        assert_eq!(t("Settings"), "Settings");
        assert_eq!(t("Synchronization"), "Synchronization");
        reset_locale();
    }

    // ---- GTK smoke --------------------------------------------------------

    #[test]
    fn settings_window_construction_smoke() {
        crate::ui::test_helpers::gtk_smoke(|| {
            // The ambient environment is Spanish (LANG=es_ES.UTF-8); pin the
            // locale on the GTK worker thread so the assertions are
            // deterministic, then verify the view actually localizes.
            set_locale(Locale::English);
            let dir = tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));
            let account = sample_account();
            let account_id = store.add_account(&account).unwrap();
            let host = test_host();
            let view = SettingsView::new(
                store.clone(),
                account.clone(),
                account_id.clone(),
                SettingsCallbacks::default(),
                &host,
            );
            assert_eq!(view.page_names().len(), 4);
            assert!(view
                .page_names()
                .iter()
                .any(|name| name == crate::ui::settings::page::ADVANCED));

            set_locale(Locale::Spanish);
            let view = SettingsView::new(
                store,
                account.clone(),
                account_id.clone(),
                SettingsCallbacks::default(),
                &host,
            );
            assert_eq!(view.page_names().len(), 4);
            reset_locale();
        });
    }

    /// A `SettingsHost` anchored to a bare window + toast overlay for tests.
    fn test_host() -> SettingsHost {
        let window = gtk4::Window::new();
        let toast = libadwaita::ToastOverlay::new();
        SettingsHost::new(&window, &toast)
    }

    /// The in-app view embeds exactly the four preference pages and switches
    /// between them in place (issue #10: no separate window anywhere).
    #[test]
    fn settings_view_embeds_four_pages_and_switches_in_place() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let dir = tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));
            let account = sample_account();
            let account_id = store.add_account(&account).unwrap();
            let view = SettingsView::new(
                store,
                account,
                account_id,
                SettingsCallbacks::default(),
                &test_host(),
            );

            let names: Vec<&str> = view.page_names().iter().map(String::as_str).collect();
            assert_eq!(
                names,
                [
                    page::GENERAL,
                    page::SYNCHRONIZATION,
                    page::NETWORK,
                    page::ADVANCED
                ]
            );
            for name in view.page_names() {
                assert!(
                    view.stack.child_by_name(name).is_some(),
                    "page {name} must live in the embedded ViewStack"
                );
            }

            // The pages switch in place inside the single embedded view.
            assert_eq!(
                view.stack.visible_child_name().as_deref(),
                Some(page::GENERAL)
            );
            // The page switcher must be revealed or only General is ever
            // reachable (issue #51).
            assert!(
                view.switcher_revealed(),
                "the ViewSwitcherBar must be revealed"
            );
            view.show_page(page::ADVANCED);
            assert_eq!(
                view.stack.visible_child_name().as_deref(),
                Some(page::ADVANCED)
            );
            view.show_page(page::NETWORK);
            assert_eq!(
                view.stack.visible_child_name().as_deref(),
                Some(page::NETWORK)
            );
            reset_locale();
        });
    }
}
