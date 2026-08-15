//! Pending changes dialog (issue #46).
//!
//! Per-folder preview of what a synchronization would do locally: the
//! current tree compared against the journal of the last successful run
//! (the delete-guard manifest). The scan runs off the UI thread; only the
//! bounded, already-parsed rows cross back to the main loop.
//!
//! Limitation (by design): remote changes are not included. The engine
//! owns the remote discovery; this view is a cheap local-only diff.

use libadwaita::prelude::*;

use crate::core::pending_changes::{
    bounded_rows, pending_for_folder, PendingChanges, PENDING_LIST_CAP,
};
use crate::storage::config::{AccountConfig, FolderConfig};
use crate::util::i18n::t;

/// Label of one bounded row (pure; testable).
pub fn pending_row_title(kind: &str, path: &str) -> String {
    let marker = match kind {
        "new" => t("New"),
        "changed" => t("Changed"),
        _ => t("Deleted"),
    };
    format!("{marker} · {path}")
}

/// Dialog body explaining what the list is (and is not).
pub fn pending_body(had_journal: bool) -> String {
    if had_journal {
        t("Local changes since the last synchronization, compared against the local tree and the synchronization journal. Remote changes are not included.").to_string()
    } else {
        t("This folder has no synchronization journal yet, so every local file counts as new. Remote changes are not included.").to_string()
    }
}

/// Present the pending-changes dialog for one folder.
pub fn present_pending_changes(
    account: &AccountConfig,
    folder: &FolderConfig,
    parent: &gtk4::Widget,
) {
    let account = account.clone();
    let folder = folder.clone();
    let compute = gio::spawn_blocking(move || pending_for_folder(&account, &folder));
    let parent = parent.clone();
    glib::spawn_future_local(async move {
        let Ok((changes, had_journal)) = compute.await else {
            return;
        };
        present_changes(&changes, had_journal, &parent);
    });
}

fn present_changes(changes: &PendingChanges, had_journal: bool, parent: &gtk4::Widget) {
    let list = gtk4::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    if changes.is_empty() {
        let row = libadwaita::ActionRow::builder()
            .title(t(
                "No pending local changes since the last synchronization.",
            ))
            .build();
        list.append(&row);
    } else {
        let (rows, _remaining) = bounded_rows(changes, PENDING_LIST_CAP);
        let shown = rows.len();
        for (kind, path) in rows {
            let row = libadwaita::ActionRow::builder()
                .title(pending_row_title(kind, path))
                .title_lines(1)
                .build();
            let icon = gtk4::Image::builder()
                .icon_name(match kind {
                    "new" => "list-add-symbolic",
                    "changed" => "document-edit-symbolic",
                    _ => "user-trash-symbolic",
                })
                .pixel_size(16)
                .build();
            row.add_prefix(&icon);
            list.append(&row);
        }
        if let Some(line) = crate::ui::server_trash::remaining_line(
            changes.created.len() + changes.modified.len() + changes.deleted.len(),
            shown,
        ) {
            let row = libadwaita::ActionRow::builder().title(line).build();
            list.append(&row);
        }
    }

    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .max_content_height(360)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    let dialog =
        libadwaita::AlertDialog::new(Some(t("Pending Changes")), Some(&pending_body(had_journal)));
    dialog.set_extra_child(Some(&scrolled));
    dialog.add_response("close", t("Close"));
    dialog.set_default_response(Some("close"));
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_titles_carry_the_kind_marker() {
        crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
        assert_eq!(pending_row_title("new", "a.txt"), "New · a.txt");
        assert_eq!(pending_row_title("changed", "b.txt"), "Changed · b.txt");
        assert_eq!(pending_row_title("deleted", "c.txt"), "Deleted · c.txt");
        crate::util::i18n::reset_locale();
    }

    #[test]
    fn body_explains_the_missing_journal() {
        crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
        assert!(pending_body(true).contains("journal"));
        assert!(pending_body(false).contains("no synchronization journal"));
        crate::util::i18n::reset_locale();
    }

    #[test]
    fn dialog_constructs_with_and_without_changes() {
        crate::ui::test_helpers::gtk_smoke(|| {
            crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
            let window = gtk4::Window::new();
            let mut changes = PendingChanges::default();
            changes.created.push("a.txt".to_string());
            changes.deleted.push("b.txt".to_string());
            present_changes(&changes, true, window.upcast_ref::<gtk4::Widget>());
            present_changes(
                &PendingChanges::default(),
                false,
                window.upcast_ref::<gtk4::Widget>(),
            );
            crate::util::i18n::reset_locale();
        });
    }
}
