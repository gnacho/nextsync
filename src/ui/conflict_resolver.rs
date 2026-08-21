//! Unified recent-activity and conflicted-copy window (Task 5.4).
//!
//! Mirrors `ui/conflict_resolver.py` (v0.4.0): a single [`libadwaita::Window`]
//! with an [`libadwaita::ViewStack`] + [`libadwaita::ViewSwitcher`] whose tabs
//! are:
//!
//! - **Recent**: the live synchronization log. Each line is parsed with
//!   [`crate::ui::activity::parse_activity_line`] and rendered as a row with a
//!   level icon, so an entry such as "Synchronized with conflicts" has context.
//! - **Conflicts**: every `* (Nextcloud conflicted copy <date>).*` file found
//!   in the synchronized folder, each with *Keep Local*, *Keep Remote* and
//!   *Open in Files* actions.
//!
//! The window never runs `nextcloudcmd`; it only scans the folder and performs
//! local file operations via [`crate::core::conflict_files`].
//!
//! # Log consumer
//!
//! The Python window subscribes to the real logger
//! (`logger.subscribe` + `logger.recent_lines`). The Rust log module
//! (`src/core/log.rs`) is being written by another subagent in parallel, so
//! this module defines a minimal local trait — [`RecentLog`] — as the single
//! integration point: the window reads recent lines through
//! `recent_lines(max)` and, because a getter is all we have for now, re-polls
//! it on a short timer to keep the Recent tab live. When the real logger
//! lands at merge time, the polling closure can be swapped for a push-style
//! subscription without touching the rest of the window.
//!
//! # Deviations from `conflict_resolver.py` (motivated)
//!
//! - **`RecentLog` trait + polling instead of `subscribe`**: see above. The
//!   poll redraws only when the joined line content actually changed.
//! - **`ToastOverlay` wraps the content**: the Python `_toast` looks up
//!   `get_ancestor(Adw.ToastOverlay)`, which is always `None` in a standalone
//!   `Adw.Window`, so its toasts never displayed. Here the toolbar sits inside
//!   a [`libadwaita::ToastOverlay`] and the toasts always show.
//! - **The window scans with the folder's [`ExclusionMatcher`]**: consistent
//!   with the rewrite's `find_conflicts` (the Python resolver passed no
//!   exclusions).
//! - **i18n**: user-visible strings go through [`t`] (Task 6.1); strings the
//!   catalog does not carry fall back to English. The pure activity parsing
//!   lives in [`crate::ui::activity`] and is imported here.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gio::prelude::*;
use libadwaita::prelude::*;

use crate::core::conflict_files::{
    describe_modified, find_conflicts, keep_local, keep_remote, ConflictFile,
};
use crate::core::exclusions::ExclusionMatcher;
use crate::ui::activity::{parse_activity_line, ActivityEntry};
use crate::util::i18n::t;

/// Translated window title, mirroring the Python
/// `_("Sync Activity and Conflicts")`.
pub fn window_title() -> &'static str {
    t("Sync Activity and Conflicts")
}
/// How many recent log lines are kept by the log consumer.
pub const RECENT_MAX_LINES: usize = 200;
/// How many recent lines are actually shown in the Recent tab.
pub const RECENT_VISIBLE_LINES: usize = 50;
/// Seconds between Recent-tab refreshes while polling the log getter.
pub const RECENT_POLL_SECONDS: u32 = 2;

/// Minimal consumer for the application log (see module docs: the real
/// integration with `src/core/log.rs` lands at merge time).
pub trait RecentLog {
    /// Return up to `max` most recent formatted log lines, oldest first.
    fn recent_lines(&self, max: usize) -> Vec<String>;
}

/// The application [`LogBuffer`](crate::core::log::LogBuffer) already exposes
/// the exact surface the Recent tab needs.
impl RecentLog for crate::core::log::LogBuffer {
    fn recent_lines(&self, max: usize) -> Vec<String> {
        crate::core::log::LogBuffer::recent_lines(self, max)
    }
}

/// The unified Sync Activity and Conflicts window.
pub struct ConflictResolverWindow {
    window: libadwaita::Window,
    // The widgets are kept alive by the window itself, but holding them here
    // also keeps the row handlers' captures trivially safe.
    _recent_list: gtk4::ListBox,
    _conflict_list: gtk4::ListBox,
    _deletion_list: gtk4::ListBox,
    _toast_overlay: libadwaita::ToastOverlay,
}

