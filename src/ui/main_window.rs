//! Main application window with the account sidebar and folder-focused views.
//!
//! Fase 5 (Task 5.1): mirrors `ui/main_window.py` (v0.4.0 redesign, issue
//! #35). A `NavigationSplitView` shows the account list on the left and the
//! selected account's view on the right. The account view is focused on the
//! synchronized folders — one `FolderStatusRow` per folder with its live
//! status and a more (…) menu — plus the global Sync Now / Pause buttons.
//! Account management lives in Settings.
//!
//! Header buttons use the Lucide `settings-2` / `info` symbolic icons
//! (issue #21).

use std::rc::Rc;

use libadwaita::prelude::*;

use crate::core::account_runtime::{AccountManager, AccountRuntime};
use crate::state::{AppState, StateSnapshot};
use crate::storage::config::Config;
use crate::ui::folder_status::{pair_folder_runtimes, FolderRowCallbacks, FolderStatusRow};

/// Stable machine-readable name used as a window hint (no GTK dependency).
pub const WINDOW_TITLE: &str = "NextSync";
pub const WINDOW_SUBTITLE: &str = "Nextcloud file synchronization";

/// Callback invoked with the local root of a folder to open it in the file
/// manager.
pub type OpenFolderCallback = Rc<dyn Fn(&str)>;
/// Callback invoked with the account and folder ids to drop a sync folder.
pub type RemoveFolderCallback = Rc<dyn Fn(&str, &str)>;
/// Callback invoked when the user wants to edit ignored files.
pub type EditIgnoredCallback = Rc<dyn Fn()>;

/// Folder row callbacks as plain functions the view can invoke.
pub struct AccountCallbacks {
    pub on_open_folder: Option<OpenFolderCallback>,
    pub on_remove_folder: Option<RemoveFolderCallback>,
    pub on_edit_ignored: Option<EditIgnoredCallback>,
}

/// One account rendered as the content of the split view: a list of folder
/// rows plus the Sync Now / Pause buttons.
pub struct AccountView {
    pub root: gtk4::Box,
    _account_runtime: AccountRuntime,
    _subscription: Option<crate::state::Subscription>,
}

impl AccountView {
    /// Build the folder-focused view for one account.
    pub fn new(account_runtime: AccountRuntime, callbacks: AccountCallbacks) -> Self {
        let account = account_runtime.account.clone();
        let runtime = account_runtime.clone();

        let root = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(12)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(18)
            .margin_end(18)
            .build();

        let account_list = gtk4::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk4::SelectionMode::None)
            .build();

        let pairs = pair_folder_runtimes(&account.folders, runtime.folders());
        for (folder, folder_runtime) in pairs {
            let local_root = folder.local_root.clone();
            let account_id = account.id.clone();
            let row_callbacks = FolderRowCallbacks {
                on_open: {
                    let cb = callbacks.on_open_folder.clone();
                    let local_root = local_root.clone();
                    Some(Rc::new(move || {
                        if let Some(cb) = &cb {
                            cb(&local_root);
                        }
                    }))
                },
                on_edit_ignored: callbacks.on_edit_ignored.clone(),
                on_force_sync: {
                    let folder_runtime = folder_runtime.clone();
                    Some(Rc::new(move || {
                        if let Some(fr) = &folder_runtime {
                            fr.sync_now();
                        }
                    }))
                },
                on_toggle_pause: {
                    let folder_runtime = folder_runtime.clone();
                    Some(Rc::new(move || {
                        if let Some(fr) = &folder_runtime {
                            fr.set_paused(!fr.user_paused());
                        }
                    }))
                },
                on_remove: {
                    let cb = callbacks.on_remove_folder.clone();
                    let folder_id = folder.id.clone();
                    let account_id = account_id.clone();
                    Some(Rc::new(move || {
                        if let Some(cb) = &cb {
                            cb(&account_id, &folder_id);
                        }
                    }))
                },
            };
            let format_last_sync: Option<Rc<dyn Fn() -> String>> = {
                let runtime = folder_runtime.clone();
                Some(Rc::new(move || {
                    let value = runtime
                        .as_ref()
                        .and_then(|fr| fr.scheduler().delete_alert().map(|_| String::new()))
                        .unwrap_or_default();
                    let _ = value;
                    String::new()
                }))
            };
            let is_paused: Option<Rc<dyn Fn() -> bool>> = {
                let folder_runtime = folder_runtime.clone();
                Some(Rc::new(move || {
                    folder_runtime
                        .as_ref()
                        .map(|fr| fr.user_paused())
                        .unwrap_or(false)
                }))
            };
            let state = folder_runtime.as_ref().map(|fr| fr.state());
            let row =
                FolderStatusRow::new(folder, state, row_callbacks, format_last_sync, is_paused);
            account_list.append(&row.row);
        }
        if account.folders.is_empty() {
            let row = libadwaita::ActionRow::builder()
                .title("No Synchronization Folders")
                .subtitle("Add folders from Settings")
                .build();
            let icon = gtk4::Image::builder()
                .icon_name("folder-symbolic")
                .pixel_size(16)
                .build();
            row.add_prefix(&icon);
            account_list.append(&row);
        }
        root.append(&account_list);

