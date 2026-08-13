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

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use libadwaita::prelude::*;

use crate::core::account_runtime::{AccountManager, AccountRuntime};
use crate::state::{AppState, StateSnapshot};
use crate::storage::config::{Config, ConfigStore};
use crate::ui::about;
use crate::ui::folder_status::{pair_folder_runtimes, FolderRowCallbacks, FolderStatusRow};
use crate::ui::settings::{SettingsCallbacks, SettingsWindow};
use crate::util::i18n::t;

/// Translated window title (also used as a machine-readable page name).
pub fn window_title() -> &'static str {
    t("NextSync")
}

/// Translated window subtitle.
pub fn window_subtitle() -> &'static str {
    t("Nextcloud file synchronization")
}

/// Callback invoked with the local root of a folder to open it in the file
/// manager.
pub type OpenFolderCallback = Rc<dyn Fn(&str)>;
/// Callback invoked with the account and folder ids to drop a sync folder.
pub type RemoveFolderCallback = Rc<dyn Fn(&str, &str)>;
/// Callback invoked when the user wants to edit ignored files.
pub type EditIgnoredCallback = Rc<dyn Fn()>;

/// Shared holder for the Settings header-button handler, installed after the
/// window lives in a shared cell.
pub type SettingsHandler = Rc<RefCell<Option<Box<dyn FnMut()>>>>;