impl ConflictResolverWindow {
    /// Build the window (already wired, not yet shown). `local_root` is the
    /// synchronized folder to scan for conflicted copies, `matcher` the
    /// exclusion rules that apply to its files, and `scheduler` the folder
    /// scheduler driving the deletion-review tab (issue #118).
    pub fn new(
        application: &libadwaita::Application,
        local_root: impl AsRef<Path>,
        matcher: ExclusionMatcher,
        log: Rc<dyn RecentLog>,
        scheduler: Option<crate::core::scheduler::Scheduler>,
        on_close: Option<Rc<dyn Fn()>>,
    ) -> Self {
        let window = libadwaita::Window::builder()
            .title(window_title())
            .default_width(820)
            .default_height(600)
            .build();
        window.set_application(Some(application));

        let toast_overlay = libadwaita::ToastOverlay::new();
        let toolbar = libadwaita::ToolbarView::new();
        let header = gtk4::HeaderBar::new();
        toolbar.add_top_bar(&header);

        let switcher = libadwaita::ViewSwitcher::new();
        switcher.set_policy(libadwaita::ViewSwitcherPolicy::Wide);
        header.set_title_widget(Some(&switcher));

        let stack = libadwaita::ViewStack::new();

        // ---- Recent page ----------------------------------------------------
        let recent_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(8)
            .build();
        recent_box.set_margin_top(4);
        recent_box.set_margin_bottom(4);
        let recent_list = gtk4::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        let recent_scroller = gtk4::ScrolledWindow::builder().vexpand(true).build();
        recent_scroller.set_child(Some(&recent_list));
        recent_box.append(&clamp(&recent_scroller));
        let recent_page = stack.add_named(&recent_box, Some("recent"));
        recent_page.set_title(Some(t("Recent")));
        recent_page.set_icon_name(Some("nextsync-tab-recent"));

        // ---- Conflicts page -------------------------------------------------
        let conflicts_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(8)
            .build();
        conflicts_box.set_margin_top(4);
        conflicts_box.set_margin_bottom(4);
        let empty_state = libadwaita::StatusPage::builder()
            .icon_name("nextsync-activity-ok")
            .title(t("No Conflicts"))
            .description(t(
                "No Nextcloud conflicted copies were found in this folder.",
            ))
            .vexpand(true)
            .build();
        empty_state.set_visible(false);
        let conflict_list = gtk4::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        let conflict_scroller = gtk4::ScrolledWindow::builder().vexpand(true).build();
        conflict_scroller.set_child(Some(&conflict_list));
        // Bulk actions over the whole list (issue #77); kept hidden until a
        // scan finds conflicts.
        let bulk_bar = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk4::Align::End)
            .build();
        bulk_bar.set_visible(false);
        conflicts_box.append(&empty_state);
        conflicts_box.append(&clamp(&bulk_bar));
        conflicts_box.append(&clamp(&conflict_scroller));
        let conflicts_page = stack.add_named(&conflicts_box, Some("conflicts"));
        conflicts_page.set_title(Some(t("Conflicts")));
        conflicts_page.set_icon_name(Some("nextsync-tab-conflicts"));