        // Sync Now / Pause buttons.
        let buttons = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(12)
            .homogeneous(true)
            .build();

        let sync_content = libadwaita::ButtonContent::builder()
            .label("Sync Now")
            .icon_name("emblem-synchronizing-symbolic")
            .build();
        let sync_button = gtk4::Button::builder()
            .child(&sync_content)
            .css_classes(["suggested-action", "pill"])
            .build();
        let runtime_for_sync = runtime.clone();
        sync_button.connect_clicked(move |_button| {
            let scheduler = runtime_for_sync.scheduler();
            if scheduler.delete_alert().is_some() {
                // Review handled at the application level; a no-op here.
                return;
            }
            scheduler.sync_now();
        });
        buttons.append(&sync_button);

        let pause_content = libadwaita::ButtonContent::builder()
            .label("Pause Sync")
            .icon_name("media-playback-pause-symbolic")
            .build();
        let pause_button = gtk4::Button::builder()
            .child(&pause_content)
            .css_classes(["pill"])
            .build();
        let runtime_for_pause = runtime.clone();
        pause_button.connect_clicked(move |_button| {
            let scheduler = runtime_for_pause.scheduler();
            scheduler.set_paused(!scheduler.user_paused());
        });
        buttons.append(&pause_button);
        root.append(&buttons);

        // Live update of the Sync / Pause button labels from the account state.
        let aggregate = runtime.state();
        let sync_content = sync_content.clone();
        let pause_content = pause_content.clone();
        let subscription = aggregate.subscribe(move |snapshot: &StateSnapshot| {
            let paused = snapshot.state == AppState::PausedUser;
            pause_content.set_label(if paused { "Resume Sync" } else { "Pause Sync" });
            pause_content.set_icon_name(if paused {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            });
            match snapshot.state {
                AppState::DeleteReview => {
                    sync_content.set_label("Review Deletions");
                    sync_content.set_icon_name("security-high-symbolic");
                }
                AppState::KeyringLocked => {
                    sync_content.set_label("Unlock Password Keyring");
                    sync_content.set_icon_name("changes-prevent-symbolic");
                }
                _ => {
                    let once = matches!(
                        snapshot.state,
                        AppState::PausedUser | AppState::PausedBattery
                    );
                    sync_content.set_label(if once { "Sync Once" } else { "Sync Now" });
                    sync_content.set_icon_name("emblem-synchronizing-symbolic");
                }
            }
        });

        Self {
            root,
            _account_runtime: account_runtime,
            _subscription: Some(subscription),
        }
    }
}

/// The main application window.
pub struct MainWindow {
    window: libadwaita::ApplicationWindow,
    account_manager: AccountManager,
    config: Config,
    accounts_list: gtk4::ListBox,
    content_stack: gtk4::Stack,
    account_rows: std::collections::HashMap<String, gtk4::ListBoxRow>,
    account_view: Option<AccountView>,
    _subscription: Option<crate::state::Subscription>,
    // Kept alive while the window exists.
    _sidebar_page: libadwaita::NavigationPage,
    _content_page: libadwaita::NavigationPage,
}

