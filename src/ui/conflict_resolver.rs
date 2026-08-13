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
//! - **No i18n**: strings are English like the rest of the current UI (Task
//!   6.1 adds the catalogs). The pure activity parsing lives in
//!   [`crate::ui::activity`] and is imported here.

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

/// Window title, mirroring the Python `_("Sync Activity and Conflicts")`.
pub const WINDOW_TITLE: &str = "Sync Activity and Conflicts";
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
    _toast_overlay: libadwaita::ToastOverlay,
}

impl ConflictResolverWindow {
    /// Build the window (already wired, not yet shown). `local_root` is the
    /// synchronized folder to scan for conflicted copies and `matcher` the
    /// exclusion rules that apply to its files.
    pub fn new(
        application: &libadwaita::Application,
        local_root: impl AsRef<Path>,
        matcher: ExclusionMatcher,
        log: Rc<dyn RecentLog>,
        on_close: Option<Rc<dyn Fn()>>,
    ) -> Self {
        let window = libadwaita::Window::builder()
            .title(WINDOW_TITLE)
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
        recent_box.append(&recent_scroller);
        let recent_page = stack.add_named(&recent_box, Some("recent"));
        recent_page.set_title(Some("Recent"));

        // ---- Conflicts page -------------------------------------------------
        let conflicts_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(8)
            .build();
        conflicts_box.set_margin_top(4);
        conflicts_box.set_margin_bottom(4);
        let summary = gtk4::Label::builder()
            .xalign(0.0)
            .css_classes(["dim-label"])
            .wrap(true)
            .build();
        let empty_state = libadwaita::StatusPage::builder()
            .icon_name("emblem-ok-symbolic")
            .title("No Conflicts")
            .description("No Nextcloud conflicted copies were found in this folder.")
            .vexpand(true)
            .build();
        empty_state.set_visible(false);
        let conflict_list = gtk4::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        let conflict_scroller = gtk4::ScrolledWindow::builder().vexpand(true).build();
        conflict_scroller.set_child(Some(&conflict_list));
        conflicts_box.append(&summary);
        conflicts_box.append(&empty_state);
        conflicts_box.append(&conflict_scroller);
        let conflicts_page = stack.add_named(&conflicts_box, Some("conflicts"));
        conflicts_page.set_title(Some("Conflicts"));

        switcher.set_stack(Some(&stack));
        toolbar.set_content(Some(&stack));
        toast_overlay.set_child(Some(&toolbar));
        window.set_content(Some(&toast_overlay));

        // ---- Header actions -------------------------------------------------
        let refresh = gtk4::Button::builder()
            .label("Refresh")
            .icon_name("view-refresh-symbolic")
            .build();
        header.pack_end(&refresh);
        let close = gtk4::Button::builder()
            .label("Close")
            .css_classes(["suggested-action"])
            .build();
        header.pack_end(&close);
        let window_for_close = window.clone();
        close.connect_clicked(move |_| window_for_close.close());

        // ---- Shared conflict reload target ----------------------------------
        let target = ReloadTarget {
            list: conflict_list.clone(),
            summary: summary.clone(),
            empty_state: empty_state.clone(),
            local_root: local_root.as_ref().to_path_buf(),
            matcher: matcher.clone(),
            toast_overlay: toast_overlay.clone(),
        };
        let target_for_refresh = target.clone();
        refresh.connect_clicked(move |_| target_for_refresh.reload());
        target.reload();

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
    summary: gtk4::Label,
    empty_state: libadwaita::StatusPage,
    local_root: PathBuf,
    matcher: ExclusionMatcher,
    toast_overlay: libadwaita::ToastOverlay,
}

impl ReloadTarget {
    /// Rescan the folder and rebuild the Conflicts list.
    fn reload(&self) {
        reload_conflicts(
            &self.list,
            &self.summary,
            &self.empty_state,
            &self.local_root,
            &self.matcher,
            &self.toast_overlay,
        );
    }

