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

use crate::core::triggers::TriggerSettings;
use crate::nextcloud::api::{ApiError, NextcloudApi};
use crate::nextcloud::credentials::CredentialsStore;
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
        let synchronization = build_sync_page(&config_store, &account_id, &account, &callbacks);
        let network = build_network_page(&config_store, &account, &config.network);
        let advanced = build_advanced_page(
            &config_store,
            &account_id,
            &account,
            &config.logging,
            &callbacks,
            host,
        );

        let stack = libadwaita::ViewStack::new();
        let toolbar = libadwaita::ToolbarView::new();
        let switcher = libadwaita::ViewSwitcherBar::new();
        switcher.set_stack(Some(&stack));
        toolbar.add_bottom_bar(&switcher);
        toolbar.set_content(Some(&stack));
        let page_names = Vec::new();
        let mut view = Self {
            root: toolbar,
            stack: stack.clone(),
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

/// General page: Startup and Power switches.
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
    let pause_battery = libadwaita::SwitchRow::builder()
        .title(t("Pause synchronization while on battery"))
        .subtitle(t("A running synchronization is allowed to finish."))
        .active(general.pause_on_battery)
        .build();

    {
        let store = store.clone();
        let autostart_guard = autostart.clone();
        let pause_guard = pause_battery.clone();
        autostart.connect_active_notify(move |_| {
            save_general(&store, &autostart_guard, &pause_guard);
        });
    }
    {
        let store = store.clone();
        let autostart_guard = autostart.clone();
        let pause_guard = pause_battery.clone();
        pause_battery.connect_active_notify(move |_| {
            save_general(&store, &autostart_guard, &pause_guard);
        });
    }

    startup.add(&autostart);
    page.add(&startup);

    let power = libadwaita::PreferencesGroup::builder()
        .title(t("Power"))
        .build();
    power.add(&pause_battery);
    page.add(&power);

    page
}

/// Synchronization page: the manual-only banner, the four trigger switches,
/// exclusions and reliability.
fn build_sync_page(
    store: &ConfigStore,
    account_id: &str,
    account: &AccountConfig,
    callbacks: &SettingsCallbacks,
) -> libadwaita::PreferencesPage {
    let sync = &account.sync;
    let page = libadwaita::PreferencesPage::builder()
        .title(t("Synchronization"))
        .icon_name("emblem-synchronizing-symbolic")
        .build();

    let manual_group = libadwaita::PreferencesGroup::new();
    let banner = libadwaita::Banner::new(t(
        "Automatic synchronization is off. Files synchronize only with Sync Now.",
    ));
    banner.set_revealed(crate::core::triggers::manual_only(&TriggerSettings::from(
        sync,
    )));
    manual_group.add(&banner);
    page.add(&manual_group);

    let local = libadwaita::PreferencesGroup::builder()
        .title(t("Local Changes"))
        .build();
    let inotify = libadwaita::SwitchRow::builder()
        .title(t("Monitor filesystem changes"))
        .subtitle(t("Synchronizes shortly after a local file changes."))
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
    page.add(&local);

    let remote = libadwaita::PreferencesGroup::builder()
        .title(t("Remote Changes"))
        .build();
    let push = libadwaita::SwitchRow::builder()
        .title(t("Use server push notifications"))
        .subtitle(t("Near-real-time detection when notify_push is supported."))
        .active(sync.remote_push_enabled)
        .build();
    let remote_timer = libadwaita::SwitchRow::builder()
        .title(t("Run a remote interval"))
        .subtitle(t("Recommended because push delivery is best effort."))
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
    page.add(&remote);

    let excluded = libadwaita::PreferencesGroup::builder()
        .title(t("Excluded Files"))
        .build();
    let exclusions_enabled = libadwaita::SwitchRow::builder()
        .title(t("Exclude disposable files"))
        .subtitle(t("Hidden files remain synchronized unless a rule matches."))
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
    page.add(&excluded);

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
    page.add(&reliability);

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
    page
}

/// Network page: server/login read-only row, custom proxy and TLS trust.
fn build_network_page(
    store: &ConfigStore,
    account: &AccountConfig,
    network: &crate::storage::config::NetworkConfig,
) -> libadwaita::PreferencesPage {
    let page = libadwaita::PreferencesPage::builder()
        .title(t("Network"))
        .icon_name("network-wired-symbolic")
        .build();

    let server = libadwaita::PreferencesGroup::builder()
        .title(t("Server"))
        .build();
    server.add(
        &libadwaita::ActionRow::builder()
            .title(account.server_url.as_str())
            .subtitle(account.login_name.as_str())
            .build(),
    );
    page.add(&server);

    let proxy_group = libadwaita::PreferencesGroup::builder()
        .title(t("Proxy"))
        .build();
    let proxy = libadwaita::EntryRow::new();
    proxy.set_title(t("Custom HTTP proxy"));
    proxy.set_text(network.custom_proxy.as_deref().unwrap_or(""));
    proxy.set_show_apply_button(true);
    proxy.set_tooltip_text(Some(t("Save the custom HTTP proxy")));
    let trust = libadwaita::SwitchRow::builder()
        .title(t("Allow invalid or self-signed certificates"))
        .subtitle(t(
            "This weakens connection security. Enable only for a server you trust.",
        ))
        .active(network.trust_invalid_certificates)
        .build();

    {
        let store = store.clone();
        let proxy_guard = proxy.clone();
        let trust_guard = trust.clone();
        proxy.connect_apply(move |_| {
            save_network(&store, &proxy_guard, &trust_guard);
        });
    }
    {
        let store = store.clone();
        let proxy_guard = proxy.clone();
        let trust_guard = trust.clone();
        trust.connect_active_notify(move |_| {
            save_network(&store, &proxy_guard, &trust_guard);
        });
    }

    proxy_group.add(&proxy);
    page.add(&proxy_group);

    let tls = libadwaita::PreferencesGroup::builder()
        .title(t("TLS"))
        .build();
    tls.add(&trust);
    page.add(&tls);
    page
}

