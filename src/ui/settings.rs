//! Settings window (Task 5.2).
//!
//! Port of `ui/settings.py` (v0.4.0) to gtk-rs/libadwaita: a
//! `Adw.PreferencesWindow` with General, Synchronization, Network and
//! Advanced pages, the per-folder groups with the Add Folder flow (including
//! the remote folder picker, issue #25) and the typed Remove Account
//! confirmation (issue #35, moved to Advanced by the account-view redesign).
//!
//! # Deviations from `settings.py` (motivated)
//!
//! - i18n (Task 6.1): user-visible strings go through [`crate::util::i18n::t`];
//!   msgids missing from the Spanish catalog fall back to the English source.
//! - No `runtime`/`desktop_integration` parameters. The window only receives a
//!   [`ConfigStore`], the [`AccountConfig`] snapshot, the account id and the
//!   [`SettingsCallbacks`] closures. The live Diagnostics rows (inotify
//!   watches, push state) are dropped because the window has no handle to the
//!   runtimes; `runtime.last_exit_code` is shown instead.
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

use gio::prelude::ListModelExt;
use libadwaita::prelude::*;

use crate::core::triggers::TriggerSettings;
use crate::nextcloud::api::NextcloudApi;
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
    /// Invoked after the typed Remove Account confirmation succeeds.
    pub on_remove_account: Option<SettingsCallback>,
    /// Invoked after a folder is added or removed (refreshes the account view).
    pub on_folder_changed: Option<SettingsCallback>,
    /// Invoked after trigger/logging settings change (hot-reconfigures the
    /// account runtimes).
    pub on_reconfigure: Option<SettingsCallback>,
}

/// The Settings window: a `PreferencesWindow` with the four pages.
pub struct SettingsWindow {
    window: libadwaita::PreferencesWindow,
}

impl SettingsWindow {
    /// Build the window for one account.
    ///
    /// `account` is the snapshot used for the initial widget values;
    /// `account_id` is the key every write operation uses against the store.
    pub fn new(
        config_store: ConfigStore,
        account: AccountConfig,
        account_id: String,
        callbacks: SettingsCallbacks,
    ) -> Self {
        let window = libadwaita::PreferencesWindow::new();
        window.set_title(Some(t("Settings")));
        window.set_default_size(720, 640);

        // Top-level sections (general/logging/network) come from the current
        // configuration; account-owned settings come from the snapshot.
        let config = config_store.load().unwrap_or_default();

        let folder_ui = FolderUi {
            store: config_store.clone(),
            account_id: account_id.clone(),
            callbacks: callbacks.clone(),
            group: libadwaita::PreferencesGroup::new(),
            window: window.clone(),
            rows: Rc::new(RefCell::new(Vec::new())),
        };

        let general = build_general_page(&config_store, &config.general, &folder_ui.group);
        window.add(&general);

        let synchronization = build_sync_page(&config_store, &account_id, &account, &callbacks);
        window.add(&synchronization);

        let network = build_network_page(&config_store, &account, &config.network);
        window.add(&network);

        let advanced = build_advanced_page(
            &config_store,
            &account_id,
            &account,
            &config.logging,
            &callbacks,
            &window,
        );
        window.add(&advanced);

        folder_ui.refresh();

        Self { window }
    }

    /// The underlying window, for presentation.
    pub fn window(&self) -> &libadwaita::PreferencesWindow {
        &self.window
    }
}

// ---------------------------------------------------------------------------
// Page builders
// ---------------------------------------------------------------------------