        // ---- Deletions page -------------------------------------------------
        let deletions_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(8)
            .build();
        deletions_box.set_margin_top(4);
        deletions_box.set_margin_bottom(4);
        let deletions_empty = libadwaita::StatusPage::builder()
            .icon_name("nextsync-tab-deletions")
            .title(t("No deleted files to resolve"))
            .description(t(
                "The deletion guard has not flagged any files in this folder.",
            ))
            .vexpand(true)
            .build();
        let deletion_list = gtk4::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        let deletion_scroller = gtk4::ScrolledWindow::builder().vexpand(true).build();
        deletion_scroller.set_child(Some(&deletion_list));
        let deletion_clamp = clamp(&deletion_scroller);
        deletion_clamp.set_visible(false);
        let deletion_actions = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk4::Align::End)
            .build();
        deletion_actions.set_visible(false);
        let deletion_actions_clamp = clamp(&deletion_actions);
        deletions_box.append(&deletions_empty);
        deletions_box.append(&deletion_actions_clamp);
        deletions_box.append(&deletion_clamp);
        let deletions_page = stack.add_named(&deletions_box, Some("deletions"));
        deletions_page.set_title(Some(t("Deletions")));
        deletions_page.set_icon_name(Some("nextsync-tab-deletions"));

        switcher.set_stack(Some(&stack));
        toolbar.set_content(Some(&stack));
        toast_overlay.set_child(Some(&toolbar));
        window.set_content(Some(&toast_overlay));

        // ---- Header actions -------------------------------------------------
        let refresh = gtk4::Button::builder()
            .label(t("Refresh"))
            .icon_name("view-refresh-symbolic")
            .build();
        header.pack_end(&refresh);

        // ---- Shared conflict reload target ----------------------------------
        let target = ReloadTarget {
            list: conflict_list.clone(),
            empty_state: empty_state.clone(),
            bulk_bar: bulk_bar.clone(),
            parent: window.upcast_ref::<gtk4::Widget>().clone(),
            local_root: local_root.as_ref().to_path_buf(),
            matcher: matcher.clone(),
            toast_overlay: toast_overlay.clone(),
        };
        wire_bulk_buttons(&bulk_bar, &target);
        let target_for_refresh = target.clone();
        refresh.connect_clicked(move |_| target_for_refresh.reload());
        target.reload();

        // ---- Deletion review tab --------------------------------------------
        let deletion_target = scheduler.map(|scheduler| {
            wire_deletion_actions(
                &deletion_actions,
                &deletions_empty,
                &deletion_list,
                &deletion_clamp,
                &deletion_actions_clamp,
                scheduler,
            )
        });
        if let Some(deletion_target) = &deletion_target {
            reload_deletions(deletion_target);
        }

        // ---- Live Recent via the log getter ---------------------------------
        let last_rendered: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let poll_list = recent_list.clone();
        let poll_log = log.clone();
        let poll_last = last_rendered.clone();
        reload_recent_if_changed(&poll_list, poll_log.as_ref(), &poll_last);

        let poll_shared: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let poll_source = glib::timeout_add_seconds_local(RECENT_POLL_SECONDS, move || {
            reload_recent_if_changed(&poll_list, poll_log.as_ref(), &poll_last);
            glib::ControlFlow::Continue
        });
        *poll_shared.borrow_mut() = Some(poll_source);

        let on_close_shared = on_close.clone();
        let poll_shared_for_close = poll_shared.clone();
        window.connect_close_request(move |_| {
            // Stop the Recent poll and release its captures.
            if let Some(source) = poll_shared_for_close.borrow_mut().take() {
                source.remove();
            }
            if let Some(on_close) = &on_close_shared {
                on_close();
            }
            glib::Propagation::Proceed
        });

        Self {
            window,
            _recent_list: recent_list,
            _conflict_list: conflict_list,
            _deletion_list: deletion_list,
            _toast_overlay: toast_overlay,
        }
    }

    /// The underlying window, for presentation.
    pub fn window(&self) -> &libadwaita::Window {
        &self.window
    }

    /// Present the window.
    pub fn present(&self) {
        self.window.present();
    }
}

/// The widgets and inputs a conflict action needs to refresh the list after
/// mutating a file. Cloned into every row handler; no reference cycles (rows
/// are dropped when the list is rebuilt).
#[derive(Clone)]
struct ReloadTarget {
    list: gtk4::ListBox,
    empty_state: libadwaita::StatusPage,
    /// The bulk action bar, shown only while conflicts exist (issue #77).
    bulk_bar: gtk4::Box,
    /// The resolver window, parent of the bulk confirmation dialog.
    parent: gtk4::Widget,
    local_root: PathBuf,
    matcher: ExclusionMatcher,
    toast_overlay: libadwaita::ToastOverlay,
}

impl ReloadTarget {
    /// Rescan the folder and rebuild the Conflicts list.
    fn reload(&self) {
        reload_conflicts(self);
    }

    /// Show a toast on the window's overlay.
    fn toast(&self, message: impl AsRef<str>) {
        self.toast_overlay
            .add_toast(libadwaita::Toast::new(message.as_ref()));
    }
}

