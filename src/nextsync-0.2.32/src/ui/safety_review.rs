//! Blocking first-sync safety review (issue #35).
//!
//! One shared dialog for the two places a folder enters the app (the setup
//! wizard and the Add Folder dialog). It presents the base first-sync
//! wording plus the warnings computed from [`FirstSyncFacts`]: a merge of
//! two populated trees and/or a folder that was synchronized before. The
//! user decides between starting fresh (old hidden journals go to the
//! trash) or keeping the previous synchronization history.

use std::rc::Rc;

use libadwaita::prelude::*;

use crate::core::sync_safety::{trash_stale_artifacts, FirstSyncFacts, FirstSyncWarning};
use crate::util::i18n::t;

/// What the user chose to do with the previous sync journals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshStart {
    /// Move the stale engine artifacts to the trash and re-download.
    Yes,
    /// Keep the journals and resume the previous sync history.
    No,
}

/// Compose the extra body sections the facts demand (pure; testable).
///
/// The base first-sync wording (conflict copies etc.) comes from the
/// caller; these paragraphs state the blocking facts of this review.
/// `threshold_bytes` is the folder-size confirmation threshold (issue #36)
/// and `size_target` the local folder the download would land in.
pub fn review_sections(
    facts: &FirstSyncFacts,
    threshold_bytes: Option<u64>,
    size_target: &str,
) -> Vec<String> {
    let mut sections = Vec::new();
    for warning in crate::core::sync_safety::first_sync_warnings(facts, threshold_bytes) {
        match warning {
            FirstSyncWarning::Merge => sections.push(
                t("Both the local folder and the remote folder contain files. Confirm that you want to merge their contents.")
                    .to_string(),
            ),
            FirstSyncWarning::PreviouslySynced => {
                // One line is enough (issue #74): the Keep/Start Fresh
                // buttons below carry the decision and its consequences.
                sections.push(t("This folder was synchronized before.").to_string());
            }
            FirstSyncWarning::Oversized => {
                let Some(size) = facts.remote_size else {
                    continue;
                };
                let size = crate::nextcloud::api::format_bytes(size);
                sections.push(
                    t("The remote folder holds about {size}. Its files will be downloaded into {target}.")
                        .replacen("{size}", &size, 1)
                        .replacen("{target}", size_target, 1),
                );
            }
        }
    }
    sections
}

/// Presentation parameters of the blocking first-sync review.
pub struct FirstSyncReview<'a> {
    /// Dialog title.
    pub title: &'a str,
    /// Standard first-sync wording shown before the warning sections.
    pub base_body: &'a str,
    /// The collected facts (merge, journals, remote size).
    pub facts: &'a FirstSyncFacts,
    /// Folder-size confirmation threshold in bytes (issue #36).
    pub threshold_bytes: Option<u64>,
    /// Local folder a confirmed download would land in.
    pub size_target: &'a str,
    /// Label of the cancel response ("Back to setup" in the wizard).
    pub cancel_label: &'a str,
}

/// Present the blocking review and invoke `on_decision` with the choice.
///
/// The dialog is modal by nature: nothing happens until a response is
/// chosen; `on_cancel` runs when the cancel response is picked.
pub fn present_first_sync_review(
    parent: &gtk4::Widget,
    review: FirstSyncReview<'_>,
    on_decision: Rc<dyn Fn(FreshStart)>,
    on_cancel: Rc<dyn Fn()>,
) {
    let FirstSyncReview {
        title,
        base_body,
        facts,
        threshold_bytes,
        size_target,
        cancel_label,
    } = review;
    let mut body = base_body.to_string();
    for section in review_sections(facts, threshold_bytes, size_target) {
        body.push_str("\n\n");
        body.push_str(&section);
    }
    let previously_synced = crate::core::sync_safety::first_sync_warnings(facts, threshold_bytes)
        .contains(&FirstSyncWarning::PreviouslySynced);

    let dialog = libadwaita::AlertDialog::new(Some(title), Some(&body));
    dialog.add_response("cancel", cancel_label);
    if previously_synced {
        dialog.add_response("keep", t("Keep Synchronization History"));
        dialog.add_response("fresh", t("Start Fresh"));
        dialog.set_response_appearance("fresh", libadwaita::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("fresh"));
    } else {
        dialog.add_response("start", t("Start"));
        dialog.set_response_appearance("start", libadwaita::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("start"));
    }
    dialog.present(Some(parent));
    let on_decision = on_decision.clone();
    dialog.connect_response(None, move |_dialog, response| match response {
        "fresh" => on_decision(FreshStart::Yes),
        "keep" | "start" => on_decision(FreshStart::No),
        _ => on_cancel(),
    });
}