/// General page: Startup switch, Power switch and the Synchronization Folders
/// group (managed by [`FolderUi`]).
fn build_general_page(
    store: &ConfigStore,
    general: &GeneralConfig,
    folders_group: &libadwaita::PreferencesGroup,
) -> libadwaita::PreferencesPage {
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

    let folders = libadwaita::PreferencesGroup::builder()
        .title(t("Synchronization Folders"))
        .build();
    folders.add(folders_group);
    page.add(&folders);

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
    window: &libadwaita::PreferencesWindow,
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

    // Diagnostics.
    let diagnostics = libadwaita::PreferencesGroup::builder()
        .title(t("Diagnostics"))
        .build();
    let last_code = account
        .runtime
        .last_exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| t("None").to_string());
    diagnostics.add(
        &libadwaita::ActionRow::builder()
            .title(t("Last exit code"))
            .subtitle(last_code)
            .build(),
    );
    page.add(&diagnostics);

    // Account removal (typed confirmation).
    let account_group = libadwaita::PreferencesGroup::builder()
        .title(t("Account"))
        .description(
            t("Removing the account only removes the connection; your local folders and files are never touched."),
        )
        .build();
    let remove = libadwaita::ActionRow::builder()
        .title(t("Remove Account"))
        .subtitle(t("Rarely needed. Keeps all local files."))
        .activatable(true)
        .build();
    remove.add_css_class("error");
    let login_name = account.login_name.clone();
    let callbacks = callbacks.clone();
    let window = window.clone();
    remove.connect_activated(move |_| {
        present_remove_account(&login_name, &window, &callbacks);
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
// Synchronization Folders group
// ---------------------------------------------------------------------------

/// Shared state for the Synchronization Folders group: the store, the account
/// id and the widgets the Add/Remove flows rebuild.
#[derive(Clone)]
struct FolderUi {
    store: ConfigStore,
    account_id: String,
    callbacks: SettingsCallbacks,
    group: libadwaita::PreferencesGroup,
    window: libadwaita::PreferencesWindow,
    /// Rows added by [`refresh`](Self::refresh). `PreferencesGroup` keeps an
    /// internal box as its direct child, so only these rows may be removed.
    rows: Rc<RefCell<Vec<gtk4::Widget>>>,
}

impl FolderUi {
    /// Rebuild the folder rows from the current configuration.
    fn refresh(&self) {
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
        let Ok(Some(account)) = self.store.account(&self.account_id) else {
            return;
        };
        for folder in &account.folders {
            let row = libadwaita::ActionRow::builder()
                .title(folder.local_root.as_str())
                .subtitle(t("Remote: {remote}").replacen(
                    "{remote}",
                    folder_subtitle(&folder.remote_path),
                    1,
                ))
                .build();
            let icon = gtk4::Image::builder()
                .icon_name("folder-symbolic")
                .pixel_size(16)
                .build();
            row.add_prefix(&icon);
            let remove = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .valign(gtk4::Align::Center)
                .tooltip_text(t("Remove folder"))
                .css_classes(["flat"])
                .build();
            let folder_id = folder.id.clone();
            let ui = self.clone();
            remove.connect_clicked(move |_| {
                let _ = ui.store.remove_folder(&ui.account_id, &folder_id);
                ui.refresh();
                invoke(&ui.callbacks.on_folder_changed);
            });
            row.add_suffix(&remove);
            self.group.add(&row);
            self.rows.borrow_mut().push(row.upcast::<gtk4::Widget>());
        }

        let add_row = libadwaita::ActionRow::builder()
            .title(t("Add Folder"))
            .subtitle(t("Mirror another local folder from this account"))
            .activatable(true)
            .build();
        let add_icon = gtk4::Image::builder()
            .icon_name("folder-new-symbolic")
            .pixel_size(16)
            .build();
        add_row.add_prefix(&add_icon);
        let next = gtk4::Image::builder()
            .icon_name("go-next-symbolic")
            .pixel_size(16)
            .build();
        add_row.add_suffix(&next);
        let ui = self.clone();
        add_row.connect_activated(move |_| {
            ui.present_add_folder_dialog(None, None);
        });
        self.group.add(&add_row);
        self.rows
            .borrow_mut()
            .push(add_row.upcast::<gtk4::Widget>());
    }

    /// Present the Add Folder dialog. `previous` and `error` let a failed
    /// attempt re-open with the typed values and an inline message.
    fn present_add_folder_dialog(&self, previous: Option<(String, String)>, error: Option<String>) {
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
        picker.set_sensitive(false);
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

        if let Ok(Some(account)) = self.store.account(&self.account_id) {
            populate_remote_picker(
                &self.account_id,
                &account.server_url,
                &account.login_name,
                &picker,
                &remote_list,
            );
        }

        let store = self.store.clone();
        let account_id = self.account_id.clone();
        let folder_ui = self.clone();
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
                    store.add_folder(
                        &account_id,
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
                Ok(_) => {
                    folder_ui.refresh();
                    invoke(&folder_ui.callbacks.on_folder_changed);
                }
                Err(error) => {
                    let message = error.to_string();
                    let toast = libadwaita::Toast::new(&message);
                    folder_ui.window.add_toast(toast);
                    folder_ui
                        .present_add_folder_dialog(Some((local_root, remote_text)), Some(message));
                }
            }
        });

        dialog.present(Some(&self.window));
    }
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

/// Fill the remote picker with folders that already exist on the server.
///
/// The keyring lookup and the PROPFIND run off the UI thread (blocking Secret
/// Service + network); the model is updated back on the main loop.
fn populate_remote_picker(
    account_id: &str,
    server: &str,
    username: &str,
    picker: &gtk4::DropDown,
    list: &gtk4::StringList,
) {
    let account_id = account_id.to_string();
    let server = server.to_string();
    let username = username.to_string();
    let picker = picker.clone();
    let list = list.clone();
    let handle = gio::spawn_blocking(move || -> Vec<String> {
        let Ok(Some(password)) = CredentialsStore::get(&account_id) else {
            return Vec::new();
        };
        NextcloudApi::new()
            .list_remote_folders(&server, &username, &password)
            .unwrap_or_default()
    });
    glib::spawn_future_local(async move {
        if let Ok(folders) = handle.await {
            for folder in folders {
                list.append(&folder);
            }
            if list.n_items() > 0 {
                picker.set_sensitive(true);
            }
        }
    });
}

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

/// Remote subtitle of a folder row: the account root displays as `/`.
fn folder_subtitle(remote_path: &str) -> &str {
    if remote_path.is_empty() {
        "/"
    } else {
        remote_path
    }
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

/// The two-step Remove Account flow (issue #35).
fn present_remove_account(
    login_name: &str,
    window: &libadwaita::PreferencesWindow,
    callbacks: &SettingsCallbacks,
) {
    let dialog = libadwaita::AlertDialog::new(
        Some(t("Remove Nextcloud Account?")),
        Some(t("The account credential will be removed from the password keyring. Your local NextCloud folder and all files inside it will remain untouched.")),
    );
    dialog.add_response("cancel", t("Cancel"));
    dialog.add_response("remove", t("Continue"));
    dialog.set_response_appearance("remove", libadwaita::ResponseAppearance::Destructive);

    let login_name = login_name.to_string();
    let window_for_response = window.clone();
    let callbacks_for_response = callbacks.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response == "remove" {
            present_remove_account_step_two(
                &login_name,
                &window_for_response,
                &callbacks_for_response,
            );
        }
    });
    dialog.present(Some(window));
}