/// Wrap a widget in a `Clamp` so the boxed lists keep a uniform, centered
/// background instead of stretching white across a wide window (issue #118).
fn clamp(child: &impl IsA<gtk4::Widget>) -> libadwaita::Clamp {
    let clamp = libadwaita::Clamp::builder()
        .maximum_size(760)
        .tightening_threshold(600)
        .build();
    clamp.set_child(Some(child));
    clamp
}

/// The widgets driving the deletion-review tab (issue #118): the folder
/// scheduler plus the list, empty state and actions, cloned into the action
/// handlers.
#[derive(Clone)]
struct DeletionTarget {
    scheduler: crate::core::scheduler::Scheduler,
    list: gtk4::ListBox,
    empty_state: libadwaita::StatusPage,
    list_clamp: libadwaita::Clamp,
    actions_clamp: libadwaita::Clamp,
    approve_button: gtk4::Button,
    restore_button: gtk4::Button,
}

/// Wire the Approve/Restore actions of the deletion tab and return the target.
fn wire_deletion_actions(
    actions: &gtk4::Box,
    empty_state: &libadwaita::StatusPage,
    list: &gtk4::ListBox,
    list_clamp: &libadwaita::Clamp,
    actions_clamp: &libadwaita::Clamp,
    scheduler: crate::core::scheduler::Scheduler,
) -> DeletionTarget {
    let approve = gtk4::Button::builder()
        .label(t("Approve These Deletions Once"))
        .css_classes(["suggested-action"])
        .build();
    let restore = gtk4::Button::builder()
        .label(t("Restore from Nextcloud"))
        .build();
    actions.append(&restore);
    actions.append(&approve);

    let target = DeletionTarget {
        scheduler,
        list: list.clone(),
        empty_state: empty_state.clone(),
        list_clamp: list_clamp.clone(),
        actions_clamp: actions_clamp.clone(),
        approve_button: approve.clone(),
        restore_button: restore.clone(),
    };

    let target_for_approve = target.clone();
    approve.connect_clicked(move |_| {
        target_for_approve.scheduler.approve_delete_once();
        reload_deletions(&target_for_approve);
    });
    let target_for_restore = target.clone();
    restore.connect_clicked(move |_| {
        target_for_restore.scheduler.restore_from_server();
        reload_deletions(&target_for_restore);
    });

    target
}

/// Rebuild the deletion-review tab from the folder's current deletion alert.
fn reload_deletions(target: &DeletionTarget) {
    while let Some(child) = target.list.first_child() {
        child.unparent();
    }
    let Some(alert) = target.scheduler.delete_alert() else {
        target.empty_state.set_visible(true);
        target.list_clamp.set_visible(false);
        target.actions_clamp.set_visible(false);
        return;
    };
    target.empty_state.set_visible(false);
    for path in &alert.missing_paths {
        let row = libadwaita::ActionRow::builder()
            .title(path)
            .activatable(false)
            .selectable(false)
            .build();
        target.list.append(&row);
    }
    target.list_clamp.set_visible(true);
    target.approve_button.set_visible(alert.can_approve_once);
    target.restore_button.set_visible(true);
    target.actions_clamp.set_visible(true);
}

/// Which side every conflict should be resolved to (issue #77).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BulkSide {
    Local,
    Remote,
}

/// Wire the two bulk buttons: confirm with the real count, then resolve
/// every conflict in one pass and reload.
fn wire_bulk_buttons(bulk_bar: &gtk4::Box, target: &ReloadTarget) {
    let mut side_buttons = [
        (BulkSide::Local, t("Keep Local for All")),
        (BulkSide::Remote, t("Keep Remote for All")),
    ];
    for (side, label) in side_buttons.iter_mut() {
        let button = gtk4::Button::builder().label(*label).build();
        let target_for_bulk = target.clone();
        let side = *side;
        button.connect_clicked(move |_| confirm_bulk_resolve(&target_for_bulk, side));
        bulk_bar.append(&button);
    }
}