    /// Show a toast on the window's overlay.
    fn toast(&self, message: impl AsRef<str>) {
        self.toast_overlay
            .add_toast(libadwaita::Toast::new(message.as_ref()));
    }
}

/// Rebuild the Conflicts list from a fresh scan.
fn reload_conflicts(
    list: &gtk4::ListBox,
    summary: &gtk4::Label,
    empty_state: &libadwaita::StatusPage,
    local_root: &Path,
    matcher: &ExclusionMatcher,
    toast_overlay: &libadwaita::ToastOverlay,
) {
    while let Some(child) = list.first_child() {
        child.unparent();
    }
    let conflicts = find_conflicts(local_root, matcher);
    if conflicts.is_empty() {
        empty_state.set_visible(true);
        summary.set_text(&format!(
            "No conflicted copies found in {}.",
            local_root.display()
        ));
        return;
    }
    empty_state.set_visible(false);
    summary.set_text(&format!(
        "{} conflicted copy(ies) found in {}.",
        conflicts.len(),
        local_root.display()
    ));
    for conflict in conflicts {
        let target = ReloadTarget {
            list: list.clone(),
            summary: summary.clone(),
            empty_state: empty_state.clone(),
            local_root: local_root.to_path_buf(),
            matcher: matcher.clone(),
            toast_overlay: toast_overlay.clone(),
        };
        list.append(&build_conflict_row(&conflict, &target));
    }
}

/// One conflict row: warning icon, original/date/size caption and the three
/// resolution buttons.
fn build_conflict_row(conflict: &ConflictFile, target: &ReloadTarget) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(10)
        .build();
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let icon = gtk4::Image::builder()
        .icon_name("dialog-warning-symbolic")
        .pixel_size(24)
        .build();
    content.append(&icon);

    let text = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    let title = gtk4::Label::builder()
        .label(conflict.name())
        .xalign(0.0)
        .wrap(false)
        .build();
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let subtitle = gtk4::Label::builder()
        .label(format!(
            "Original: {} · {} · {} bytes",
            conflict.original_name,
            describe_modified(conflict.modified),
            conflict.size
        ))
        .xalign(0.0)
        .css_classes(["dim-label"])
        .wrap(true)
        .build();
    text.append(&title);
    text.append(&subtitle);
    content.append(&text);

    let conflict_local = conflict.clone();
    let target_local = target.clone();
    let keep_local_button = gtk4::Button::builder().label("Keep Local").build();
    keep_local_button.set_tooltip_text(Some("Delete the conflicted copy, keep the working file."));
    keep_local_button.connect_clicked(move |_| {
        if keep_local(&conflict_local) {
            target_local.toast(format!(
                "Kept local version of {}",
                conflict_local.original_name
            ));
            target_local.reload();
        }
    });
    content.append(&keep_local_button);

    let conflict_remote = conflict.clone();
    let target_remote = target.clone();
    let keep_remote_button = gtk4::Button::builder()
        .label("Keep Remote")
        .css_classes(["suggested-action"])
        .build();
    keep_remote_button.set_tooltip_text(Some("Replace the working file with the conflicted copy."));
    keep_remote_button.connect_clicked(move |_| {
        if keep_remote(&conflict_remote) {
            target_remote.toast(format!(
                "Kept remote version of {}",
                conflict_remote.original_name
            ));
            target_remote.reload();
        }
    });
    content.append(&keep_remote_button);

    let conflict_open = conflict.clone();
    let open_button = gtk4::Button::builder().icon_name("folder-symbolic").build();
    open_button.set_tooltip_text(Some("Open in Files"));
    open_button.connect_clicked(move |_| {
        let file = gio::File::for_path(&conflict_open.path);
        let _ = gio::AppInfo::launch_default_for_uri(&file.uri(), None::<&gio::AppLaunchContext>);
    });
    content.append(&open_button);

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

/// One Recent row: level icon + (up to two-line) message.
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
        .build();
    content.append(&icon);
    let label = gtk4::Label::builder()
        .label(&entry.message)
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .build();
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_lines(2);
    content.append(&label);
    row.set_child(Some(&content));
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeLog(Vec<String>);

    impl RecentLog for FakeLog {
        fn recent_lines(&self, max: usize) -> Vec<String> {
            self.0.iter().rev().take(max).rev().cloned().collect()
        }
    }

    #[test]
    fn window_constants_are_stable() {
        assert_eq!(WINDOW_TITLE, "Sync Activity and Conflicts");
        assert_eq!(RECENT_MAX_LINES, 200);
        assert_eq!(RECENT_VISIBLE_LINES, 50);
    }

    #[test]
    fn window_construction_and_recent_rendering_smoke() {
        crate::ui::test_helpers::gtk_smoke(|| {
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let dir = tempfile::tempdir().unwrap();
            let matcher = ExclusionMatcher::defaults();
            let log = Rc::new(FakeLog(vec![
                "2026-08-07 14:12:41 INFO    Synchronization completed successfully.".to_string(),
            ]));
            let window = ConflictResolverWindow::new(&app, dir.path(), matcher, log, None);
            assert_eq!(
                window.window().title().unwrap_or_default().to_string(),
                WINDOW_TITLE
            );
            // The Recent tab already rendered the fake log line.
            assert!(window._recent_list.first_child().is_some());
            // No conflicted copies were found in the empty temp folder.
            assert!(!window._conflict_list.first_child().is_some());
        });
    }
}
