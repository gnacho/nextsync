//! Server trash dialog (issue #38).
//!
//! The deletion review can offer a real restore from the Nextcloud
//! trashbin: the trash items are listed (name, original location and
//! deletion date) and a Restore All issues the WebDAV `MOVE`s that put
//! everything back where it came from. OpenCloud has no documented
//! trashbin endpoint, so the entry point stays hidden for those accounts.

use libadwaita::prelude::*;

use crate::nextcloud::api::{NextcloudApi, TrashItem};
use crate::nextcloud::credentials::CredentialsStore;
use crate::storage::config::AccountConfig;
use crate::util::i18n::t;

/// Cap of trash rows rendered in the dialog; bigger trashbins show a
/// trailing "and N more" line (bounded like the pending-changes view).
pub const TRASH_LIST_CAP: usize = 50;

/// Title of one trash row: the original location when known, else the name.
pub fn trash_row_title(item: &TrashItem) -> String {
    match &item.original_location {
        Some(location) if !location.is_empty() => location.clone(),
        _ => item.filename.clone(),
    }
}

/// Subtitle of one trash row: folder marker, trash name and deletion date.
pub fn trash_row_subtitle(item: &TrashItem) -> String {
    let kind = if item.is_collection {
        t("Folder")
    } else {
        t("File")
    };
    let mut subtitle = format!("{kind} · {}", item.filename);
    if let Some(deletion_time) = item.deletion_time {
        subtitle.push_str(" · ");
        subtitle.push_str(&format_deletion_time(deletion_time));
    }
    subtitle
}

/// Localized date of a unix timestamp (empty when out of range).
pub fn format_deletion_time(unix_seconds: i64) -> String {
    glib::DateTime::from_unix_local(unix_seconds)
        .ok()
        .and_then(|datetime| datetime.format("%x %H:%M").ok())
        .map(|text| text.to_string())
        .unwrap_or_default()
}

/// The "and N more" suffix line for a list longer than the cap.
pub fn remaining_line(total: usize, shown: usize) -> Option<String> {
    let remaining = total.saturating_sub(shown);
    if remaining == 0 {
        return None;
    }
    Some(t("and {count} more…").replacen("{count}", &remaining.to_string(), 1))
}

/// Present the trash listing and the Restore All action.
///
/// The fetch runs off the UI thread (`gio::spawn_blocking`); only the
/// already-parsed items cross back to the main loop. Restoring re-issues
/// one `MOVE` per item sequentially off the UI thread and reports the
/// outcome in a closing dialog (failures keep the items in the trash, so
/// they can be retried).
pub fn present_server_trash(account: &AccountConfig, parent: &gtk4::Widget) {
    let account_for_probe = account.clone();
    let handle = gio::spawn_blocking(move || -> Result<Vec<TrashItem>, String> {
        let password = CredentialsStore::get_for_account(
            &account_for_probe.id,
            &account_for_probe.server_url,
            &account_for_probe.login_name,
        )
        .ok()
        .flatten()
        .ok_or_else(|| t("No saved credentials").to_string())?;
        NextcloudApi::new()
            .list_trash(
                &account_for_probe.server_url,
                &account_for_probe.login_name,
                &password,
            )
            .map_err(|error| error.to_string())
    });
    let parent = parent.clone();
    let account_for_list = account.clone();
    glib::spawn_future_local(async move {
        let items = match handle.await {
            Ok(Ok(items)) => items,
            Ok(Err(message)) => {
                let dialog = libadwaita::AlertDialog::new(Some(t("Server Trash")), Some(&message));
                dialog.add_response("close", t("Close"));
                dialog.present(Some(&parent));
                return;
            }
            Err(_) => return,
        };
        present_trash_list(&items, &account_for_list, &parent);
    });
}

/// Show the (already fetched) trash items with the Restore All action.
fn present_trash_list(items: &[TrashItem], account: &AccountConfig, parent: &gtk4::Widget) {
    let shown: Vec<TrashItem> = items.iter().take(TRASH_LIST_CAP).cloned().collect();
    let list = gtk4::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    if items.is_empty() {
        let row = libadwaita::ActionRow::builder()
            .title(t("No restorable files were found in the server trash."))
            .build();
        list.append(&row);
    }
    for item in &shown {
        let row = libadwaita::ActionRow::builder()
            .title(trash_row_title(item))
            .subtitle(trash_row_subtitle(item))
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        let icon = gtk4::Image::builder()
            .icon_name(if item.is_collection {
                "folder-symbolic"
            } else {
                "text-x-generic-symbolic"
            })
            .pixel_size(16)
            .build();
        row.add_prefix(&icon);
        list.append(&row);
    }
    if let Some(line) = remaining_line(items.len(), shown.len()) {
        let row = libadwaita::ActionRow::builder().title(line).build();
        list.append(&row);
    }

    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .max_content_height(360)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    // The trashbin retention window is server-side (commonly 30 days);
    // the dates above tell what is still restorable.
    let heading = t("Deleted files can be restored to their original location. The retention window depends on the server settings.");

    let dialog = libadwaita::AlertDialog::new(Some(t("Server Trash")), Some(heading));
    dialog.set_extra_child(Some(&scrolled));
    dialog.add_response("close", t("Close"));
    dialog.add_response("restore", t("Restore All"));
    dialog.set_response_appearance("restore", libadwaita::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("close"));

    let restorable: Vec<TrashItem> = items.to_vec();
    let account = account.clone();
    let parent_for_response = parent.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response != "restore" {
            return;
        }
        dialog.force_close();
        restore_all(restorable.clone(), &account, &parent_for_response);
    });
    dialog.present(Some(parent));
}

