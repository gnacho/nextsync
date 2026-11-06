//! Restore reviewed paths from local conflicted copies (issue #302).
//!
//! A file the server already dropped often survives on disk as the engine's
//! conflicted copy (`<name> (Nextcloud conflicted copy <date>)<ext>`). The
//! review offers this answer when at least one listed path has such a copy:
//! the newest copy is promoted to the original name ([`keep_remote`]) and the
//! folder is rescanned, so the restored content goes back to the server on
//! the next sync.
//!
//! Only local files are used: nothing is fetched from the Nextcloud versions
//! API (out of scope of the issue).

use libadwaita::prelude::*;

use crate::core::conflict_files::{keep_remote, ConflictFile};
use crate::core::scheduler::Scheduler;
use crate::util::i18n::{plural, t};

/// Promote every copy, off the UI thread, then rescan the folder so the
/// restored files leave again for the server. Reports how many made it
/// instead of claiming a blanket success.
pub(crate) fn present_restore_copies(
    copies: Vec<ConflictFile>,
    scheduler: Scheduler,
    window: &libadwaita::ApplicationWindow,
) {
    if copies.is_empty() {
        return;
    }
    let total = copies.len();
    let handle = gio::spawn_blocking(move || {
        let mut restored = 0usize;
        for conflict in &copies {
            if keep_remote(conflict) {
                restored += 1;
            } else {
                eprintln!(
                    "restore: could not promote {} to {}",
                    conflict.path.display(),
                    conflict.original_path.display()
                );
            }
        }
        restored
    });
    let window = window.clone();
    glib::spawn_future_local(async move {
        let Ok(restored) = handle.await else {
            return;
        };
        if restored > 0 {
            // Clears the alert and asks for a run: the restored file is new
            // locally, so the engine uploads it to the path the server lost.
            scheduler.restore_from_server();
        }
        let body = report(restored, total);
        let dialog = libadwaita::AlertDialog::new(Some(t("Restored local copies")), Some(&body));
        dialog.add_response("close", t("Close"));
        dialog.set_default_response(Some("close"));
        crate::ui::main_window::MainWindow::present_modal_dialog(&dialog, &window);
    });
}

/// What to say after the promotion attempt: the restored count, and the
/// failures when the totals do not match.
fn report(restored: usize, total: usize) -> String {
    let mut body = if restored == 0 {
        t("No file could be restored from its local conflicted copy.").to_string()
    } else {
        plural(
            restored as u64,
            t("1 file was restored from a local conflicted copy and will be uploaded on the next sync."),
            t("{count} files were restored from local conflicted copies and will be uploaded on the next sync."),
        )
        .replacen("{count}", &restored.to_string(), 1)
    };
    if restored > 0 && restored < total {
        let failed = total - restored;
        body.push(' ');
        body.push_str(
            &plural(
                failed as u64,
                t("1 copy could not be restored."),
                t("{count} copies could not be restored."),
            )
            .replacen("{count}", &failed.to_string(), 1),
        );
    }
    body
}

/// Label of the review answer, with the count of files it would restore.
pub(crate) fn restore_response_label(count: usize) -> String {
    plural(
        count as u64,
        t("Restore 1 file from its local conflicted copy"),
        t("Restore {count} files from local conflicted copies"),
    )
    .replacen("{count}", &count.to_string(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{set_locale, Locale};

    fn english() {
        set_locale(Locale::English);
    }

    #[test]
    fn report_counts_restored_files() {
        english();
        assert_eq!(
            report(1, 1),
            "1 file was restored from a local conflicted copy and will be uploaded on the next sync."
        );
        assert_eq!(
            report(3, 3),
            "3 files were restored from local conflicted copies and will be uploaded on the next sync."
        );
    }

    #[test]
    fn report_names_the_failures() {
        english();
        assert_eq!(
            report(2, 3),
            "2 files were restored from local conflicted copies and will be uploaded on the next sync. 1 copy could not be restored."
        );
        assert_eq!(
            report(0, 2),
            "No file could be restored from its local conflicted copy."
        );
    }

    #[test]
    fn response_label_is_plural() {
        english();
        assert_eq!(
            restore_response_label(1),
            "Restore 1 file from its local conflicted copy"
        );
        assert_eq!(
            restore_response_label(4),
            "Restore 4 files from local conflicted copies"
        );
    }
}