/// Ask for confirmation with the conflict count, then resolve them all.
fn confirm_bulk_resolve(target: &ReloadTarget, side: BulkSide) {
    let conflicts = find_conflicts(&target.local_root, &target.matcher);
    if conflicts.is_empty() {
        return;
    }
    let count = conflicts.len();
    let (question, action, done) = match side {
        BulkSide::Local => (
            t("Keep the local version of all {count} conflicted copy(ies)? The conflicted copies are deleted."),
            t("Keep Local for All"),
            t("Kept the local version of {count} file(s)"),
        ),
        BulkSide::Remote => (
            t("Keep the server version of all {count} conflicted copy(ies)? The working files are replaced."),
            t("Keep Remote for All"),
            t("Kept the server version of {count} file(s)"),
        ),
    };
    let dialog = libadwaita::AlertDialog::new(
        Some(t("Resolve All Conflicts")),
        Some(question.replacen("{count}", &count.to_string(), 1).as_str()),
    );
    dialog.add_response("cancel", t("Cancel"));
    dialog.add_response("resolve", action);
    dialog.set_response_appearance("resolve", libadwaita::ResponseAppearance::Destructive);
    let target_for_apply = target.clone();
    dialog.connect_response(None, move |dialog, response| {
        if response != "resolve" {
            return;
        }
        dialog.close();
        let resolved = apply_bulk_resolve(&target_for_apply, side);
        target_for_apply.toast(done.replacen("{count}", &resolved.to_string(), 1));
        target_for_apply.reload();
    });
    dialog.present(Some(&target.parent));
}

/// Resolve every conflict to `side`; returns how many succeeded.
fn apply_bulk_resolve(target: &ReloadTarget, side: BulkSide) -> usize {
    let conflicts = find_conflicts(&target.local_root, &target.matcher);
    conflicts
        .iter()
        .filter(|conflict| match side {
            BulkSide::Local => keep_local(conflict),
            BulkSide::Remote => keep_remote(conflict),
        })
        .count()
}

/// Rebuild the Conflicts list from a fresh scan.
fn reload_conflicts(target: &ReloadTarget) {
    let (list, empty_state, bulk_bar, local_root, matcher) = (
        &target.list,
        &target.empty_state,
        &target.bulk_bar,
        &target.local_root,
        &target.matcher,
    );
    while let Some(child) = list.first_child() {
        child.unparent();
    }
    let conflicts = find_conflicts(local_root, matcher);
    if conflicts.is_empty() {
        empty_state.set_visible(true);
        bulk_bar.set_visible(false);
        return;
    }
    empty_state.set_visible(false);
    bulk_bar.set_visible(true);
    for conflict in conflicts {
        list.append(&build_conflict_row(&conflict, target));
    }
}

/// One conflict row: warning icon, original/date/size caption and the three
/// resolution buttons stacked under the text, so long file names wrap or
/// ellipsize and the buttons stay on screen at any window width (issue #33).
fn build_conflict_row(conflict: &ConflictFile, target: &ReloadTarget) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(8)
        .build();
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let header = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(10)
        .build();

    let icon = gtk4::Image::builder()
        .icon_name("nextsync-conflict-warning")
        .pixel_size(24)
        .valign(gtk4::Align::Start)
        .build();
    header.append(&icon);

    let text = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    let title = gtk4::Label::builder()
        .label(conflict.name())
        .xalign(0.0)
        .wrap(true)
        .build();
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let subtitle = gtk4::Label::builder()
        .label(
            t("Original: {name} · {modified} · {size} bytes")
                .replace("{name}", &conflict.original_name)
                .replace("{modified}", &describe_modified(conflict.modified))
                .replace("{size}", &conflict.size.to_string()),
        )
        .xalign(0.0)
        .css_classes(["dim-label"])
        .wrap(true)
        .build();
    subtitle.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    text.append(&title);
    text.append(&subtitle);
    header.append(&text);
    content.append(&header);

    let actions = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk4::Align::End)
        .build();

    let conflict_local = conflict.clone();
    let target_local = target.clone();
    let keep_local_button = gtk4::Button::builder().label(t("Keep Local")).build();
    keep_local_button.set_tooltip_text(Some(t(
        "Delete the conflicted copy, keep the working file.",
    )));
    keep_local_button.connect_clicked(move |_| {
        if keep_local(&conflict_local) {
            target_local.toast(
                t("Kept local version of {name}").replace("{name}", &conflict_local.original_name),
            );
            target_local.reload();
        }
    });
    actions.append(&keep_local_button);

    let conflict_remote = conflict.clone();
    let target_remote = target.clone();
    let keep_remote_button = gtk4::Button::builder()
        .label(t("Keep Remote"))
        .css_classes(["suggested-action"])
        .build();
    keep_remote_button.set_tooltip_text(Some(t(
        "Replace the working file with the conflicted copy.",
    )));
    keep_remote_button.connect_clicked(move |_| {
        if keep_remote(&conflict_remote) {
            target_remote.toast(
                t("Kept remote version of {name}")
                    .replace("{name}", &conflict_remote.original_name),
            );
            target_remote.reload();
        }
    });
    actions.append(&keep_remote_button);

    let conflict_open = conflict.clone();
    let open_button = gtk4::Button::builder().icon_name("folder-symbolic").build();
    open_button.set_tooltip_text(Some(t("Open in Files")));
    open_button.connect_clicked(move |_| {
        let file = gio::File::for_path(&conflict_open.path);
        let _ = gio::AppInfo::launch_default_for_uri(&file.uri(), None::<&gio::AppLaunchContext>);
    });
    actions.append(&open_button);

    content.append(&actions);
    row.set_child(Some(&content));
    row
}

