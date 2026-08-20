//! Per-folder status row with a live state glyph.
//!
//! Fase 5 (Task 5.1): mirrors `ui/folder_status.py`. One row per synchronized
//! folder showing its current state (a check when synchronized, a spinner
//! while syncing), a subtitle with the remote path and the last sync stamp,
//! and a more (…) menu with the per-folder actions.
//!
//! The row subscribes to the folder's [`StateController`] and re-renders on
//! every change; when no controller is available it renders the
//! `Unconfigured` presentation statically.

use std::path::Path;
use std::rc::Rc;

use libadwaita::prelude::*;

use crate::core::account_runtime::FolderRuntime;
use crate::state::{AppState, StateController, StateSnapshot};
use crate::storage::config::FolderConfig;
use crate::util::i18n::t;

/// The `(icon_name, status_label)` pair for a folder state, mirroring the
/// Python `STATE_PRESENTATION` table. The label is translated through
/// [`t`]; because every msgid is a `'static` literal the signature keeps
/// `'static` lifetimes. The icons are the app's Lucide family
/// (`nextsync-state-*`): waiting pauses, sync-ok checks, a running sync
/// spins dots, trouble states show the info glyph (issue #94).
pub fn folder_status_presentation(state: AppState) -> (&'static str, &'static str) {
    match state {
        AppState::Unconfigured => ("nextsync-state-attention-symbolic", t("Not Configured")),
        AppState::IdleOk => ("nextsync-state-check-symbolic", t("Synchronized")),
        AppState::IdleManualOnly => ("nextsync-state-paused-symbolic", t("Automatic Sync Is Off")),
        AppState::IdleNotSynced => (
            "nextsync-state-attention-symbolic",
            t("Not Synchronized Yet"),
        ),
        AppState::SyncQueued => (
            "nextsync-state-paused-symbolic",
            t("Synchronization Scheduled"),
        ),
        AppState::Syncing => ("nextsync-state-syncing-symbolic", t("Synchronizing…")),
        AppState::PausedUser => ("nextsync-state-paused-symbolic", t("Paused")),
        AppState::PausedBattery => ("nextsync-state-paused-symbolic", t("Paused on Battery")),
        AppState::Offline => ("nextsync-state-globe-off-symbolic", t("Offline")),
        AppState::Error => (
            "nextsync-state-attention-symbolic",
            t("Synchronization Error"),
        ),
        AppState::AuthRequired => (
            "nextsync-state-attention-symbolic",
            t("Account Needs Attention"),
        ),
        AppState::KeyringLocked => (
            "nextsync-state-attention-symbolic",
            t("Password Keyring Locked"),
        ),
        AppState::DeleteReview => ("nextsync-state-attention-symbolic", t("Review Deletions")),
    }
}

/// The color class a folder state's icon carries (issue #94): green while
/// syncing and when synchronized, red when the folder needs attention,
/// none for the neutral waiting/paused states.
pub fn folder_status_color(state: AppState) -> Option<&'static str> {
    match state {
        AppState::IdleOk | AppState::Syncing => Some("success"),
        AppState::Error
        | AppState::AuthRequired
        | AppState::KeyringLocked
        | AppState::DeleteReview => Some("error"),
        _ => None,
    }
}

/// The translated subtitle segment for a synced folder ("Synced in local
/// {folder}", issue #78). The folder is the remote path without its leading
/// slash: the name is what matters, the slash is noise.
pub fn remote_label(remote_path: &str) -> String {
    let folder = remote_path.trim_start_matches('/');
    t("Synced in local {folder}").replace("{folder}", folder)
}

/// The translated suffix for the folder's local used space ("{size} local",
/// issue #43).
pub fn local_size_label(bytes: u64) -> String {
    t("{size} local").replace("{size}", &crate::nextcloud::api::format_bytes(bytes))
}