/// Apply a fresh-start decision to one local root (issue #35, point 4):
/// move the stale hidden artifacts to the system trash.
pub fn apply_fresh_start(local_root: &str, decision: FreshStart) {
    if decision == FreshStart::Yes {
        let root = crate::storage::config::expanduser(local_root);
        trash_stale_artifacts(&root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{reset_locale, set_locale, Locale};

    fn facts(journals: &[&str], local_empty: bool, remote_empty: Option<bool>) -> FirstSyncFacts {
        FirstSyncFacts {
            local_empty,
            remote_empty,
            remote_size: None,
            journal_names: journals.iter().map(|name| name.to_string()).collect(),
        }
    }

    #[test]
    fn merge_facts_add_the_merge_section() {
        set_locale(Locale::English);
        let sections = review_sections(&facts(&[], false, Some(false)), None, "/tmp/nc");
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("merge"));
        reset_locale();
    }

    #[test]
    fn journal_facts_say_it_in_one_line() {
        set_locale(Locale::English);
        let sections = review_sections(
            &facts(&[".sync_1.db", ".sync_2.db"], true, Some(true)),
            None,
            "/tmp/nc",
        );
        assert_eq!(sections.len(), 1);
        // One short line (issue #74); the buttons carry the decision, so the
        // journal names and the trash explanation stay out of the body.
        assert_eq!(sections[0], "This folder was synchronized before.");
        reset_locale();
    }

    #[test]
    fn oversized_facts_add_the_size_section() {
        set_locale(Locale::English);
        let oversized = FirstSyncFacts {
            local_empty: true,
            remote_empty: Some(false),
            remote_size: Some(600 * 1024 * 1024),
            journal_names: Vec::new(),
        };
        let sections = review_sections(&oversized, Some(500 * 1024 * 1024), "/tmp/nc");
        assert_eq!(sections.len(), 1);
        assert!(
            sections[0].contains("600.0 MiB"),
            "section was: {sections:?}"
        );
        assert!(sections[0].contains("/tmp/nc"));
        // Below the threshold there is nothing to say.
        assert!(review_sections(&oversized, Some(1024 * 1024 * 1024), "/tmp/nc").is_empty());
        reset_locale();
    }

    #[test]
    fn both_warnings_stack_in_order() {
        set_locale(Locale::English);
        let sections =
            review_sections(&facts(&[".sync_1.db"], false, Some(false)), None, "/tmp/nc");
        assert_eq!(sections.len(), 2);
        assert!(sections[0].contains("merge"));
        assert_eq!(sections[1], "This folder was synchronized before.");
        reset_locale();
    }

    #[test]
    fn quiet_facts_have_no_sections() {
        set_locale(Locale::English);
        assert!(review_sections(&facts(&[], true, Some(true)), None, "/tmp/nc").is_empty());
        assert!(review_sections(&facts(&[], true, None), None, "/tmp/nc").is_empty());
        reset_locale();
    }

    #[test]
    fn fresh_start_decision_drives_the_trash() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".sync_1.db"), "x").expect("write");
        std::fs::write(dir.path().join("keep.txt"), "x").expect("write");
        let root = dir.path().to_string_lossy().into_owned();
        // Keeping the history leaves everything in place.
        apply_fresh_start(&root, FreshStart::No);
        assert!(dir.path().join(".sync_1.db").exists());
    }

    #[test]
    fn review_dialog_constructs_for_both_warnings() {
        crate::ui::test_helpers::gtk_smoke(|| {
            crate::util::i18n::set_locale(crate::util::i18n::Locale::English);
            let window = gtk4::Window::new();
            let facts = FirstSyncFacts {
                local_empty: false,
                remote_empty: Some(false),
                remote_size: Some(600 * 1024 * 1024),
                journal_names: vec![".sync_1.db".to_string()],
            };
            present_first_sync_review(
                window.upcast_ref::<gtk4::Widget>(),
                FirstSyncReview {
                    title: "Start Synchronizing?",
                    base_body: "base wording",
                    facts: &facts,
                    threshold_bytes: Some(500 * 1024 * 1024),
                    size_target: "/tmp/nc",
                    cancel_label: "Back to setup",
                },
                Rc::new(|_| {}),
                Rc::new(|| {}),
            );
            crate::util::i18n::reset_locale();
        });
    }
}
