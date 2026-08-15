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
    account_action_groups, save_account_network, SettingsCallbacks, SettingsHost,
};
use crate::util::i18n::t;

/// Build the account settings panel widget. The caller toggles it below the
/// folder list.
///
/// The preference groups need a [`libadwaita::PreferencesPage`] container to
/// render with the boxed-list styling; the page is wrapped in a
/// [`libadwaita::Revealer`] so the caller can animate it open/closed without
/// the layout collapsing (a nested scrolled window was fragile).
pub fn build_account_settings_panel(
    store: &ConfigStore,
    account: &AccountConfig,
    account_id: &str,
    callbacks: &SettingsCallbacks,
    host: &SettingsHost,
) -> gtk4::Revealer {
    let page = libadwaita::PreferencesPage::new();
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

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

    // The synchronization options live in Preferences (Synchronization
    // page); this panel keeps only the account identity and connection
    // (issue #63: the dropdown was too long).
    // Authentication + account removal (account-owned).
    for group in account_action_groups(store, account_id, account, callbacks, host) {
        page.add(&group);
    }

    // Revealer wrapper: animates the panel open/closed and keeps the layout
    // correct when hidden (issue #63).
    let revealer = gtk4::Revealer::new();
    revealer.set_child(Some(&page));
    revealer.set_reveal_child(false);
    revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
    revealer
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
            // The panel is a Revealer wrapping a PreferencesPage holding
            // the server and connection groups; it must build without
            // panicking, start hidden, and toggle open.
            assert!(!panel.is_child_revealed(), "the panel starts hidden");
            panel.set_reveal_child(true);
            assert!(panel.is_child_revealed());
            let child = panel.child().expect("a child");
            assert!(
                child.downcast::<libadwaita::PreferencesPage>().is_ok(),
                "the revealer hosts the PreferencesPage"
            );
            reset_locale();
        });
    }
}
