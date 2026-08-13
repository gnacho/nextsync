//! About dialog and update-check presentation (Fase 6.3).
//!
//! Mirrors the Python `ui/about.py` + `ui/update_window.py` (v0.4.0): a
//! libadwaita `AboutDialog` carrying the project metadata and a
//! "Check for Updates" link, plus the synchronous [`UpdateChecker`]
//! (Task 5.6) presented off the main thread with a "Checking…" spinner and a
//! three-state result dialog (up to date / update available / unavailable).
//!
//! Deviations from the Python (motivated):
//! - `gtk4::License::Gpl30` is used for the `GPL-3.0-or-later` declared in
//!   `Cargo.toml` (the Python used `GPL_3_0`, the same value). `Gpl30Only`
//!   would be "version 3 only" and would misrepresent the licence.
//! - The result is a single libadwaita `Dialog` (instead of a dedicated
//!   `ApplicationWindow`) so it follows the main window's presentation and
//!   lifecycle. Mandatory updates are out of scope here: the Rust rewrite has
//!   no mandatory gate yet, so every result is optional/non-blocking.
//! - `UpdateChecker` is constructed *inside* the blocking closure: the shared
//!   `HttpClient` trait is not `Send`, so the checker itself never crosses the
//!   thread boundary; only the installed version string (plain `String`) is
//!   captured.
//!
//! All user-visible strings go through [`crate::util::i18n::t`].

use libadwaita::prelude::*;

use crate::core::updates::{UpdateCheckResult, UpdateChecker, UpdateManifest, RELEASES_URL};
use crate::util::i18n::t;

/// Application id (AppStream/desktop).
pub const APPLICATION_ID: &str = "io.github.gnacho.nextsync";
/// Developer shown in the About dialog and the result dialog footer.
pub const DEVELOPER_NAME: &str = "gnacho";
/// Project website / source repository.
pub const WEBSITE_URL: &str = "https://github.com/gnacho/nextsync-rs";
/// Issue tracker, opened from the About dialog.
pub const ISSUES_URL: &str = "https://github.com/gnacho/nextsync-rs/issues";
/// Full CHANGELOG, opened from the About dialog.
pub const CHANGELOG_URL: &str = "https://github.com/gnacho/nextsync-rs/blob/main/CHANGELOG.md";
/// Synthetic URI the About dialog emits when "Check for Updates" is clicked;
/// the real network request is triggered from `activate-link`.
pub const CHECK_UPDATES_URI: &str = "nextsync://check-update";

/// Build the About dialog populated from the crate metadata.
///
/// The caller wires `activate-link` against its own `check_for_updates` so the
/// checker runs off the main thread; the dialog itself owns no async state.
pub fn build_about_dialog(version: &str) -> libadwaita::AboutDialog {
    let dialog = libadwaita::AboutDialog::builder()
        .application_name(t("NextSync"))
        .application_icon(APPLICATION_ID)
        .version(version)
        .developer_name(DEVELOPER_NAME)
        .comments(t(
            "A lightweight Nextcloud synchronization client for Linux.",
        ))
        .website(WEBSITE_URL)
        .issue_url(ISSUES_URL)
        .license_type(gtk4::License::Gpl30)
        .copyright("© 2026 gnacho")
        .build();
    dialog.add_link(t("Check for Updates"), CHECK_UPDATES_URI);
    dialog.add_link(t("Source code on GitHub"), WEBSITE_URL);
    dialog.add_link(t("Report a problem"), ISSUES_URL);
    dialog.add_link(t("Complete changelog"), CHANGELOG_URL);
    dialog.add_acknowledgement_section(
        Some(t("Synchronization Engine")),
        &["Nextcloud nextcloudcmd"],
    );
    dialog.add_acknowledgement_section(
        Some(t("Desktop Technologies")),
        &["GTK 4", "Libadwaita", "Secret Service", "D-Bus"],
    );
    dialog.add_acknowledgement_section(Some(t("Languages")), &["English", "Español"]);
    dialog
}

/// Build the transient "Checking for Updates" dialog with a spinner.
pub fn build_checking_dialog() -> libadwaita::Dialog {
    let spinner = gtk4::Spinner::builder().spinning(true).build();
    let label = gtk4::Label::builder()
        .label(t("Contacting the GitHub version service…"))
        .wrap(true)
        .max_width_chars(40)
        .build();
    let box_widget = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(16)
        .margin_top(28)
        .margin_bottom(28)
        .margin_start(28)
        .margin_end(28)
        .halign(gtk4::Align::Center)
        .build();
    box_widget.append(&spinner);
    box_widget.append(&label);

    let dialog = libadwaita::Dialog::builder()
        .title(t("Checking for Updates"))
        .content_width(360)
        .follows_content_size(true)
        .can_close(false)
        .build();
    dialog.set_child(Some(&box_widget));
    dialog
}