impl MainWindow {
    /// Build the window. `account_manager` must already be started.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        application: &libadwaita::Application,
        config: Config,
        account_manager: AccountManager,
        on_add_account: Option<Rc<dyn Fn()>>,
        on_open_settings: Option<Rc<dyn Fn()>>,
        on_show_about: Option<Rc<dyn Fn()>>,
    ) -> Self {
        let window = libadwaita::ApplicationWindow::builder()
            .application(application)
            .title(WINDOW_TITLE)
            .default_width(900)
            .default_height(600)
            .build();

        let toolbar = libadwaita::ToolbarView::new();
        let header = gtk4::HeaderBar::new();
        let title = libadwaita::WindowTitle::new(WINDOW_TITLE, WINDOW_SUBTITLE);
        header.set_title_widget(Some(&title));

        let settings_button = gtk4::Button::builder()
            .icon_name("nextsync-settings-2-symbolic")
            .tooltip_text("Settings")
            .css_classes(["flat"])
            .build();
        let settings_cb = on_open_settings.clone();
        settings_button.connect_clicked(move |_button| {
            if let Some(cb) = &settings_cb {
                cb();
            }
        });
        header.pack_end(&settings_button);

        let about_button = gtk4::Button::builder()
            .icon_name("nextsync-info-symbolic")
            .tooltip_text("About")
            .css_classes(["flat"])
            .build();
        let about_cb = on_show_about.clone();
        about_button.connect_clicked(move |_button| {
            if let Some(cb) = &about_cb {
                cb();
            }
        });
        header.pack_end(&about_button);
        toolbar.add_top_bar(&header);

        let toast_overlay = libadwaita::ToastOverlay::new();

        let split = libadwaita::NavigationSplitView::new();
        split.set_collapsed(false);
        split.set_sidebar_width_fraction(0.28);
        split.set_min_sidebar_width(220.0);

        let (sidebar, accounts_list, add_button) = build_sidebar();
        let sidebar_page = libadwaita::NavigationPage::new(&sidebar, "Accounts");
        let on_add_account = on_add_account.clone();
        add_button.connect_clicked(move |_button| {
            if let Some(cb) = &on_add_account {
                cb();
            }
        });

        let content_stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .build();
        let empty_label = gtk4::Label::builder()
            .label("Select an account to see its synchronization")
            .css_classes(["dim-label"])
            .build();
        content_stack.add_named(&empty_label, Some("empty"));
        content_stack.set_visible_child_name("empty");
        let scroller = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .build();
        let clamp = libadwaita::Clamp::builder()
            .maximum_size(660)
            .tightening_threshold(500)
            .build();
        clamp.set_child(Some(&content_stack));
        scroller.set_child(Some(&clamp));
        toast_overlay.set_child(Some(&scroller));
        let content_page = libadwaita::NavigationPage::new(&toast_overlay, WINDOW_TITLE);

        split.set_sidebar(Some(&sidebar_page));
        split.set_content(Some(&content_page));
        toolbar.set_content(Some(&split));
        window.set_child(Some(&toolbar));

        let mut main = Self {
            window,
            account_manager,
            config,
            accounts_list,
            content_stack,
            account_rows: std::collections::HashMap::new(),
            account_view: None,
            _subscription: None,
            _sidebar_page: sidebar_page,
            _content_page: content_page,
        };
        main.refresh_sidebar();
        main.present_account(None);
        main
    }

    /// The underlying window, for presentation and wiring.
    pub fn window(&self) -> &libadwaita::ApplicationWindow {
        &self.window
    }

    /// Refresh the account sidebar from the current configuration.
    pub fn refresh_sidebar(&mut self) {
        self.accounts_list.remove_all();
        self.account_rows.clear();
        for account in &self.config.accounts {
            let row = gtk4::ListBoxRow::builder()
                .activatable(true)
                .selectable(true)
                .build();
            let box_container = gtk4::Box::builder()
                .orientation(gtk4::Orientation::Horizontal)
                .spacing(10)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(8)
                .margin_end(8)
                .build();
            let avatar = gtk4::Image::builder()
                .icon_name("avatar-default-symbolic")
                .pixel_size(28)
                .build();
            box_container.append(&avatar);
            let text = gtk4::Box::builder()
                .orientation(gtk4::Orientation::Vertical)
                .spacing(1)
                .build();
            let name = gtk4::Label::builder()
                .label(&account.login_name)
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .build();
            let server = gtk4::Label::builder()
                .label(&account.server_url)
                .xalign(0.0)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .css_classes(["dim-label"])
                .build();
            text.append(&name);
            text.append(&server);
            box_container.append(&text);
            row.set_child(Some(&box_container));
            let account_id = account.id.clone();
            self.accounts_list.append(&row);
            self.account_rows.insert(account_id, row);
        }
    }

    /// Present the account with the given id (or the first one when `None`).
    pub fn present_account(&mut self, account_id: Option<&str>) {
        let account_id = match account_id {
            Some(id) if self.account_rows.contains_key(id) => Some(id.to_string()),
            _ => self
                .config
                .accounts
                .first()
                .map(|account| account.id.clone()),
        };
        self.show_account(account_id.as_deref());
    }

    fn show_account(&mut self, account_id: Option<&str>) {
        if let Some(mut view) = self.account_view.take() {
            if let Some(mut sub) = view._subscription.take() {
                sub.unsubscribe();
            }
            self.content_stack.remove(&view.root);
        }
        let Some(account_id) = account_id else {
            self.content_stack.set_visible_child_name("empty");
            return;
        };
        let Some(runtime) = self.account_manager.get(account_id) else {
            self.content_stack.set_visible_child_name("empty");
            return;
        };
        let callbacks = AccountCallbacks {
            on_open_folder: Some(Rc::new(|local_root| {
                let _ = gio::AppInfo::launch_default_for_uri(
                    &format!("file://{local_root}"),
                    None::<&gio::AppLaunchContext>,
                );
            })),
            on_remove_folder: None,
            on_edit_ignored: None,
        };
        let view = AccountView::new(runtime, callbacks);
        self.content_stack.add_named(&view.root, Some("account"));
        self.content_stack.set_visible_child_name("account");
        self.account_view = Some(view);
    }
}

