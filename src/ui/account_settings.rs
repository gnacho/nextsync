//! Per-account settings panel shown in the main window, below the
//! synchronized folders (issue #56).
//!
//! The account owns its server, proxy/TLS trust, synchronization options,
//! deletion guard, detailed output and credentials. Global settings
//! (startup, notifications, quiet hours, Wi-Fi allowlist, transfer impact,
//! logging, backup) stay in the Preferences view.

use libadwaita::prelude::*;

use crate::storage::config::{AccountConfig, ConfigStore};
use crate::ui::settings::{
    account_action_groups, deletion_guard_group, detailed_output_row, save_account_network,
    sync_option_groups, SettingsCallbacks, SettingsHost,
};
use crate::util::i18n::t;

/// Build the account settings panel widget (a vertical box of preference
/// groups). The caller toggles its visibility below the folder list.
pub fn build_account_settings_panel(
    store: &ConfigStore,
    account: &AccountConfig,
    account_id: &str,
    callbacks: &SettingsCallbacks,
    host: &SettingsHost,
) -> gtk4::Box {
    let box_container = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    // Server (read-only).
    let server_group = libadwaita::PreferencesGroup::builder()
        .title(t("Server"))
        .build();
    server_group.add(
        &libadwaita::ActionRow::builder()
            .title(account.server_url.as_str())
            .subtitle(account.login_name.as_str())
            .build(),
    );
    box_container.append(&server_group);

    // Connection: per-account proxy + TLS trust (issue #56).
    let connection = libadwaita::PreferencesGroup::builder()
        .title(t("Connection"))
        .build();
    let proxy = libadwaita::EntryRow::new();
    proxy.set_title(t("Custom HTTP proxy"));
    proxy.set_text(account.custom_proxy.as_deref().unwrap_or(""));
    proxy.set_show_apply_button(true);
    proxy.set_tooltip_text(Some(t("Save the custom HTTP proxy")));
    let trust = libadwaita::SwitchRow::builder()
        .title(t("Allow invalid or self-signed certificates"))
        .subtitle(t(
            "This weakens connection security. Enable only for a server you trust.",
        ))
        .active(account.trust_invalid_certificates)
        .build();
    {
        let store = store.clone();
        let account_id = account_id.to_string();
        let callbacks = callbacks.clone();
        let proxy_guard = proxy.clone();
        let trust_guard = trust.clone();
        proxy.connect_apply({
            let store = store.clone();
            let account_id = account_id.clone();
            let callbacks = callbacks.clone();
            let proxy_guard = proxy_guard.clone();
            let trust_guard = trust_guard.clone();
            move |_| {
                save_account_network(&store, &account_id, &proxy_guard, &trust_guard, &callbacks)
            }
        });
        trust.connect_active_notify(move |_| {
            save_account_network(&store, &account_id, &proxy_guard, &trust_guard, &callbacks);
        });
    }
    connection.add(&proxy);
    connection.add(&trust);
    box_container.append(&connection);

    // Synchronization options (account-owned).
    for group in sync_option_groups(store, account_id, account, callbacks, host) {
        box_container.append(&group);
    }

    // Detailed output (account-owned).
    let detailed_group = libadwaita::PreferencesGroup::new();
    detailed_group.add(&detailed_output_row(store, account_id, callbacks, account));
    box_container.append(&detailed_group);

    // Deletion guard (account-owned).
    box_container.append(&deletion_guard_group(store, account_id, account));

    // Authentication + account removal (account-owned).
    for group in account_action_groups(store, account_id, account, callbacks, host) {
        box_container.append(&group);
    }

    box_container
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{reset_locale, set_locale, Locale};

    fn sample_account() -> AccountConfig {
        AccountConfig {
            server_url: "https://cloud.example.com".to_string(),
            login_name: "alice".to_string(),
            ..AccountConfig::default()
        }
    }

    #[test]
    fn panel_contains_the_account_server_and_connection_groups() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let dir = tempfile::tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));
            let account = sample_account();
            let panel = build_account_settings_panel(
                &store,
                &account,
                "account-id",
                &crate::ui::settings::SettingsCallbacks::default(),
                &crate::ui::settings::SettingsHost::new(
                    &gtk4::Window::new(),
                    &libadwaita::ToastOverlay::new(),
                ),
            );
            // The panel hosts the server + connection groups plus the shared
            // account option groups; it builds without panicking.
            let children = panel.observe_children();
            let count = children.n_items();
            assert!(
                count >= 2,
                "expected at least the server and connection groups, got {count}"
            );
            reset_locale();
        });
    }
}