/// Run the synchronous update check off the main thread and return the result.
///
/// Exposed (and tested) as a plain function so the blocking body is decoupled
/// from the GTK presentation: the closure is what `gio::spawn_blocking` runs.
pub fn run_update_check(current_version: &str) -> UpdateCheckResult {
    UpdateChecker::new().check(current_version)
}

/// A presentation-oriented view of an [`UpdateCheckResult`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The installed version matches (or is newer than) the published one.
    UpToDate,
    /// A newer release was published.
    Available(UpdateManifest),
    /// The check failed (network, HTTP or manifest error). Carries the raw
    /// technical error for diagnostics; the user-facing wording is generic.
    Unavailable(String),
}

/// Collapse a raw [`UpdateCheckResult`] into the presentation enum. Pure: no
/// network, no GTK — unit tested in [`mod tests`].
pub fn classify_update_result(result: &UpdateCheckResult) -> UpdateOutcome {
    if let Some(error) = &result.error {
        return UpdateOutcome::Unavailable(error.clone());
    }
    match (&result.latest, result.update_available) {
        (Some(manifest), true) => UpdateOutcome::Available(manifest.clone()),
        (Some(_), false) => UpdateOutcome::UpToDate,
        (None, _) => UpdateOutcome::Unavailable(
            t("The version information could not be obtained. Check your connection and try again later.")
                .to_string(),
        ),
    }
}

/// Short heading shown as the title-2 of the result dialog.
pub fn result_heading(outcome: &UpdateOutcome) -> &'static str {
    match outcome {
        UpdateOutcome::UpToDate => t("NextSync Is Up to Date"),
        UpdateOutcome::Available(_) => t("A new version is available"),
        UpdateOutcome::Unavailable(_) => t("Could Not Check for Updates"),
    }
}

/// One-line explanation shown under the heading. For `Available` it conveys
/// the optional nature of the update; for `Unavailable` it gives actionable
/// advice (the raw transport error stays in the log).
pub fn result_explanation(outcome: &UpdateOutcome) -> &'static str {
    match outcome {
        UpdateOutcome::UpToDate => t("You are already using the latest available version."),
        UpdateOutcome::Available(_) => t(
            "This update is optional. NextSync can continue running while you decide when to install it.",
        ),
        UpdateOutcome::Unavailable(_) => t(
            "The version information could not be obtained. Check your connection and try again later.",
        ),
    }
}

/// Symbolic icon name for the result dialog.
pub fn result_icon_name(outcome: &UpdateOutcome) -> &'static str {
    match outcome {
        UpdateOutcome::UpToDate => "emblem-default-symbolic",
        UpdateOutcome::Available(_) => "software-update-available-symbolic",
        UpdateOutcome::Unavailable(_) => "dialog-warning-symbolic",
    }
}

/// Window title used by the result dialog.
pub fn result_title(outcome: &UpdateOutcome) -> &'static str {
    match outcome {
        UpdateOutcome::UpToDate => t("NextSync Is Up to Date"),
        UpdateOutcome::Available(_) => t("Update Available"),
        UpdateOutcome::Unavailable(_) => t("Could Not Check for Updates"),
    }
}