/// Shared holder for the Add Account sidebar-button handler.
pub type AddAccountHandler = Rc<RefCell<Option<Box<dyn FnMut()>>>>;

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
                .title(t("No Synchronization Folders"))
                .subtitle(t("Add folders from Settings"))
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
            .label(t("Sync Now"))
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
            .label(t("Pause Sync"))
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
            pause_content.set_label(if paused {
                t("Resume Sync")
            } else {
                t("Pause Sync")
            });
            pause_content.set_icon_name(if paused {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            });
            match snapshot.state {
                AppState::DeleteReview => {
                    sync_content.set_label(t("Review Deletions"));
                    sync_content.set_icon_name("security-high-symbolic");
                }
                AppState::KeyringLocked => {
                    sync_content.set_label(t("Unlock Password Keyring"));
                    sync_content.set_icon_name("changes-prevent-symbolic");
                }
                _ => {
                    let once = matches!(
                        snapshot.state,
                        AppState::PausedUser | AppState::PausedBattery
                    );
                    sync_content.set_label(if once { t("Sync Once") } else { t("Sync Now") });
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
    application: libadwaita::Application,
    account_manager: AccountManager,
    config: Config,
    config_store: ConfigStore,
    logger: crate::core::log::LogBuffer,
    active_account_id: Option<String>,
    settings_window: Option<SettingsWindow>,
    setup_window: Option<crate::ui::setup::SetupWindow>,
    log_window: Option<crate::ui::log_view::LogWindow>,
    conflicts_window: Option<crate::ui::conflict_resolver::ConflictResolverWindow>,
    about_dialog: Option<libadwaita::AboutDialog>,
    checking_dialog: Option<libadwaita::Dialog>,
    update_result_dialog: Option<libadwaita::Dialog>,
    accounts_list: gtk4::ListBox,
    content_stack: gtk4::Stack,
    account_rows: std::collections::HashMap<String, gtk4::ListBoxRow>,
    account_view: Option<AccountView>,
    settings_handler: SettingsHandler,
    add_account_handler: AddAccountHandler,
    self_weak: Weak<RefCell<MainWindow>>,
    _subscription: Option<crate::state::Subscription>,
    // Kept alive while the window exists.
    _sidebar_page: libadwaita::NavigationPage,
    _content_page: libadwaita::NavigationPage,
}

impl MainWindow {
    /// Build the window. `account_manager` must already be started.
    pub fn new(
        application: &libadwaita::Application,
        config: Config,
        config_store: ConfigStore,
        account_manager: AccountManager,
        logger: crate::core::log::LogBuffer,
        on_show_about: Option<Rc<dyn Fn()>>,
    ) -> Self {
        let window = libadwaita::ApplicationWindow::builder()
            .application(application)
            .title(window_title())
            .default_width(900)
            .default_height(600)
            .build();

        let toolbar = libadwaita::ToolbarView::new();
        let header = gtk4::HeaderBar::new();
        let title = libadwaita::WindowTitle::new(window_title(), window_subtitle());
        header.set_title_widget(Some(&title));

        let settings_button = gtk4::Button::builder()
            .icon_name("nextsync-settings-2-symbolic")
            .tooltip_text(t("Settings"))
            .css_classes(["flat"])
            .build();
        // The handler is installed after construction (the window must exist
        // inside a shared cell first); until then the button is inert.
        let settings_handler: SettingsHandler = Rc::new(RefCell::new(None));
        let handler_for_button = settings_handler.clone();
        settings_button.connect_clicked(move |_button| {
            if let Some(handler) = handler_for_button.borrow_mut().as_mut() {
                handler();
            }
        });
        header.pack_end(&settings_button);

        let about_button = gtk4::Button::builder()
            .icon_name("nextsync-info-symbolic")
            .tooltip_text(t("About"))
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
        let sidebar_page = libadwaita::NavigationPage::new(&sidebar, t("Accounts"));
        let add_account_handler: AddAccountHandler = Rc::new(RefCell::new(None));
        let handler_for_add = add_account_handler.clone();
        add_button.connect_clicked(move |_button| {
            if let Some(handler) = handler_for_add.borrow_mut().as_mut() {
                handler();
            }
        });

        let content_stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .build();
        let empty_label = gtk4::Label::builder()
            .label(t("Select an account to see its synchronization"))
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
        let content_page = libadwaita::NavigationPage::new(&toast_overlay, window_title());

        split.set_sidebar(Some(&sidebar_page));
        split.set_content(Some(&content_page));
        toolbar.set_content(Some(&split));
        window.set_content(Some(&toolbar));

        // Close button quits the application outright. The StatusNotifier tray
        // runs on its own thread and would otherwise keep the process alive
        // after the last window is gone, leaving an invisible app with no way
        // back in. If a "minimize to tray" pattern is wanted later, this is
        // the single place to change.
        let app_for_close = application.clone();
        window.connect_close_request(move |_| {
            eprintln!("nextsync: main window close-request, quitting application");
            app_for_close.quit();
            glib::Propagation::Proceed
        });

        let mut main = Self {
            window,
            application: application.clone(),
            account_manager,
            config,
            config_store,
            logger,
            active_account_id: None,
            settings_window: None,
            setup_window: None,
            log_window: None,
            conflicts_window: None,
            about_dialog: None,
            checking_dialog: None,
            update_result_dialog: None,
            accounts_list,
            content_stack,
            account_rows: std::collections::HashMap::new(),
            account_view: None,
            settings_handler,
            add_account_handler,
            self_weak: Weak::new(),
            _subscription: None,
            _sidebar_page: sidebar_page,
            _content_page: content_page,
        };
        // Install the Settings handler: opening Settings needs the whole
        // window, so it is wired once the shared cell exists.
        let settings_handler = main.settings_handler.clone();
        *settings_handler.borrow_mut() = Some(Box::new(move || {
            // Replaced after construction via `install_settings_handler`.
        }));
        main.refresh_sidebar();
        main.present_account(None);
        main
    }

    /// The underlying window, for presentation and wiring.
    pub fn window(&self) -> &libadwaita::ApplicationWindow {
        &self.window
    }

    /// Wire the Settings header button to open this window's Settings.
    ///
    /// Called once from the launcher once the window lives in a shared cell.
    pub fn install_settings_handler(&mut self, weak: Weak<RefCell<MainWindow>>) {
        self.self_weak = weak.clone();
        let handler = self.settings_handler.clone();
        *handler.borrow_mut() = Some(Box::new(move || {
            if let Some(main) = weak.upgrade() {
                main.borrow_mut().show_settings();
            }
        }));
    }

    /// Wire the Add Account sidebar button to open the setup wizard.
    ///
    /// Called once from the launcher once the window lives in a shared cell.
    pub fn install_add_account_handler(&mut self, weak: Weak<RefCell<MainWindow>>) {
        let handler = self.add_account_handler.clone();
        *handler.borrow_mut() = Some(Box::new(move || {
            if let Some(main) = weak.upgrade() {
                main.borrow_mut().show_add_account();
            }
        }));
    }

    /// Open (or bring to front) the account setup wizard.
    /// Open (or bring to front) the live synchronization log window.
    pub fn show_log(&mut self) {
        if let Some(window) = &self.log_window {
            window.present();
            return;
        }
        let window = crate::ui::log_view::LogWindow::new(Some(&self.window), &self.logger);
        window.present();
        self.log_window = Some(window);
    }

    /// Open (or bring to front) the activity/conflicts window for the active
    /// account's first synchronized folder.
    pub fn show_conflicts(&mut self) {
        if let Some(window) = &self.conflicts_window {
            window.present();
            return;
        }
        let Some(account_id) = &self.active_account_id else {
            return;
        };
        let Some(account) = self
            .config
            .accounts
            .iter()
            .find(|account| &account.id == account_id)
        else {
            return;
        };
        let Some(folder) = account.folders.first() else {
            return;
        };
        let matcher = crate::core::exclusions::ExclusionMatcher::new(
            account.sync.exclude_patterns.clone(),
            account.sync.exclude_patterns_enabled,
        );
        let logger = self.logger.clone();
        let window = crate::ui::conflict_resolver::ConflictResolverWindow::new(
            &self.application,
            &folder.local_root,
            matcher,
            Rc::new(logger),
            None,
        );
        window.present();
        self.conflicts_window = Some(window);
    }

    /// Open (or bring to front) the About dialog. The dialog's
    /// "Check for Updates" link is wired back to [`Self::check_for_updates`]
    /// via this window's shared cell.
    pub fn show_about(&mut self) {
        if let Some(dialog) = &self.about_dialog {
            dialog.present(Some(&self.window));
            return;
        }
        let version = env!("CARGO_PKG_VERSION");
        let dialog = about::build_about_dialog(version);
        let weak = self.self_weak.clone();
        dialog.connect_activate_link(move |_dialog, uri| {
            if uri == about::CHECK_UPDATES_URI {
                if let Some(main) = weak.upgrade() {
                    // Defer so the About dialog can close first (the checker
                    // opens its own modal spinner over the main window).
                    glib::idle_add_local_once(move || {
                        main.borrow_mut().check_for_updates();
                    });
                }
                return true;
            }
            false
        });
        dialog.present(Some(&self.window));

        // Drop our reference when the dialog is dismissed.
        let weak = self.self_weak.clone();
        dialog.connect_closed(move |_| {
            if let Some(main) = weak.upgrade() {
                main.borrow_mut().about_dialog = None;
            }
        });
        self.about_dialog = Some(dialog);
    }

    /// Run the update check off the main thread, showing a spinner dialog
    /// while the synchronous [`about::run_update_check`] runs on the Gio
    /// blocking pool, then present the result.
    pub fn check_for_updates(&mut self) {
        if self.checking_dialog.is_some() {
            // A check is already running; ignore the second click.
            return;
        }
        let version = env!("CARGO_PKG_VERSION").to_string();
        let checking = about::build_checking_dialog();
        checking.present(Some(&self.window));

        let weak = self.self_weak.clone();
        checking.connect_closed(move |_| {
            if let Some(main) = weak.upgrade() {
                main.borrow_mut().checking_dialog = None;
            }
        });
        self.checking_dialog = Some(checking);

        let weak = self.self_weak.clone();
        glib::spawn_future_local(async move {
            // Build the checker *inside* the blocking closure: the shared
            // `HttpClient` is not `Send`, so the checker never crosses the
            // thread boundary — only the version string does.
            let handle = gio::spawn_blocking(move || about::run_update_check(&version));
            match handle.await {
                Ok(result) => {
                    if let Some(main) = weak.upgrade() {
                        main.borrow_mut().finish_update_check(result);
                    }
                }
                Err(_panic) => {
                    if let Some(main) = weak.upgrade() {
                        main.borrow_mut().finish_update_check(
                            crate::core::updates::UpdateCheckResult {
                                error: Some(
                                    t("The version information could not be obtained. Check your connection and try again later.")
                                        .to_string(),
                                ),
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        });
    }

    /// Replace the spinner with the result dialog.
    fn finish_update_check(&mut self, result: crate::core::updates::UpdateCheckResult) {
        if let Some(error) = &result.error {
            self.logger.append(&format!("update check failed: {error}"));
        }
        // Close the spinner first.
        if let Some(checking) = self.checking_dialog.take() {
            checking.force_close();
        }
        let outcome = about::classify_update_result(&result);
        let dialog = about::build_update_result_dialog(&outcome, env!("CARGO_PKG_VERSION"));
        dialog.present(Some(&self.window));

        let weak = self.self_weak.clone();
        dialog.connect_closed(move |_| {
            if let Some(main) = weak.upgrade() {
                main.borrow_mut().update_result_dialog = None;
            }
        });
        self.update_result_dialog = Some(dialog);
    }

    pub fn show_add_account(&mut self) {
        if let Some(window) = &self.setup_window {
            window.present();
            return;
        }
        let callbacks = crate::ui::setup::SetupCallbacks {
            on_complete: Some(Rc::new({
                let weak = self.self_weak.clone();
                move |account: crate::storage::config::AccountConfig| {
                    if let Some(main) = weak.upgrade() {
                        let mut main = main.borrow_mut();
                        main.account_manager.ensure_account_runtime(account);
                        main.refresh_after_config_change();
                    }
                }
            })),
        };
        let window = crate::ui::setup::SetupWindow::new(
            &self.application,
            self.config_store.clone(),
            callbacks,
        );
        window.present();
        self.setup_window = Some(window);
    }

    /// Open (or bring to front) the Settings window for the active account.
    pub fn show_settings(&mut self) {
        let Some(account_id) = self.active_account_id.clone() else {
            return;
        };
        let Some(account) = self
            .config
            .accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned()
        else {
            return;
        };
        if let Some(window) = &self.settings_window {
            window.window().present();
            return;
        }
        let callbacks = self.build_settings_callbacks();
        let window = SettingsWindow::new(self.config_store.clone(), account, account_id, callbacks);
        window.window().present();
        self.settings_window = Some(window);
    }

    /// Re-read the configuration, refresh the sidebar and re-present the
    /// active account (called after Settings mutates folders).
    fn refresh_after_config_change(&mut self) {
        self.config = self.config_store.load().unwrap_or_default();
        self.refresh_sidebar();
        let account_id = self.active_account_id.clone();
        self.present_account(account_id.as_deref());
    }

    /// Reconcile the active account runtimes with the current configuration.
    fn reconfigure_active_account(&mut self) {
        self.config = self.config_store.load().unwrap_or_default();
        if let Some(account_id) = &self.active_account_id {
            if let Some(account) = self
                .config
                .accounts
                .iter()
                .find(|account| &account.id == account_id)
                .cloned()
            {
                self.account_manager.sync_folders(&account);
            }
        }
    }

    /// Build the Settings callbacks against this window's shared cell.
    fn build_settings_callbacks(&mut self) -> SettingsCallbacks {
        let weak = self.self_weak.clone();
        SettingsCallbacks {
            on_folder_changed: {
                let weak = weak.clone();
                Some(Rc::new(move || {
                    if let Some(main) = weak.upgrade() {
                        main.borrow_mut().refresh_after_config_change();
                    }
                }))
            },
            on_reconfigure: {
                let weak = weak.clone();
                Some(Rc::new(move || {
                    if let Some(main) = weak.upgrade() {
                        main.borrow_mut().reconfigure_active_account();
                    }
                }))
            },
            on_remove_account: {
                let weak = weak.clone();
                Some(Rc::new(move || {
                    if let Some(main) = weak.upgrade() {
                        main.borrow_mut().remove_active_account();
                    }
                }))
            },
        }
    }

    /// Remove the active account (config + runtime), then refresh the window.
    fn remove_active_account(&mut self) {
        let Some(account_id) = self.active_account_id.clone() else {
            return;
        };
        let _ = self.config_store.remove_account(&account_id);
        let _ = self.account_manager.remove(&account_id);
        self.refresh_after_config_change();
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
        self.active_account_id = account_id.clone();
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
        .label(t("Accounts"))
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
        .label(t("Add Account"))
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
    use crate::util::i18n::{reset_locale, set_locale, Locale};

    #[test]
    fn window_constants_are_stable() {
        set_locale(Locale::English);
        assert_eq!(window_title(), "NextSync");
        assert_eq!(window_subtitle(), "Nextcloud file synchronization");
        reset_locale();
    }

    #[test]
    fn window_subtitle_translates_to_spanish() {
        set_locale(Locale::Spanish);
        assert_eq!(window_title(), "NextSync");
        assert_eq!(window_subtitle(), "Sincronización de archivos de Nextcloud");
        reset_locale();
    }

    #[test]
    fn main_window_construction_smoke() {
        // Must run through the shared GTK test worker: a second `gtk4::init()`
        // on a separate test thread panics (see `ui::test_helpers`).
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let manager = AccountManager::new(std::rc::Rc::new(std::cell::RefCell::new(
                crate::core::debounce::FakeTimeoutSource::default(),
            )));
            let store = ConfigStore::with_path(std::env::temp_dir().join("nextsync-smoke.json"));
            let window = MainWindow::new(
                &app,
                Config::default(),
                store,
                manager,
                crate::core::log::LogBuffer::new(),
                None,
            );
            assert_eq!(
                window.window().title().unwrap_or_default().to_string(),
                "NextSync"
            );
            reset_locale();
        });
    }
}