/// Second step: the user must type “remove” to proceed.
fn present_remove_account_step_two(
    login_name: &str,
    window: &libadwaita::PreferencesWindow,
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
    dialog.add_response("remove", t("Remove Account"));
    dialog.set_response_appearance("remove", libadwaita::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));

    let window_for_response = window.clone();
    let callbacks_for_response = callbacks.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response != "remove" {
            return;
        }
        if !typed_confirmation_matches(&entry.text()) {
            let toast = libadwaita::Toast::new("Type “remove” to confirm account removal.");
            window_for_response.add_toast(toast);
            return;
        }
        invoke(&callbacks_for_response.on_remove_account);
    });
    dialog.present(Some(window));
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
    fn folder_subtitle_uses_root_for_empty_remote() {
        assert_eq!(folder_subtitle(""), "/");
        assert_eq!(folder_subtitle("/Documents"), "/Documents");
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
            // locale on the GTK worker thread so the title assertion is
            // deterministic, then verify the window actually localizes.
            set_locale(Locale::English);
            let dir = tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));
            let account = sample_account();
            let account_id = store.add_account(&account).unwrap();
            let window = SettingsWindow::new(
                store.clone(),
                account.clone(),
                account_id.clone(),
                SettingsCallbacks::default(),
            );
            assert_eq!(
                window.window().title().unwrap_or_default().to_string(),
                "Settings"
            );

            set_locale(Locale::Spanish);
            let window =
                SettingsWindow::new(store, account, account_id, SettingsCallbacks::default());
            assert_eq!(
                window.window().title().unwrap_or_default().to_string(),
                "Configuración"
            );
            reset_locale();
        });
    }
}