/// Build the sidebar: the container, the accounts list and the Add Account
/// button.
fn build_sidebar() -> (gtk4::Box, gtk4::ListBox, gtk4::Button) {
    let sidebar = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(6)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(6)
        .margin_end(6)
        .build();

    let label = gtk4::Label::builder()
        .label("Accounts")
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_start(8)
        .margin_bottom(4)
        .build();
    sidebar.append(&label);

    let accounts_list = gtk4::ListBox::builder()
        .css_classes(["boxed-list", "navigation-sidebar"])
        .selection_mode(gtk4::SelectionMode::Single)
        .build();
    sidebar.append(&accounts_list);

    let add_button = gtk4::Button::builder()
        .label("Add Account")
        .icon_name("list-add-symbolic")
        .halign(gtk4::Align::Fill)
        .css_classes(["flat"])
        .build();
    sidebar.append(&add_button);
    (sidebar, accounts_list, add_button)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_constants_are_stable() {
        assert_eq!(WINDOW_TITLE, "NextSync");
        assert_eq!(WINDOW_SUBTITLE, "Nextcloud file synchronization");
    }

    #[test]
    fn main_window_construction_smoke() {
        if gtk4::init().is_err() {
            eprintln!("skipped: no display available");
            return;
        }
        let app = libadwaita::Application::builder()
            .application_id("io.github.gnacho.nextsync")
            .build();
        let manager = AccountManager::new(std::rc::Rc::new(std::cell::RefCell::new(
            crate::core::debounce::FakeTimeoutSource::default(),
        )));
        let window = MainWindow::new(&app, Config::default(), manager, None, None, None);
        assert_eq!(
            window.window().title().unwrap_or_default().to_string(),
            "NextSync"
        );
    }
}
