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

/// Build the account settings panel widget. The caller toggles its
/// visibility below the folder list.
///
/// The preference groups need a [`libadwaita::PreferencesPage`] container to
/// render with the boxed-list styling; a bare box of groups looks broken, so
/// the page is wrapped in a scroll window here.
pub fn build_account_settings_panel(
    store: &ConfigStore,
    account: &AccountConfig,
    account_id: &str,
    callbacks: &SettingsCallbacks,
    host: &SettingsHost,
) -> gtk4::Box {
    let page = libadwaita::PreferencesPage::new();
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

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
    page.add(&server_group);

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
    page.add(&connection);

    // Synchronization options (account-owned).
    for group in sync_option_groups(store, account_id, account, callbacks, host) {
        page.add(&group);
    }

    // Detailed output (account-owned).
    let detailed_group = libadwaita::PreferencesGroup::new();
    detailed_group.add(&detailed_output_row(store, account_id, callbacks, account));
    page.add(&detailed_group);

    // Deletion guard (account-owned).
    page.add(&deletion_guard_group(store, account_id, account));

    // Authentication + account removal (account-owned).
    for group in account_action_groups(store, account_id, account, callbacks, host) {
        page.add(&group);
    }

    // Scrollable wrapper so the panel works in any window height.
    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroller.set_child(Some(&page));
    scroller.set_vexpand(true);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&scroller);
    root
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
            // The panel is a scrollable wrapper around a PreferencesPage
            // holding the server and connection groups; it must build
            // without panicking and expose a real page.
            let children = panel.observe_children();
            assert_eq!(children.n_items(), 1, "one scroll window wrapper");
            let child = children.item(0).unwrap();
            let scroller = child
                .downcast::<gtk4::ScrolledWindow>()
                .expect("the wrapper is a ScrolledWindow");
            assert!(
                scroller.child().is_some(),
                "the scroller hosts the PreferencesPage"
            );
            reset_locale();
        });
    }
}
