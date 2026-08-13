//! Log viewer window.
//!
//! Task 5.4: replaces the placeholder with a `LogWindow` replicating
//! `ui/log_view.py` (v0.4.0). An `Adw.Window` with a header bar and a bottom
//! action bar hosts a monospace `gtk4::TextView` seeded from
//! [`LogBuffer::tail`], a copy button, an "open log folder" button and an
//! auto-scroll toggle. New lines arrive through a live subscription; the
//! buffer is trimmed to [`MAX_BUFFER_LINES`] / [`TRIM_TO_LINES`] and the
//! subscription is released on close.
//!
//! The `LogBuffer` is passed by shared reference (`LogBuffer` is `Clone` over
//! shared `Rc<RefCell>` state), so the window holds its own clone while other
//! components keep appending.
//!
//! # Deviations from `ui/log_view.py` (motivated)
//! - Strings are literal English for now; the i18n catalogs land in Fase 6
//!   (`util/i18n.rs` is still a placeholder).
//! - The subscription callback only receives the trimmed line; the text view
//!   joins lines with `\n` (the Python inserts the prefix `\n` when non-empty,
//!   which is equivalent).
//! - `close-request` returns `Propagation::Proceed` (allow close) after
//!   unsubscribing, matching the Python `return False`.
//! - On subscribe the window seeds its buffer from `LogBuffer::tail(500)`
//!   instead of relying on an initial callback (the Python `subscribe` does
//!   not replay history either).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita::prelude::*;

use crate::core::log::{LogBuffer, Subscription};

/// Hard cap for the text view buffer, in lines.
pub const MAX_BUFFER_LINES: i32 = 2_000;
/// Trim target after the hard cap is exceeded, in lines.
pub const TRIM_TO_LINES: i32 = 1_500;
/// How many history lines the window seeds from on open (Python `tail(500)`).
pub const SEED_LINES: usize = 500;

/// Log viewer window. Clone-safe: all GTK widgets are shared refs.
#[derive(Clone)]
pub struct LogWindow {
    window: libadwaita::Window,
    buffer: gtk4::TextBuffer,
    view: gtk4::TextView,
    auto_scroll: Rc<Cell<bool>>,
    subscription: Rc<RefCell<Option<Subscription>>>,
}

impl LogWindow {
    /// Build the log window as a non-modal transient of `parent`, subscribed
    /// to `logger` for new lines.
    pub fn new(parent: Option<&impl IsA<gtk4::Window>>, logger: &LogBuffer) -> Self {
        let window = libadwaita::Window::new();
        window.set_title(Some("Synchronization Log"));
        window.set_default_size(820, 560);
        window.set_transient_for(parent);

        let toolbar = libadwaita::ToolbarView::new();
        let header = gtk4::HeaderBar::new();
        toolbar.add_top_bar(&header);

        let buffer = gtk4::TextBuffer::new(None);
        buffer.set_text(&logger.tail(SEED_LINES).join("\n"));
        let view = gtk4::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .left_margin(12)
            .right_margin(12)
            .top_margin(12)
            .bottom_margin(12)
            .build();
        view.set_buffer(Some(&buffer));

        let scroller = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .build();
        scroller.set_child(Some(&view));
        toolbar.set_content(Some(&scroller));

        let action_bar = gtk4::ActionBar::new();
        let auto_scroll = Rc::new(Cell::new(true));
        let auto = gtk4::CheckButton::with_label("Auto-scroll");
        auto.set_active(true);
        let auto_scroll_guard = auto_scroll.clone();
        auto.connect_toggled(move |button| auto_scroll_guard.set(button.is_active()));
        action_bar.pack_start(&auto);

        let copy_button = gtk4::Button::with_label("Copy");
        let copy_buffer = buffer.clone();
        copy_button.connect_clicked(move |_| {
            let (start, end) = copy_buffer.bounds();
            let text = copy_buffer.text(&start, &end, true);
            if let Some(display) = gtk4::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        });
        action_bar.pack_end(&copy_button);

        let folder_button = gtk4::Button::with_label("Open Log Folder");
        let directory = logger.directory();
        folder_button.connect_clicked(move |_| {
            if let Err(error) = std::fs::create_dir_all(&directory) {
                eprintln!("Could not create log directory: {error}");
                return;
            }
            let uri = gio::File::for_path(&directory).uri();
            let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
        });
        action_bar.pack_end(&folder_button);

        toolbar.add_bottom_bar(&action_bar);
        window.set_content(Some(&toolbar));

        let log_window = LogWindow {
            window,
            buffer,
            view,
            auto_scroll,
            subscription: Rc::new(RefCell::new(None)),
        };

        let subscription = {
            let buffer = log_window.buffer.clone();
            let view = log_window.view.clone();
            let auto_scroll = log_window.auto_scroll.clone();
            logger.subscribe(move |line| {
                append_line(&buffer, &view, &auto_scroll, line);
            })
        };
        *log_window.subscription.borrow_mut() = Some(subscription);

        let close_self = log_window.clone();
        log_window.window.connect_close_request(move |_| {
            close_self.unsubscribe();
            glib::Propagation::Proceed
        });

        log_window
    }

    /// The underlying window widget.
    pub fn window(&self) -> &libadwaita::Window {
        &self.window
    }

    /// Present the window to the user.
    pub fn present(&self) {
        self.window.present();
    }

    /// Release the log subscription (also runs on close). Idempotent.
    pub fn unsubscribe(&self) {
        if let Some(mut subscription) = self.subscription.borrow_mut().take() {
            subscription.unsubscribe();
        }
    }
}

/// Append one log line to the text buffer, trim it and optionally scroll to
/// the end on the main loop.
fn append_line(
    buffer: &gtk4::TextBuffer,
    view: &gtk4::TextView,
    auto_scroll: &Cell<bool>,
    line: &str,
) {
    let mut end = buffer.end_iter();
    let prefix = if buffer.char_count() > 0 { "\n" } else { "" };
    buffer.insert(&mut end, &format!("{prefix}{line}"));
    trim_buffer(buffer);
    if auto_scroll.get() {
        let buffer = buffer.clone();
        let view = view.clone();
        glib::idle_add_local(move || {
            let mark = buffer.create_mark(None, &buffer.end_iter(), false);
            view.scroll_mark_onscreen(&mark);
            buffer.delete_mark(&mark);
            glib::ControlFlow::Break
        });
    }
}

/// Keep the text buffer under `MAX_BUFFER_LINES`, trimming from the top down to
/// `TRIM_TO_LINES`.
fn trim_buffer(buffer: &gtk4::TextBuffer) {
    let line_count = buffer.line_count();
    if line_count <= MAX_BUFFER_LINES {
        return;
    }
    let remove_lines = line_count - TRIM_TO_LINES;
    let mut start = buffer.start_iter();
    let mut end = buffer
        .iter_at_line(remove_lines)
        .unwrap_or_else(|| buffer.end_iter());
    buffer.delete(&mut start, &mut end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::log::LogBufferOptions;
    use crate::ui::test_helpers::gtk_smoke;

    #[test]
    fn log_window_smoke_builds_subscribes_and_appends() {
        gtk_smoke(|| {
            let directory = tempfile::tempdir().unwrap();
            let logger = LogBuffer::with_options(LogBufferOptions {
                directory: directory.path().to_path_buf(),
                ..Default::default()
            });
            let log_window = LogWindow::new(None::<&gtk4::Window>, &logger);
            assert_eq!(
                log_window.window().title().as_deref(),
                Some("Synchronization Log")
            );
            logger.append("hello from smoke");
            log_window.present();
            log_window.unsubscribe();
        });
    }
}