/// Restore every item sequentially off the UI thread, then report.
fn restore_all(items: Vec<TrashItem>, account: &AccountConfig, parent: &gtk4::Widget) {
    // The account travels as plain data; the password is resolved inside
    // the blocking closure and never leaves it.
    let parent = parent.clone();
    let account = account.clone();
    let restore = gio::spawn_blocking(move || -> Option<(String, String)> {
        let password = CredentialsStore::get_for_account(
            &account.id,
            &account.server_url,
            &account.login_name,
        )
        .ok()
        .flatten()?;
        let api = NextcloudApi::new();
        let mut restored = 0usize;
        for item in &items {
            if api
                .restore_trash_item(
                    &account.server_url,
                    &account.login_name,
                    &password,
                    &item.filename,
                )
                .is_ok()
            {
                restored += 1;
            }
        }
        Some((
            t("{count} of {total} items were restored from the server trash.")
                .replacen("{count}", &restored.to_string(), 1)
                .replacen("{total}", &items.len().to_string(), 1),
            t("Items that could not be restored stay in the server trash.").to_string(),
        ))
    });
    glib::spawn_future_local(async move {
        let Some((title, detail)) = restore.await.ok().flatten() else {
            return;
        };
        let dialog = libadwaita::AlertDialog::new(Some(&title), Some(&detail));
        dialog.add_response("close", t("Close"));
        dialog.present(Some(&parent));
    });
}

/// Pure part of the wiring (kept public for tests): whether the trash
/// entry point applies to a provider.
pub fn trash_supported(account: &AccountConfig) -> bool {
    account.provider == crate::nextcloud::driver::Provider::Nextcloud
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nextcloud::api::TrashItem;
    use crate::nextcloud::driver::Provider;
    use crate::storage::config::AccountConfig;

    fn item(filename: &str, original: Option<&str>, deleted: Option<i64>) -> TrashItem {
        TrashItem {
            filename: filename.to_string(),
            original_location: original.map(str::to_string),
            deletion_time: deleted,
            is_collection: false,
        }
    }

    #[test]
    fn row_title_prefers_the_original_location() {
        assert_eq!(
            trash_row_title(&item("a.txt.d1678", Some("Documents/a.txt"), None)),
            "Documents/a.txt"
        );
        assert_eq!(
            trash_row_title(&item("b.txt.d1678", None, None)),
            "b.txt.d1678"
        );
        assert_eq!(
            trash_row_title(&item("c.txt.d1678", Some(""), None)),
            "c.txt.d1678"
        );
    }

    #[test]
    fn row_subtitle_carries_kind_name_and_date() {
        crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
        let mut folder = item("dir.d1678", Some("Photos/dir"), Some(1700000000));
        folder.is_collection = true;
        let subtitle = trash_row_subtitle(&folder);
        assert!(
            subtitle.starts_with("Folder · dir.d1678 · "),
            "was: {subtitle}"
        );
        let plain = trash_row_subtitle(&item("b.txt.d1678", None, None));
        assert_eq!(plain, "File · b.txt.d1678");
        crate::util::i18n::reset_locale();
    }

    #[test]
    fn remaining_line_counts_past_the_cap() {
        crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
        assert_eq!(remaining_line(50, 50), None);
        assert_eq!(remaining_line(3, 2).unwrap(), "and 1 more…");
        assert_eq!(remaining_line(120, 50).unwrap(), "and 70 more…");
        crate::util::i18n::reset_locale();
    }

    #[test]
    fn deletion_time_formats_or_stays_empty() {
        assert!(!format_deletion_time(1_700_000_000).is_empty());
        assert_eq!(format_deletion_time(i64::MAX), "");
    }

    #[test]
    fn trash_entry_point_is_nextcloud_only() {
        let nextcloud = AccountConfig {
            provider: Provider::Nextcloud,
            ..AccountConfig::default()
        };
        assert!(trash_supported(&nextcloud));
        let opencloud = AccountConfig {
            provider: Provider::OpenCloud,
            ..AccountConfig::default()
        };
        assert!(!trash_supported(&opencloud));
    }

    #[test]
    fn trash_dialog_constructs() {
        crate::ui::test_helpers::gtk_smoke(|| {
            crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
            let window = gtk4::Window::new();
            let items = vec![
                item("a.txt.d1678", Some("Documents/a.txt"), Some(1_700_000_000)),
                item("b.txt.d1678", None, None),
            ];
            let account = AccountConfig::default();
            present_trash_list(&items, &account, window.upcast_ref::<gtk4::Widget>());
            crate::util::i18n::reset_locale();
        });
    }
}