/// One line of live progress for a folder row (issue #86): the translated
/// action, the file being operated on and, when the engine reports one, the
/// operation counter for the whole run.
pub fn progress_line_text(
    progress: &crate::nextcloud::nextcloudcmd_progress::SyncProgress,
    file: &str,
) -> String {
    let action = match progress.action.as_str() {
        "download" => t("downloading {file}"),
        "upload" => t("uploading {file}"),
        "delete" => t("deleting {file}"),
        "conflict" => t("conflict on {file}"),
        "checking" => t("checking {file}"),
        _ => t("processing {file}"),
    }
    .replace("{file}", file);
    if progress.processed > 0 {
        t("{action} · {count}")
            .replace("{action}", &action)
            .replace("{count}", &progress.processed.to_string())
    } else {
        action
    }
}

/// Recursively sum the size of the regular files below `root`, skipping
/// symlinks entirely (their targets are not part of the synchronized tree).
///
/// Iterative like [`crate::core::delete_guard::scan_local_files`] so deep
/// trees cannot overflow the stack; unreadable directories contribute
/// nothing.
pub fn local_tree_size(root: &Path) -> u64 {
    use std::fs;
    if !root.is_dir() {
        return 0;
    }
    let mut total: u64 = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(children) = fs::read_dir(&directory) else {
            continue;
        };
        for child in children.flatten() {
            let Ok(kind) = child.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(child.path());
            } else if kind.is_file() {
                total += child.metadata().map(|meta| meta.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// How a last-sync stamp is rendered, mirroring `AccountView._format_sync_stamp`.
///
/// Not-yet-synced folders show a "Not yet synchronized" label; otherwise the
/// stamp is re-formatted as `%x %H:%M` in the local timezone. Returns the
/// subtitle segment (empty when nothing is available).
pub fn format_sync_stamp(value: Option<&str>) -> String {
    match value {
        None | Some("") => t("Not yet synchronized").to_string(),
        Some(raw) => match parse_iso8601(raw) {
            Some((date, time)) => format!("{date} {time}"),
            None => raw.to_string(),
        },
    }
}

/// Best-effort ISO-8601 local rendering (`%x %H:%M` in a C-locale-free way):
/// splits the date part and the time part of a timestamp.
fn parse_iso8601(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    let (date_part, time_part) = raw.split_once(['T', ' '])?;
    let date = date_part.replace('-', "/");
    let time = time_part.chars().take(5).collect::<String>();
    Some((date, time))
}

/// Match folder sessions to folder runtimes by `folder.id` in session order.
/// A folder without a runtime yields `None` and the row degrades to a static
/// presentation.
pub fn pair_folder_runtimes(
    folders: &[FolderConfig],
    runtimes: &std::collections::HashMap<String, FolderRuntime>,
) -> Vec<(FolderConfig, Option<FolderRuntime>)> {
    folders
        .iter()
        .map(|folder| (folder.clone(), runtimes.get(&folder.id).cloned()))
        .collect()
}

/// Per-folder menu callbacks. All optional; the corresponding menu item is
/// omitted when `None`.
#[derive(Default)]
pub struct FolderRowCallbacks {
    pub on_open: Option<Rc<dyn Fn()>>,
    pub on_edit_ignored: Option<Rc<dyn Fn()>>,
    pub on_force_sync: Option<Rc<dyn Fn()>>,
    pub on_toggle_pause: Option<Rc<dyn Fn()>>,
    pub on_remove: Option<Rc<dyn Fn()>>,
    pub on_pending_changes: Option<Rc<dyn Fn()>>,
}

/// A GTK action row rendering one synchronized folder with live status.
pub struct FolderStatusRow {
    pub row: libadwaita::ActionRow,
    /// The row plus its run-progress bar (issue #90); what the folder list
    /// appends.
    pub slot: gtk4::Box,
    icon: gtk4::Image,
    spinner: gtk4::Spinner,
    progress_bar: gtk4::ProgressBar,
    /// Suffix label with the folder's local used space (issue #43); hidden
    /// until a size is known.
    pub local_size: gtk4::Label,
    /// Suffix label with the live per-file progress (issue #86); hidden
    /// while no synchronization is running.
    progress_label: gtk4::Label,
    _menu_button: gtk4::MenuButton,
    menu_model: gio::Menu,
    pending_item: Option<gio::MenuItem>,
    _actions: std::collections::HashMap<String, gio::SimpleAction>,
    format_last_sync: Option<Rc<dyn Fn() -> String>>,
    remote_path: String,
    _subscription: Option<crate::state::Subscription>,
    _progress_subscription: Option<crate::state::Subscription>,
}

impl FolderStatusRow {
    /// Build the row for one folder. `state` drives the live rendering.
    pub fn new(
        folder: FolderConfig,
        state: Option<StateController>,
        callbacks: FolderRowCallbacks,
        format_last_sync: Option<Rc<dyn Fn() -> String>>,
        is_paused: Option<Rc<dyn Fn() -> bool>>,
    ) -> Self {
        let name = Path::new(&folder.local_root)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_string())
            .unwrap_or_else(|| folder.local_root.clone());
        let row = libadwaita::ActionRow::builder()
            .title(name)
            .title_lines(1)
            .subtitle_lines(1)
            .activatable(true)
            .selectable(false)
            .build();
        // The bold folder name (issue #78) comes from the app-wide style
        // provider; the internal title label has no public handle.
        row.add_css_class("folder-source");

        let icon = gtk4::Image::builder()
            .icon_name("folder-symbolic")
            .pixel_size(22)
            .build();
        row.add_prefix(&icon);

        // Local used-space suffix (issue #43); stays hidden until the
        // background walk delivers a size.
        let local_size = gtk4::Label::builder()
            .css_classes(["dim-label"])
            .valign(gtk4::Align::Center)
            .visible(false)
            .build();
        row.add_suffix(&local_size);

        // Live per-file progress (issue #86): small dim text next to the
        // status icon while a run is in flight, gone when it ends.
        let progress_label = gtk4::Label::builder()
            .css_classes(["dim-label", "caption"])
            .valign(gtk4::Align::Center)
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .max_width_chars(28)
            .visible(false)
            .build();
        row.add_suffix(&progress_label);

        let spinner = gtk4::Spinner::builder().build();
        spinner.set_visible(false);
        row.add_suffix(&spinner);

        // The green run progress under the row (issue #90): a slim
        // indeterminate bar while the run is in flight (the engine reports
        // operations done, never a total), gone when it ends. The row and
        // the bar share a vertical slot that the folder list appends.
        let progress_bar = gtk4::ProgressBar::builder()
            .hexpand(true)
            .valign(gtk4::Align::End)
            .height_request(3)
            .visible(false)
            .build();
        progress_bar.add_css_class("nextsync-run-bar");
        let slot = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(0)
            .build();
        slot.append(&row);
        slot.append(&progress_bar);

        let menu_button = gtk4::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text(t("Folder options"))
            .valign(gtk4::Align::Center)
            .css_classes(["flat"])
            .build();
        row.add_suffix(&menu_button);

        let menu_actions = gio::SimpleActionGroup::new();
        row.insert_action_group("folder", Some(&menu_actions));

        let mut actions: std::collections::HashMap<String, gio::SimpleAction> =
            std::collections::HashMap::new();
        for (name, callback) in [
            ("open", callbacks.on_open.clone()),
            ("edit-ignored", callbacks.on_edit_ignored.clone()),
            ("force-sync", callbacks.on_force_sync.clone()),
            ("toggle-pause", callbacks.on_toggle_pause.clone()),
            ("remove", callbacks.on_remove.clone()),
            ("pending-changes", callbacks.on_pending_changes.clone()),
        ] {
            let Some(callback) = callback else { continue };
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_action, _param| callback());
            menu_actions.add_action(&action);
            actions.insert(name.to_string(), action);
        }

        let menu = gio::Menu::new();
        if actions.contains_key("open") {
            let item = gio::MenuItem::new(Some(t("Open local folder")), Some("folder.open"));
            item.set_icon(&gio::ThemedIcon::new("folder-open-symbolic"));
            menu.append_item(&item);
        }
        if actions.contains_key("edit-ignored") {
            let item =
                gio::MenuItem::new(Some(t("Edit ignored files")), Some("folder.edit-ignored"));
            item.set_icon(&gio::ThemedIcon::new("text-x-generic-symbolic"));
            menu.append_item(&item);
        }
        if actions.contains_key("force-sync") {
            let item = gio::MenuItem::new(Some(t("Force sync now")), Some("folder.force-sync"));
            item.set_icon(&gio::ThemedIcon::new("emblem-synchronizing-symbolic"));
            menu.append_item(&item);
        }
        if actions.contains_key("toggle-pause") {
            let paused = is_paused.as_ref().map(|f| f()).unwrap_or(false);
            let item = gio::MenuItem::new(
                Some(if paused {
                    t("Resume sync")
                } else {
                    t("Pause sync")
                }),
                Some("folder.toggle-pause"),
            );
            item.set_icon(&gio::ThemedIcon::new(if paused {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            }));
            menu.append_item(&item);
        }
        if actions.contains_key("remove") {
            let item = gio::MenuItem::new(Some(t("Remove synchronization")), Some("folder.remove"));
            item.set_icon(&gio::ThemedIcon::new("user-trash-symbolic"));
            menu.append_item(&item);
        }
        // Pending-changes menu item (issue #92): only offered once a scan
        // finds something pending. The action exists from the start; the
        // item joins the model on demand.
        let pending_item = actions.contains_key("pending-changes").then(|| {
            let item =
                gio::MenuItem::new(Some(t("Pending changes…")), Some("folder.pending-changes"));
            item.set_icon(&gio::ThemedIcon::new("nextsync-list-checks-symbolic"));
            item
        });
        let popover = gtk4::PopoverMenu::from_model(Some(&menu));
        menu_button.set_popover(Some(&popover));

        let remote_path = folder.remote_path.clone();
        let mut this = Self {
            row,
            slot,
            icon,
            spinner,
            progress_bar,
            local_size,
            progress_label,
            _menu_button: menu_button,
            menu_model: menu,
            pending_item,
            _actions: actions,
            format_last_sync,
            remote_path: remote_path.clone(),
            _subscription: None,
            _progress_subscription: None,
        };

        match state {
            Some(controller) => {
                let icon = this.icon.clone();
                let spinner = this.spinner.clone();
                let progress_bar = this.progress_bar.clone();
                let row = this.row.clone();
                let remote_path = this.remote_path.clone();
                let format_last_sync = this.format_last_sync.clone();
                let subscription = controller.subscribe(move |snapshot: &StateSnapshot| {
                    render(
                        &row,
                        &icon,
                        &spinner,
                        &progress_bar,
                        &remote_path,
                        format_last_sync.as_ref().map(|f| f()),
                        snapshot,
                    );
                });
                this._subscription = Some(subscription);
                // Live per-file progress (issue #86). Only the widgets are
                // captured, so the closure dies with the label and the row.
                let progress_label = this.progress_label.clone();
                let progress_subscription =
                    controller.subscribe_progress(move |progress| match progress {
                        Some(progress) if progress.is_operation() => {
                            let file = std::path::Path::new(&progress.path)
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| progress.path.clone());
                            progress_label.set_text(&progress_line_text(progress, &file));
                            progress_label.set_visible(true);
                        }
                        _ => progress_label.set_visible(false),
                    });
                this._progress_subscription = Some(progress_subscription);
            }
            None => {
                let snapshot = StateSnapshot::new(AppState::Unconfigured);
                render(
                    &this.row,
                    &this.icon,
                    &this.spinner,
                    &this.progress_bar,
                    &this.remote_path,
                    this.format_last_sync.as_ref().map(|f| f()),
                    &snapshot,
                );
            }
        }
        this
    }

    /// Show (or clear) the local used-space suffix (issue #43).
    pub fn set_local_size(&self, text: &str) {
        self.local_size.set_text(text);
        self.local_size.set_visible(!text.is_empty());
    }

    /// Handle for the pending-changes menu gating (issue #92): enough state
    /// to add or drop the item from another closure without owning the row.
    pub fn pending_handle(&self) -> PendingMenuHandle {
        PendingMenuHandle {
            menu_model: self.menu_model.clone(),
            pending_item: self.pending_item.clone(),
            in_menu: std::cell::Cell::new(false),
        }
    }
}