/// Build the result dialog. "Download New Version" (when an update exists)
/// opens [`RELEASES_URL`] in the browser; the secondary response closes.
pub fn build_update_result_dialog(
    outcome: &UpdateOutcome,
    installed_version: &str,
) -> libadwaita::Dialog {
    let toolbar = libadwaita::ToolbarView::builder().build();
    let header = gtk4::HeaderBar::new();
    toolbar.add_top_bar(&header);

    let dialog = libadwaita::Dialog::builder()
        .title(result_title(outcome))
        .content_width(480)
        .content_height(560)
        .build();
    dialog.set_title(result_title(outcome));

    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();
    let clamp = libadwaita::Clamp::builder()
        .maximum_size(520)
        .tightening_threshold(400)
        .build();
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(18)
        .margin_top(22)
        .margin_bottom(20)
        .margin_start(20)
        .margin_end(20)
        .build();
    clamp.set_child(Some(&content));
    scroller.set_child(Some(&clamp));
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));

    let hero = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(14)
        .halign(gtk4::Align::Center)
        .build();
    let icon = gtk4::Image::builder()
        .icon_name(result_icon_name(outcome))
        .pixel_size(56)
        .build();
    hero.append(&icon);
    let heading = gtk4::Label::builder()
        .label(result_heading(outcome))
        .css_classes(["title-2"])
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .build();
    hero.append(&heading);
    let explanation = gtk4::Label::builder()
        .label(result_explanation(outcome))
        .css_classes(["dim-label"])
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .xalign(0.5)
        .build();
    hero.append(&explanation);
    content.append(&hero);

    if let UpdateOutcome::Available(manifest) = outcome {
        content.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

        let version_group = libadwaita::PreferencesGroup::builder()
            .title(t("Version Information"))
            .build();
        version_group.add(
            &libadwaita::ActionRow::builder()
                .title(t("Installed version"))
                .subtitle(installed_version)
                .build(),
        );
        version_group.add(
            &libadwaita::ActionRow::builder()
                .title(t("Available version"))
                .subtitle(&manifest.version_text)
                .build(),
        );
        version_group.add(
            &libadwaita::ActionRow::builder()
                .title(t("Released at"))
                .subtitle(manifest.released_at_utc_text())
                .build(),
        );
        content.append(&version_group);

        let changes_group = libadwaita::PreferencesGroup::builder()
            .title(t("What's New"))
            .build();
        changes_group.add(
            &libadwaita::ActionRow::builder()
                .title(&manifest.summary)
                .build(),
        );

        let changelog_group = libadwaita::PreferencesGroup::builder().build();
        let count_text = t("{count} changes in this release")
            .replace("{count}", &manifest.changelog.len().to_string());
        let changelog = libadwaita::ExpanderRow::builder()
            .title(t("Full Changelog"))
            .subtitle(&count_text)
            .build();
        for (index, item) in manifest.changelog.iter().enumerate() {
            let row = libadwaita::ActionRow::builder().title(item).build();
            let badge = gtk4::Label::builder()
                .label(format!("{}", index + 1))
                .css_classes(["dim-label"])
                .build();
            row.add_prefix(&badge);
            changelog.add_row(&row);
        }
        changelog_group.add(&changelog);
        content.append(&changes_group);
        content.append(&changelog_group);
    }

    let footer = gtk4::Label::builder()
        .label(t("NextSync"))
        .css_classes(["dim-label", "caption"])
        .halign(gtk4::Align::Center)
        .build();
    content.append(&footer);

    // Responses in the header bar.
    let actions = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(10)
        .build();
    if matches!(outcome, UpdateOutcome::Available(_)) {
        let download = gtk4::Button::builder()
            .label(t("Download New Version"))
            .css_classes(["suggested-action"])
            .build();
        let releases_url = RELEASES_URL.to_string();
        download.connect_clicked(move |_| {
            let _ =
                gio::AppInfo::launch_default_for_uri(&releases_url, None::<&gio::AppLaunchContext>);
        });
        actions.append(&download);
    }

    let close = gtk4::Button::builder()
        .label(if matches!(outcome, UpdateOutcome::Available(_)) {
            t("Not Now")
        } else {
            t("Close")
        })
        .build();
    let close_dialog = dialog.clone();
    close.connect_clicked(move |_| {
        close_dialog.close();
    });
    actions.append(&close);
    header.pack_end(&actions);

    dialog
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::updates::{SemanticVersion, UpdateManifest};
    use crate::util::i18n::{reset_locale, set_locale, Locale};

    fn manifest(version: &str) -> UpdateManifest {
        UpdateManifest {
            version_text: version.to_string(),
            version: SemanticVersion::parse(version).unwrap(),
            mandatory: false,
            summary: "Corrections and improvements.".to_string(),
            changelog: vec![
                "Fixed one issue.".to_string(),
                "Improved one workflow.".to_string(),
            ],
            released_at: "2026-08-09T01:51:24Z".to_string(),
        }
    }

    #[test]
    fn error_result_classifies_as_unavailable_with_raw_message() {
        set_locale(Locale::English);
        let result = UpdateCheckResult {
            latest: None,
            update_available: false,
            error: Some("HTTP status 503".to_string()),
        };
        assert_eq!(
            classify_update_result(&result),
            UpdateOutcome::Unavailable("HTTP status 503".to_string())
        );
        reset_locale();
    }

    #[test]
    fn newer_manifest_classifies_as_available() {
        set_locale(Locale::English);
        let latest = manifest("9.9.9");
        let result = UpdateCheckResult {
            latest: Some(latest.clone()),
            update_available: true,
            error: None,
        };
        assert_eq!(
            classify_update_result(&result),
            UpdateOutcome::Available(latest)
        );
        reset_locale();
    }

    #[test]
    fn equal_or_older_manifest_classifies_as_up_to_date() {
        set_locale(Locale::English);
        let result = UpdateCheckResult {
            latest: Some(manifest("0.0.1")),
            update_available: false,
            error: None,
        };
        assert_eq!(classify_update_result(&result), UpdateOutcome::UpToDate);
        reset_locale();
    }

    #[test]
    fn missing_manifest_and_error_classifies_as_unavailable_with_generic_message() {
        set_locale(Locale::English);
        let result = UpdateCheckResult {
            latest: None,
            update_available: false,
            error: None,
        };
        let UpdateOutcome::Unavailable(message) = classify_update_result(&result) else {
            panic!("expected Unavailable");
        };
        assert!(message.contains("version information"));
        reset_locale();
    }

    #[test]
    fn result_wording_matches_each_state_in_english() {
        set_locale(Locale::English);
        let up_to_date = UpdateOutcome::UpToDate;
        assert_eq!(result_heading(&up_to_date), "NextSync Is Up to Date");
        assert_eq!(
            result_explanation(&up_to_date),
            "You are already using the latest available version."
        );
        assert_eq!(result_icon_name(&up_to_date), "emblem-default-symbolic");

        let available = UpdateOutcome::Available(manifest("9.9.9"));
        assert_eq!(result_heading(&available), "A new version is available");
        assert_eq!(result_title(&available), "Update Available");
        assert_eq!(
            result_icon_name(&available),
            "software-update-available-symbolic"
        );

        let unavailable = UpdateOutcome::Unavailable("offline".to_string());
        assert_eq!(result_heading(&unavailable), "Could Not Check for Updates");
        assert_eq!(result_title(&unavailable), "Could Not Check for Updates");
        assert_eq!(result_icon_name(&unavailable), "dialog-warning-symbolic");
        reset_locale();
    }

    #[test]
    fn result_wording_translates_to_spanish() {
        set_locale(Locale::Spanish);
        assert_eq!(
            result_heading(&UpdateOutcome::UpToDate),
            "NextSync está actualizado"
        );
        assert_eq!(
            result_title(&UpdateOutcome::Available(manifest("9.9.9"))),
            "Actualización disponible"
        );
        assert_eq!(
            result_heading(&UpdateOutcome::Unavailable(String::new())),
            "No se pudieron buscar actualizaciones"
        );
        reset_locale();
    }

    #[test]
    fn about_constants_point_at_the_rewrite_repository() {
        assert_eq!(APPLICATION_ID, "io.github.gnacho.nextsync");
        assert!(WEBSITE_URL.contains("gnacho/nextsync-rs"));
        assert_eq!(CHECK_UPDATES_URI, "nextsync://check-update");
        assert_eq!(
            RELEASES_URL,
            "https://github.com/gnacho/nextsync-rs/releases/latest"
        );
    }

    #[test]
    fn building_the_about_dialog_does_not_crash_without_a_display() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let dialog = build_about_dialog("0.1.0");
            assert_eq!(dialog.application_name().as_str(), "NextSync");
            assert_eq!(dialog.version().as_str(), "0.1.0");
            assert_eq!(dialog.developer_name().as_str(), DEVELOPER_NAME);
            assert_eq!(dialog.license_type(), gtk4::License::Gpl30);
            assert_eq!(dialog.website().as_str(), WEBSITE_URL);
            assert_eq!(dialog.issue_url().as_str(), ISSUES_URL);
            reset_locale();
        });
    }

    #[test]
    fn building_the_checking_dialog_does_not_crash_without_a_display() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let dialog = build_checking_dialog();
            assert_eq!(dialog.title().as_str(), "Checking for Updates");
            assert!(!dialog.can_close());
            reset_locale();
        });
    }

    #[test]
    fn building_the_update_result_dialog_covers_all_three_states() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            for outcome in [
                UpdateOutcome::UpToDate,
                UpdateOutcome::Available(manifest("9.9.9")),
                UpdateOutcome::Unavailable("offline".to_string()),
            ] {
                let dialog = build_update_result_dialog(&outcome, "0.1.0");
                assert_eq!(dialog.title().as_str(), result_title(&outcome));
            }
            reset_locale();
        });
    }
}