/// Render the Recent list only when the underlying log content changed.
fn reload_recent_if_changed(
    list: &gtk4::ListBox,
    log: &dyn RecentLog,
    last_rendered: &RefCell<String>,
) {
    let lines = log.recent_lines(RECENT_MAX_LINES);
    let joined = lines.join("\n");
    if *last_rendered.borrow() == joined {
        return;
    }
    *last_rendered.borrow_mut() = joined;
    rebuild_recent(list, &lines);
}

/// Rebuild the Recent list from the parsed log lines.
fn rebuild_recent(list: &gtk4::ListBox, lines: &[String]) {
    while let Some(child) = list.first_child() {
        child.unparent();
    }
    let start = lines.len().saturating_sub(RECENT_VISIBLE_LINES);
    for line in lines.iter().skip(start) {
        list.append(&recent_row(&parse_activity_line(line)));
    }
}

/// One Recent row: level icon + message using the available width.
fn recent_row(entry: &ActivityEntry) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .build();
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(10)
        .build();
    content.set_margin_top(7);
    content.set_margin_bottom(7);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let icon = gtk4::Image::builder()
        .icon_name(&entry.icon_name)
        .pixel_size(16)
        .valign(gtk4::Align::Start)
        .build();
    content.append(&icon);
    let label = gtk4::Label::builder()
        .label(&entry.message)
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .build();
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_tooltip_text(Some(&entry.message));
    content.append(&label);
    row.set_child(Some(&content));
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{reset_locale, set_locale, Locale};

    struct FakeLog(Vec<String>);

    impl RecentLog for FakeLog {
        fn recent_lines(&self, max: usize) -> Vec<String> {
            self.0.iter().rev().take(max).rev().cloned().collect()
        }
    }

    #[test]
    fn window_constants_are_stable() {
        set_locale(Locale::English);
        assert_eq!(window_title(), "Sync Activity and Conflicts");
        assert_eq!(RECENT_MAX_LINES, 200);
        assert_eq!(RECENT_VISIBLE_LINES, 50);
        reset_locale();
    }

    #[test]
    fn window_title_translates_to_spanish() {
        set_locale(Locale::Spanish);
        assert_eq!(window_title(), "Actividad y conflictos de sincronización");
        reset_locale();
    }

    #[test]
    fn window_construction_and_recent_rendering_smoke() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let dir = tempfile::tempdir().unwrap();
            let matcher = ExclusionMatcher::defaults();
            let log = Rc::new(FakeLog(vec![
                "2026-08-07 14:12:41 INFO    Synchronization completed successfully.".to_string(),
            ]));
            let window = ConflictResolverWindow::new(&app, dir.path(), matcher, log, None, None);
            assert_eq!(
                window.window().title().unwrap_or_default().to_string(),
                window_title()
            );
            // The Recent tab already rendered the fake log line.
            assert!(window._recent_list.first_child().is_some());
            // No conflicted copies were found in the empty temp folder.
            assert!(!window._conflict_list.first_child().is_some());
            reset_locale();
        });
    }
}