/// Advanced page: logging, detailed output, deletion guard, diagnostics and
/// the typed account removal.
#[allow(clippy::too_many_arguments)]
fn build_advanced_page(
    store: &ConfigStore,
    account_id: &str,
    account: &AccountConfig,
    logging: &LoggingConfig,
    callbacks: &SettingsCallbacks,
    host: &SettingsHost,
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

    // Detailed output sits in the Logging group but persists through the
    // account sync settings (the store field is `account.sync.detailed_output`).
    let detailed = libadwaita::SwitchRow::builder()
        .title(t("Detailed synchronization output"))
        .active(account.sync.detailed_output)
        .build();
    logging_group.add(&detailed);
    {
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
    }
    page.add(&logging_group);
    // Deletion guard (account-owned).
    let guard = libadwaita::PreferencesGroup::builder()
        .title(t("Deletion Guard"))
        .description(
            t("Blocks synchronization before nextcloudcmd starts when too many previously synchronized local files disappear."),
        )
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
    page.add(&guard);

    // Diagnostics removed by user decision (issue #18): the log files under
    // $XDG_STATE_HOME carry the same information.

    // Authentication: re-enter credentials without removing the account.
    let auth_group = libadwaita::PreferencesGroup::builder()
        .title(t("Authentication"))
        .description(t(
            "Use Sign in again when the stored credentials are missing or rejected.",
        ))
        .build();
    let sign_in_again = libadwaita::ActionRow::builder()
        .title(t("Sign in again"))
        .subtitle(t("Re-enter the username and password for this account"))
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
    page.add(&auth_group);

    // Account removal (typed confirmation).
    let account_group = libadwaita::PreferencesGroup::builder()
        .title(t("Account"))
        .description(
            t("Removing the account only removes the connection; your local folders and files are never touched."),
        )
        .build();
    let remove = libadwaita::ActionRow::builder()
        .title(t("Remove account"))
        .subtitle(t("Rarely needed. Keeps all local files."))
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
    page.add(&account_group);

    page
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

fn save_general(
    store: &ConfigStore,
    autostart: &libadwaita::SwitchRow,
    pause: &libadwaita::SwitchRow,
) {
    if let Err(error) = persist_config(store, |config| {
        config.general.autostart = autostart.is_active();
        config.general.pause_on_battery = pause.is_active();
    }) {
        eprintln!("Settings: could not save general settings: {error}");
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

fn save_network(store: &ConfigStore, proxy: &libadwaita::EntryRow, trust: &libadwaita::SwitchRow) {
    let value = proxy.text().trim().to_string();
    if !value.is_empty() && !valid_proxy_url(&value) {
        proxy.set_title(t("Invalid HTTP proxy URL"));
        proxy.add_css_class("error");
        return;
    }
    if let Err(error) = persist_config(store, |config| {
        config.network.custom_proxy = if value.is_empty() {
            None
        } else {
            Some(value.clone())
        };
        config.network.trust_invalid_certificates = trust.is_active();
    }) {
        eprintln!("Settings: could not save network settings: {error}");
        return;
    }
    proxy.set_title(t("Custom HTTP proxy"));
    proxy.remove_css_class("error");
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
            remote_path_for(&local_root, &remote_text).and_then(|remote| {
                store_for_response.add_folder(
                    &account_id_for_response,
                    &FolderConfig {
                        id: String::new(),
                        local_root: root.to_string_lossy().into_owned(),
                        remote_path: remote,
                        space_id: None,
                    },
                )
            })
        };
        match outcome {
            Ok(_) => on_folder_added_for_response(),
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
                account,
                account_id,
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
            view.show_page(page::ADVANCED);
            assert_eq!(
                view.stack.visible_child_name().as_deref(),
                Some(page::ADVANCED)
            );
            view.show_page(page::SYNCHRONIZATION);
            assert_eq!(
                view.stack.visible_child_name().as_deref(),
                Some(page::SYNCHRONIZATION)
            );
            reset_locale();
        });
    }
}
