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

use crate::core::account_runtime::{AccountManager, AccountRuntime, SchedulerFacade};
use crate::state::{AppState, StateSnapshot};
use crate::storage::config::{Config, ConfigStore};
use crate::ui::about;
use crate::ui::folder_status::{pair_folder_runtimes, FolderRowCallbacks, FolderStatusRow};
use crate::ui::settings::{
    page as settings_page, present_add_folder_dialog, ExclusionsDialog, SettingsCallbacks,
    SettingsHost, SettingsView,
};
use crate::util::i18n::t;

/// Translated window title (also used as a machine-readable page name).
pub fn window_title() -> &'static str {
    t("NextSync")
}

/// Translated window subtitle.
pub fn window_subtitle() -> &'static str {
    t("Nextcloud file synchronization")
}

/// Present the deletion-review dialog for a scheduler blocked on a deletion
/// alert. Keep Paused leaves the account blocked; Restore from Nextcloud
/// clears the alert and re-downloads the folder; Approve These Deletions Once
/// lets the run proceed for a single synchronization (the guard re-blocks if
/// the same mass deletion is still present afterwards).
fn present_delete_review(scheduler: &SchedulerFacade, parent: &gtk4::Widget) {
    let Some(alert) = scheduler.delete_alert() else {
        return;
    };
    let detail = if alert.missing_paths.is_empty() {
        alert.message.clone()
    } else {
        let sample: Vec<String> = alert.missing_paths.iter().take(5).cloned().collect();
        format!("{}\n\n{}", alert.message, sample.join("\n"))
    };
    let dialog = libadwaita::AlertDialog::new(Some(t("Review Deletions")), Some(&detail));
    dialog.add_response("keep_paused", t("Keep Paused"));
    dialog.add_response("restore", t("Restore from Nextcloud"));
    dialog.add_response("approve", t("Approve These Deletions Once"));
    dialog.set_response_appearance("approve", libadwaita::ResponseAppearance::Suggested);
    dialog.set_response_appearance("restore", libadwaita::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep_paused"));

    let scheduler_for_response = scheduler.clone();
    dialog.connect_response(None, move |_dialog, response| match response {
        "restore" => scheduler_for_response.restore_from_server(),
        "approve" => scheduler_for_response.approve_delete_once(),
        _ => {}
    });
    dialog.present(Some(parent));
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
    /// Invoked when the user clicks the in-view "Add Folder" row.
    pub on_add_folder: Option<Rc<dyn Fn()>>,
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
            // No last-sync caption is rendered (the v0.4.0 folder-focused
            // redesign dropped it), so no scheduler query here: the row's state
            // subscription fires synchronously from within SchedulerInner
            // borrows, and calling back into the scheduler from there would
            // panic on the already-borrowed RefCell (crash on DeleteReview).
            let format_last_sync: Option<Rc<dyn Fn() -> String>> = None;
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

        // "Add Folder" entry point directly from the account view, so the
        // user does not have to open Settings to add a folder. It opens the
        // same dialog the Settings Synchronization page uses.
        if let Some(on_add_folder) = &callbacks.on_add_folder {
            let add_row = libadwaita::ActionRow::builder()
                .title(t("Add Folder"))
                .subtitle(t("Mirror another local folder from this account"))
                .tooltip_text(t("Add a local folder to synchronize with this account"))
                .activatable(true)
                .build();
            let add_icon = gtk4::Image::builder()
                .icon_name("list-add-symbolic")
                .pixel_size(16)
                .build();
            add_row.add_prefix(&add_icon);
            let next = gtk4::Image::builder()
                .icon_name("go-next-symbolic")
                .pixel_size(16)
                .build();
            add_row.add_suffix(&next);
            let on_add_folder = on_add_folder.clone();
            add_row.connect_activated(move |_| {
                on_add_folder();
            });
            account_list.append(&add_row);
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
            .tooltip_text(t("Synchronize this account now"))
            .css_classes(["suggested-action", "pill"])
            .build();
        let runtime_for_sync = runtime.clone();
        sync_button.connect_clicked(move |button| {
            let scheduler = runtime_for_sync.scheduler();
            if scheduler.delete_alert().is_some() {
                present_delete_review(&scheduler, button.upcast_ref::<gtk4::Widget>());
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
            .tooltip_text(t("Pause or resume synchronization"))
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
    /// The in-app settings view (official-client style); `None` until opened.
    settings_view: Option<SettingsView>,
    /// Outer stack that slides the settings view over the sync view.
    root_stack: gtk4::Stack,
    setup_window: Option<crate::ui::setup::SetupWindow>,
    log_window: Option<crate::ui::log_view::LogWindow>,
    conflicts_window: Option<crate::ui::conflict_resolver::ConflictResolverWindow>,
    about_dialog: Option<libadwaita::AboutDialog>,
    checking_dialog: Option<libadwaita::Dialog>,
    update_result_dialog: Option<libadwaita::Dialog>,
    accounts_list: gtk4::ListBox,
    content_stack: gtk4::Stack,
    toast_overlay: libadwaita::ToastOverlay,
    account_rows: std::collections::HashMap<String, gtk4::ListBoxRow>,
    account_view: Option<AccountView>,
    settings_handler: SettingsHandler,
    add_account_handler: AddAccountHandler,
    self_weak: Weak<RefCell<MainWindow>>,
    _subscription: Option<crate::state::Subscription>,
    // Kept alive while the window exists.
    _sidebar_page: libadwaita::NavigationPage,
    _content_page: libadwaita::NavigationPage,
    // Kept alive for contract tests (the widget tree also owns a reference).
    _hamburger: gtk4::MenuButton,
}

impl MainWindow {
    /// Build the window. `account_manager` must already be started.
    ///
    /// `self_weak` must be the `Weak` pointing back to the `Rc<RefCell<MainWindow>>`
    /// that owns this window. It is captured eagerly by the per-account view
    /// (Add Folder row) constructed during `present_account`, so it has to be
    /// valid from the start; passing `Weak::new()` here would leave the Add
    /// Folder handler dead on first show.
    pub fn new(
        application: &libadwaita::Application,
        config: Config,
        config_store: ConfigStore,
        account_manager: AccountManager,
        logger: crate::core::log::LogBuffer,
        on_show_about: Option<Rc<dyn Fn()>>,
        self_weak: Weak<RefCell<MainWindow>>,
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

        // Hamburger menu (official-client style): Preferences, Advanced and
        // About, rendered in-app by sliding the settings view over the sync
        // view.
        let hamburger = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(t("Settings"))
            .css_classes(["flat"])
            .build();
        hamburger.set_menu_model(Some(&hamburger_menu_model()));
        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&{
            let weak = self_weak.clone();
            let action = gio::SimpleAction::new("sync", None);
            action.connect_activate(move |_action, _param| {
                if let Some(main) = weak.upgrade() {
                    main.borrow_mut().show_sync_view();
                }
            });
            action
        });
        actions.add_action(&{
            let weak = self_weak.clone();
            let action = gio::SimpleAction::new("preferences", None);
            action.connect_activate(move |_action, _param| {
                if let Some(main) = weak.upgrade() {
                    main.borrow_mut().show_preferences();
                }
            });
            action
        });
        actions.add_action(&{
            let weak = self_weak.clone();
            let action = gio::SimpleAction::new("advanced", None);
            action.connect_activate(move |_action, _param| {
                if let Some(main) = weak.upgrade() {
                    main.borrow_mut().show_advanced();
                }
            });
            action
        });
        actions.add_action(&{
            let on_about = on_show_about.clone();
            let action = gio::SimpleAction::new("about", None);
            action.connect_activate(move |_action, _param| {
                if let Some(cb) = &on_about {
                    cb();
                }
            });
            action
        });
        hamburger.insert_action_group("app", Some(&actions));
        header.pack_start(&hamburger);

        let log_button = gtk4::Button::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text(t("Synchronization Log"))
            .css_classes(["flat"])
            .build();
        let log_weak = self_weak.clone();
        log_button.connect_clicked(move |_button| {
            if let Some(main) = log_weak.upgrade() {
                main.borrow_mut().show_log();
            }
        });
        header.pack_end(&log_button);

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

        // Outer stack: the sync view (split) and, slid over it, the in-app
        // settings view (official-client style).
        let root_stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::SlideLeftRight)
            .build();
        root_stack.add_named(&split, Some("sync"));
        root_stack.set_visible_child_name("sync");

        toolbar.set_content(Some(&root_stack));
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

        let settings_handler: SettingsHandler = Rc::new(RefCell::new(None));
        let mut main = Self {
            window,
            application: application.clone(),
            account_manager,
            config,
            config_store,
            logger,
            active_account_id: None,
            settings_view: None,
            root_stack,
            setup_window: None,
            log_window: None,
            conflicts_window: None,
            about_dialog: None,
            checking_dialog: None,
            update_result_dialog: None,
            accounts_list,
            content_stack,
            toast_overlay,
            account_rows: std::collections::HashMap::new(),
            account_view: None,
            settings_handler,
            add_account_handler,
            self_weak,
            _subscription: None,
            _sidebar_page: sidebar_page,
            _content_page: content_page,
            _hamburger: hamburger.clone(),
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

    /// Wire the Settings header button to open this window's Preferences.
    ///
    /// Called once from the launcher once the window lives in a shared cell.
    pub fn install_settings_handler(&mut self, weak: Weak<RefCell<MainWindow>>) {
        self.self_weak = weak.clone();
        let handler = self.settings_handler.clone();
        *handler.borrow_mut() = Some(Box::new(move || {
            if let Some(main) = weak.upgrade() {
                main.borrow_mut().show_preferences();
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

    /// Open the in-app Preferences view (slides over the sync view), building
    /// it for the active account on first use.
    pub fn show_preferences(&mut self) {
        self.show_settings_page(settings_page::GENERAL);
    }

    /// Open the in-app Advanced page (slides over the sync view).
    pub fn show_advanced(&mut self) {
        self.show_settings_page(settings_page::ADVANCED);
    }

    /// Ensure the settings view exists for the active account and slide to the
    /// given page.
    fn show_settings_page(&mut self, page: &str) {
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
        if self.settings_view.is_none() {
            let host = SettingsHost::new(&self.window, &self.toast_overlay);
            let callbacks = self.build_settings_callbacks();
            let view = SettingsView::new(
                self.config_store.clone(),
                account,
                account_id,
                callbacks,
                &host,
            );
            self.root_stack.add_named(view.widget(), Some("settings"));
            self.settings_view = Some(view);
        }
        if let Some(view) = &self.settings_view {
            view.show_page(page);
            self.root_stack.set_visible_child_name("settings");
        }
    }

    /// Slide back to the synchronization view from the settings view.
    fn show_sync_view(&mut self) {
        self.root_stack.set_visible_child_name("sync");
    }

    /// Open the Add Folder dialog for the active account from the main window.
    /// Shares the same dialog the Settings Synchronization page uses; on
    /// success it refreshes the account view, and validation errors surface as
    /// a toast on the main window's overlay (in addition to the dialog's own
    /// inline re-present).
    pub fn show_add_folder_dialog(&mut self) {
        let Some(account_id) = self.active_account_id.clone() else {
            return;
        };
        let store = self.config_store.clone();
        let weak = self.self_weak.clone();
        let on_folder_added = Rc::new(move || {
            if let Some(main) = weak.upgrade() {
                main.borrow_mut().refresh_after_config_change();
            }
        });
        let overlay = self.toast_overlay.clone();
        let on_error = Rc::new(move |message: String| {
            overlay.add_toast(libadwaita::Toast::new(&message));
        });
        present_add_folder_dialog(
            store,
            account_id,
            self.window.upcast_ref::<gtk4::Widget>(),
            on_folder_added,
            on_error,
            None,
            None,
        );
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
        self.reset_settings_view();
        self.show_account(account_id.as_deref());
    }

    /// Drop the in-app settings view so the next open rebuilds it for the
    /// current account/configuration.
    fn reset_settings_view(&mut self) {
        if self.settings_view.take().is_some() {
            if let Some(child) = self.root_stack.child_by_name("settings") {
                self.root_stack.remove(&child);
            }
        }
        self.root_stack.set_visible_child_name("sync");
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
            on_remove_folder: {
                let weak = self.self_weak.clone();
                let store = self.config_store.clone();
                Some(Rc::new(move |account_id, folder_id| {
                    let _ = store.remove_folder(account_id, folder_id);
                    if let Some(main) = weak.upgrade() {
                        main.borrow_mut().refresh_after_config_change();
                    }
                }))
            },
            on_edit_ignored: {
                let weak = self.self_weak.clone();
                let store = self.config_store.clone();
                let account_id = account_id.to_string();
                let window = self.window.clone();
                Some(Rc::new(move || {
                    let callbacks = SettingsCallbacks {
                        on_reconfigure: {
                            let weak = weak.clone();
                            Some(Rc::new(move || {
                                if let Some(main) = weak.upgrade() {
                                    main.borrow_mut().reconfigure_active_account();
                                }
                            }))
                        },
                        ..SettingsCallbacks::default()
                    };
                    let dialog =
                        ExclusionsDialog::new(store.clone(), account_id.clone(), callbacks);
                    dialog.present(Some(window.upcast_ref::<gtk4::Widget>()));
                }))
            },
            on_add_folder: {
                let weak = self.self_weak.clone();
                Some(Rc::new(move || {
                    if let Some(main) = weak.upgrade() {
                        main.borrow_mut().show_add_folder_dialog();
                    }
                }))
            },
        };
        let view = AccountView::new(runtime, callbacks);
        self.content_stack.add_named(&view.root, Some("account"));
        self.content_stack.set_visible_child_name("account");
        self.account_view = Some(view);
    }
}

/// Build the hamburger menu model (official-client style): Synchronization
/// slides back to the sync view, Preferences/Advanced open the in-app
/// settings view over it and About keeps its dialog.
///
/// Extracted from [`MainWindow::new`] so the menu contract (sections, actions
/// and icons) is testable without a display.
fn hamburger_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();
    let sync_item = gio::MenuItem::new(Some(t("Synchronization")), Some("app.sync"));
    sync_item.set_icon(&gio::ThemedIcon::new("emblem-synchronizing-symbolic"));
    menu.append_item(&sync_item);
    let preferences_item = gio::MenuItem::new(Some(t("Preferences")), Some("app.preferences"));
    preferences_item.set_icon(&gio::ThemedIcon::new("preferences-system-symbolic"));
    menu.append_item(&preferences_item);
    let advanced_item = gio::MenuItem::new(Some(t("Advanced")), Some("app.advanced"));
    advanced_item.set_icon(&gio::ThemedIcon::new("applications-system-symbolic"));
    menu.append_item(&advanced_item);
    let about_item = gio::MenuItem::new(Some(t("About")), Some("app.about"));
    about_item.set_icon(&gio::ThemedIcon::new("nextsync-info-symbolic"));
    menu.append_item(&about_item);
    menu
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
        .tooltip_text(t("Add a new account"))
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
                Weak::new(),
            );
            assert_eq!(
                window.window().title().unwrap_or_default().to_string(),
                "NextSync"
            );
            reset_locale();
        });
    }

    /// A one-account configuration for the in-app settings contract tests.
    fn window_account() -> crate::storage::config::AccountConfig {
        crate::storage::config::AccountConfig {
            id: "acct-window-1".to_string(),
            server_url: "https://cloud.example.com".to_string(),
            login_name: "alice".to_string(),
            ..crate::storage::config::AccountConfig::default()
        }
    }

    #[test]
    fn hamburger_menu_offers_the_official_client_sections() {
        use gio::prelude::MenuModelExt;

        // Read one menu item's label/action/icon attributes.
        struct ItemAttrs {
            label: Option<String>,
            action: Option<String>,
            has_icon: bool,
        }

        fn item_attrs(menu: &gio::Menu, index: i32) -> ItemAttrs {
            let mut attrs = ItemAttrs {
                label: None,
                action: None,
                has_icon: false,
            };
            let iter = menu.iterate_item_attributes(index);
            while let Some((key, value)) = iter.next() {
                match key.as_str() {
                    "label" => attrs.label = value.str().map(str::to_string),
                    "action" => attrs.action = value.str().map(str::to_string),
                    "icon" => attrs.has_icon = true,
                    _ => {}
                }
            }
            attrs
        }

        set_locale(Locale::English);
        let menu = hamburger_menu_model();
        assert_eq!(menu.n_items(), 4);
        let expected: [(&str, &str); 4] = [
            ("Synchronization", "app.sync"),
            ("Preferences", "app.preferences"),
            ("Advanced", "app.advanced"),
            ("About", "app.about"),
        ];
        for (index, (label, action)) in expected.iter().enumerate() {
            let attrs = item_attrs(&menu, index as i32);
            assert_eq!(attrs.label.as_deref(), Some(*label), "item {index}");
            assert_eq!(attrs.action.as_deref(), Some(*action), "item {index}");
            assert!(attrs.has_icon, "item {index} must carry an icon");
        }

        // The Spanish catalog covers every menu section (issue #10 renders
        // the menu in-app, so the labels are user-visible on every launch).
        set_locale(Locale::Spanish);
        let menu = hamburger_menu_model();
        let labels: Vec<String> = (0..menu.n_items())
            .map(|index| item_attrs(&menu, index).label.expect("label"))
            .collect();
        assert_eq!(
            labels,
            vec![
                "Sincronización".to_string(),
                "Preferencias".to_string(),
                "Avanzado".to_string(),
                "Acerca de".to_string(),
            ]
        );
        reset_locale();
    }

    #[test]
    fn outer_stack_slides_the_settings_view_over_sync() {
        // Must run through the shared GTK test worker (see
        // `ui::test_helpers`): the assertions build the real window.
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let manager = AccountManager::new(std::rc::Rc::new(std::cell::RefCell::new(
                crate::core::debounce::FakeTimeoutSource::default(),
            )));
            let store = ConfigStore::with_path(
                std::env::temp_dir().join(format!("nextsync-stack-{}.json", std::process::id())),
            );
            let config = Config {
                accounts: vec![window_account()],
                ..Config::default()
            };
            let mut window = MainWindow::new(
                &app,
                config,
                store,
                manager,
                crate::core::log::LogBuffer::new(),
                None,
                Weak::new(),
            );

            // One window, one outer stack: the sync page only, until the
            // settings view slides over it (issue #10 acceptance).
            assert!(window.about_dialog.is_none());
            assert_eq!(
                window.root_stack.transition_type(),
                gtk4::StackTransitionType::SlideLeftRight
            );
            assert!(window.root_stack.child_by_name("sync").is_some());
            assert!(window.root_stack.child_by_name("settings").is_none());
            assert_eq!(
                window.root_stack.visible_child_name().as_deref(),
                Some("sync")
            );
            assert_eq!(
                window._hamburger.icon_name().as_deref(),
                Some("open-menu-symbolic")
            );

            // Preferences slides the in-app settings view in; the stack then
            // holds exactly the 'sync' and 'settings' pages.
            window.show_preferences();
            assert!(window.root_stack.child_by_name("settings").is_some());
            assert!(window.settings_view.is_some());
            assert_eq!(
                window.root_stack.visible_child_name().as_deref(),
                Some("settings")
            );
            window.show_advanced();
            assert_eq!(
                window.root_stack.visible_child_name().as_deref(),
                Some("settings")
            );

            // Synchronization slides back without dropping the view.
            window.show_sync_view();
            assert_eq!(
                window.root_stack.visible_child_name().as_deref(),
                Some("sync")
            );
            assert!(window.settings_view.is_some());

            // About keeps the dialog presentation (no in-app page for it).
            window.show_about();
            assert!(window.about_dialog.is_some());
            assert!(window.root_stack.child_by_name("settings").is_some());

            reset_locale();
        });
    }

    #[test]
    fn reset_settings_view_drops_the_view_on_account_change() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let manager = AccountManager::new(std::rc::Rc::new(std::cell::RefCell::new(
                crate::core::debounce::FakeTimeoutSource::default(),
            )));
            let store = ConfigStore::with_path(
                std::env::temp_dir().join(format!("nextsync-reset-{}.json", std::process::id())),
            );
            let config = Config {
                accounts: vec![window_account()],
                ..Config::default()
            };
            let mut window = MainWindow::new(
                &app,
                config,
                store,
                manager,
                crate::core::log::LogBuffer::new(),
                None,
                Weak::new(),
            );
            window.show_preferences();
            assert!(window.root_stack.child_by_name("settings").is_some());

            // Re-presenting an account drops the embedded view so a stale
            // account's settings can never come back (issue #10).
            window.present_account(None);
            assert!(window.settings_view.is_none());
            assert!(window.root_stack.child_by_name("settings").is_none());
            assert_eq!(
                window.root_stack.visible_child_name().as_deref(),
                Some("sync")
            );

            // Re-opening after the reset rebuilds the view cleanly.
            window.show_preferences();
            assert!(window.root_stack.child_by_name("settings").is_some());
            assert!(window.settings_view.is_some());
            reset_locale();
        });
    }

    #[test]
    fn show_settings_without_an_active_account_is_a_noop() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let manager = AccountManager::new(std::rc::Rc::new(std::cell::RefCell::new(
                crate::core::debounce::FakeTimeoutSource::default(),
            )));
            let store = ConfigStore::with_path(
                std::env::temp_dir().join(format!("nextsync-noop-{}.json", std::process::id())),
            );
            let mut window = MainWindow::new(
                &app,
                Config::default(),
                store,
                manager,
                crate::core::log::LogBuffer::new(),
                None,
                Weak::new(),
            );
            window.show_preferences();
            window.show_advanced();
            assert!(window.settings_view.is_none());
            assert!(window.root_stack.child_by_name("settings").is_none());
            assert_eq!(
                window.root_stack.visible_child_name().as_deref(),
                Some("sync")
            );
            reset_locale();
        });
    }
}