/// Detached control of the pending-changes menu item (issue #92): the scan
/// runs off the UI thread and reports through [`PendingMenuHandle`].
#[derive(Clone)]
pub struct PendingMenuHandle {
    menu_model: gio::Menu,
    pending_item: Option<gio::MenuItem>,
    in_menu: std::cell::Cell<bool>,
}

impl PendingMenuHandle {
    /// Offer (or drop) the entry based on a scan result.
    pub fn set_pending_changes(&self, pending: bool) {
        if pending && !self.in_menu.get() {
            if let Some(item) = &self.pending_item {
                self.menu_model.append_item(item);
                self.in_menu.set(true);
            }
        } else if !pending && self.in_menu.get() {
            let position = self.menu_model.n_items().saturating_sub(1);
            self.menu_model.remove(position);
            self.in_menu.set(false);
        }
    }
}

/// Render one snapshot into the row widgets.
fn render(
    row: &libadwaita::ActionRow,
    icon: &gtk4::Image,
    spinner: &gtk4::Spinner,
    progress_bar: &gtk4::ProgressBar,
    remote_path: &str,
    last_sync: Option<String>,
    snapshot: &StateSnapshot,
) {
    let (icon_name, status) = folder_status_presentation(snapshot.state);
    let syncing = snapshot.state == AppState::Syncing;
    let queued = snapshot.state == AppState::SyncQueued;
    // Issue #94: one Lucide glyph per state, tinted green while syncing and
    // synchronized, red when the folder needs attention. While syncing the
    // glyph (circle-ellipsis) spins instead of the old bare spinner.
    icon.set_visible(true);
    icon.set_icon_name(Some(icon_name));
    if icon_name == "nextsync-state-syncing-symbolic" {
        icon.add_css_class("nextsync-spin");
    } else {
        icon.remove_css_class("nextsync-spin");
    }
    if let Some(color) = folder_status_color(snapshot.state) {
        icon.add_css_class(color);
    } else {
        icon.remove_css_class("success");
        icon.remove_css_class("error");
    }
    // The legacy spinner stays hidden; kept for API stability.
    spinner.set_visible(false);
    spinner.stop();
    // Issue #90: the slim green run bar pulses while the run is in flight
    // and disappears when it ends (queued shows it idle at zero).
    progress_bar.set_visible(syncing || queued);
    if syncing {
        progress_bar.pulse();
    }
    let mut parts = vec![status.to_string()];
    // The synced-in-local segment belongs to the synchronized state only:
    // with the green check visible. While queued, syncing or in trouble the
    // line would read as a stale claim.
    if !remote_path.is_empty() && snapshot.state == AppState::IdleOk {
        parts.push(remote_label(remote_path));
    }
    if let Some(last_sync) = last_sync {
        parts.push(last_sync);
    }
    row.set_subtitle(&parts.join(" · "));
    if !snapshot.message.is_empty() {
        row.set_tooltip_text(Some(&snapshot.message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{reset_locale, set_locale, Locale};

    #[test]
    fn presentation_covers_every_state() {
        set_locale(Locale::English);
        for state in [
            AppState::Unconfigured,
            AppState::IdleOk,
            AppState::IdleManualOnly,
            AppState::SyncQueued,
            AppState::Syncing,
            AppState::PausedUser,
            AppState::PausedBattery,
            AppState::Offline,
            AppState::Error,
            AppState::AuthRequired,
            AppState::KeyringLocked,
            AppState::DeleteReview,
        ] {
            let (icon, label) = folder_status_presentation(state);
            assert!(!icon.is_empty());
            assert!(!label.is_empty());
        }
        reset_locale();
    }

    #[test]
    fn idle_ok_presents_as_synchronized() {
        set_locale(Locale::English);
        let (icon, label) = folder_status_presentation(AppState::IdleOk);
        assert_eq!(icon, "nextsync-state-check-symbolic");
        assert_eq!(label, "Synchronized");
        assert_eq!(folder_status_color(AppState::IdleOk), Some("success"));
        reset_locale();
    }

    #[test]
    fn syncing_presents_with_ellipsis_icon() {
        set_locale(Locale::English);
        let (icon, label) = folder_status_presentation(AppState::Syncing);
        assert_eq!(icon, "nextsync-state-syncing-symbolic");
        assert_eq!(label, "Synchronizing…");
        assert_eq!(folder_status_color(AppState::Syncing), Some("success"));
        reset_locale();
    }

    #[test]
    fn per_state_icons_and_colors() {
        // Issue #94: waiting pauses, attention turns red, offline shows the
        // struck globe.
        assert_eq!(
            folder_status_presentation(AppState::SyncQueued).0,
            "nextsync-state-paused-symbolic"
        );
        assert_eq!(folder_status_color(AppState::SyncQueued), None);
        for state in [
            AppState::Error,
            AppState::AuthRequired,
            AppState::KeyringLocked,
            AppState::DeleteReview,
        ] {
            assert_eq!(
                folder_status_presentation(state).0,
                "nextsync-state-attention-symbolic",
                "state {state:?}"
            );
            assert_eq!(folder_status_color(state), Some("error"));
        }
        assert_eq!(
            folder_status_presentation(AppState::Offline).0,
            "nextsync-state-globe-off-symbolic"
        );
    }

    #[test]
    fn presentation_translates_to_spanish() {
        set_locale(Locale::Spanish);
        let (_, label) = folder_status_presentation(AppState::Syncing);
        assert_eq!(label, "Sincronizando…");
        let (_, label) = folder_status_presentation(AppState::IdleOk);
        assert_eq!(label, "Sincronizado");
        let (_, label) = folder_status_presentation(AppState::Offline);
        assert_eq!(label, "Sin conexión");
        assert_eq!(remote_label("/docs"), "Sincronizado en local docs");
        assert_eq!(remote_label("docs"), "Sincronizado en local docs");
        // Live progress line (issue #86): translated action plus counter.
        let mut progress =
            crate::nextcloud::nextcloudcmd_progress::SyncProgress::new("upload", "/tmp/a/b.txt");
        assert_eq!(progress_line_text(&progress, "b.txt"), "subiendo b.txt");
        progress.processed = 7;
        assert_eq!(progress_line_text(&progress, "b.txt"), "subiendo b.txt · 7");
        reset_locale();
    }

    #[test]
    fn sync_stamp_formats_iso_dates_locally() {
        set_locale(Locale::English);
        assert_eq!(
            format_sync_stamp(Some("2026-08-13T09:30:00Z")),
            "2026/08/13 09:30"
        );
        assert_eq!(
            format_sync_stamp(Some("2026-08-13 09:30")),
            "2026/08/13 09:30"
        );
        reset_locale();
    }

    #[test]
    fn sync_stamp_defaults_when_missing() {
        set_locale(Locale::English);
        assert_eq!(format_sync_stamp(None), "Not yet synchronized");
        assert_eq!(format_sync_stamp(Some("")), "Not yet synchronized");
        assert_eq!(format_sync_stamp(Some("garbage")), "garbage");
        reset_locale();
    }

    #[test]
    fn sync_stamp_defaults_translate_to_spanish() {
        set_locale(Locale::Spanish);
        assert_eq!(format_sync_stamp(None), "Aún no sincronizado");
        reset_locale();
    }

    #[test]
    fn pairing_matches_folders_to_runtimes_by_id() {
        let folders = vec![
            FolderConfig {
                id: "f1".to_string(),
                local_root: "/tmp/a".to_string(),
                remote_path: "/docs".to_string(),
                space_id: None,
                size_confirmed: false,
            },
            FolderConfig {
                id: "f2".to_string(),
                local_root: "/tmp/b".to_string(),
                remote_path: "/photos".to_string(),
                space_id: None,
                size_confirmed: false,
            },
        ];
        let runtimes = std::collections::HashMap::new();
        // Without a matching runtime every folder pairs to None.
        let paired = pair_folder_runtimes(&folders, &runtimes);
        assert_eq!(paired.len(), 2);
        assert!(paired[0].1.is_none());
        assert!(paired[1].1.is_none());
    }

    #[test]
    fn row_construction_smoke() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let folder = FolderConfig {
                id: "f1".to_string(),
                local_root: "/tmp/a".to_string(),
                remote_path: "/docs".to_string(),
                space_id: None,
                size_confirmed: false,
            };
            let state = StateController::new(AppState::IdleOk);
            let row = FolderStatusRow::new(
                folder,
                Some(state.clone()),
                FolderRowCallbacks::default(),
                None,
                None,
            );
            assert_eq!(row.row.title(), "a");
            assert_eq!(
                row.row.subtitle().as_deref(),
                Some("Synchronized · Synced in local docs")
            );
            // The local-size suffix stays hidden until a size arrives and
            // leaves the subtitle alone (issue #43).
            assert!(!row.local_size.is_visible());
            row.set_local_size("12.4 KiB local");
            assert_eq!(row.local_size.label(), "12.4 KiB local");
            assert!(row.local_size.is_visible());
            assert_eq!(
                row.row.subtitle().as_deref(),
                Some("Synchronized · Synced in local docs")
            );
            row.set_local_size("");
            assert!(!row.local_size.is_visible());
            // Outside the synchronized state the synced-in-local segment
            // disappears: queued and syncing rows show only the status.
            state.set(AppState::SyncQueued, "queued");
            assert_eq!(
                row.row.subtitle().as_deref(),
                Some("Synchronization Scheduled")
            );
            state.set(AppState::IdleOk, "ok");
            assert_eq!(
                row.row.subtitle().as_deref(),
                Some("Synchronized · Synced in local docs")
            );
            // Live progress (issue #86): a progress event shows the line,
            // clearing it hides the label again.
            state.set_progress(Some(crate::nextcloud::sync_engine::SyncProgress {
                action: "download".to_string(),
                path: "/tmp/a/song.mp3".to_string(),
                processed: 3,
            }));
            assert!(row.progress_label.is_visible());
            assert_eq!(row.progress_label.label(), "downloading song.mp3 · 3");
            state.set_progress(None);
            assert!(!row.progress_label.is_visible());
            reset_locale();
        });
    }

    #[test]
    fn local_tree_size_sums_files_and_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/b.txt"), vec![0u8; 1024]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("a.txt"), dir.path().join("link")).unwrap();
        assert_eq!(local_tree_size(dir.path()), 100 + 1024);
        // A missing root contributes nothing.
        assert_eq!(
            local_tree_size(std::path::Path::new("/nonexistent-nextsync")),
            0
        );
    }

    #[test]
    fn local_size_label_formats_and_translates() {
        set_locale(Locale::English);
        assert_eq!(local_size_label(512), "512 B local");
        assert_eq!(local_size_label(12 * 1024 * 1024 * 1024), "12.0 GiB local");
        set_locale(Locale::Spanish);
        assert_eq!(
            local_size_label(12 * 1024 * 1024 * 1024),
            "12.0 GiB en local"
        );
        reset_locale();
    }
}
