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

use std::cell::{Cell, RefCell};
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

/// What a main-window close-request should do.
///
/// With a StatusNotifier tray registered the close hides the window and keeps
/// the app alive in the background (minimize to tray, issue #34); without one
/// the close is the only way out and quits the application. The launcher
/// drives this via [`MainWindow::set_tray_active`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Hide the window, keep the app running in the tray.
    Hide,
    /// Quit the application outright.
    Quit,
}

/// Decide the close action from whether a tray is registered.
pub fn close_action(tray_active: bool) -> CloseAction {
    if tray_active {
        CloseAction::Hide
    } else {
        CloseAction::Quit
    }
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

        // Account summary card: login@server, storage used and an all-ok
        // light driven by the aggregate state. The quota fetch runs off the
        // UI thread; on any failure the card simply shows no usage line.
        let summary_row = libadwaita::ActionRow::builder()
            .title(format!(
                "{}@{}",
                account.login_name,
                server_host(&account.server_url)
            ))
            .build();
        let light = gtk4::Image::builder().pixel_size(14).build();
        light.set_icon_name(Some(summary_light_for(runtime.state().snapshot().state)));
        summary_row.add_prefix(&light);
        let usage_label = gtk4::Label::builder()
            .css_classes(["caption"])
            .halign(gtk4::Align::End)
            .build();
        summary_row.add_suffix(&usage_label);
        account_list.append(&summary_row);
        {
            // Quota fetch: only plain data crosses to the blocking thread;
            // widget clones are captured in the main-loop continuation.
            let account_for_quota = account.clone();
            let handle = gio::spawn_blocking(move || {
                crate::nextcloud::credentials::CredentialsStore::get_for_account(
                    &account_for_quota.id,
                    &account_for_quota.server_url,
                    &account_for_quota.login_name,
                )
                .ok()
                .flatten()
                .and_then(|password| {
                    crate::nextcloud::api::NextcloudApi::new()
                        .account_summary(
                            &account_for_quota.server_url,
                            &account_for_quota.login_name,
                            &password,
                        )
                        .ok()
                })
            });
            let usage_label = usage_label.clone();
            let summary_row = summary_row.clone();
            let title_for_check = summary_row.title().to_string();
            glib::spawn_future_local(async move {
                let Ok(Some(summary)) = handle.await else {
                    return;
                };
                // The view may have been rebuilt for another account while
                // the fetch ran; a detached row keeps its title, so compare
                // against the one captured at fetch start.
                if summary_row.title() != title_for_check {
                    return;
                }
                let mut parts = Vec::new();
                if let Some(name) = summary.display_name.clone() {
                    if !name.is_empty() {
                        parts.push(name);
                    }
                }
                let usage = summary.usage_label();
                if !usage.is_empty() {
                    parts.push(usage);
                }
                usage_label.set_text(&parts.join(" · "));
            });
        }

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
    /// Whether a StatusNotifier tray is registered. When `true`, closing the
    /// main window hides it instead of quitting (minimize to tray); the tray
    /// Quit item is the only way to fully exit (issue #34). The launcher sets
    /// it once the tray is up; without a tray the close keeps quitting.
    tray_active: Rc<Cell<bool>>,
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

        // gnome-text-editor layout: the back-to-sync button sits at the left
        // of the header; the hamburger menu sits at the far right, next to
        // the window close button.
        let back_button = gtk4::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text(t("Synchronization"))
            .css_classes(["flat"])
            .build();
        let back_weak = self_weak.clone();
        back_button.connect_clicked(move |_button| {
            if let Some(main) = back_weak.upgrade() {
                main.borrow_mut().show_sync_view();
            }
        });
        header.pack_start(&back_button);

        let hamburger = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text(t("Settings"))
            .css_classes(["flat"])
            .build();
        // gnome-text-editor pattern: a popover whose first section is the
        // custom CSS-circle theme selector, followed by the regular items.
        let popover = gtk4::PopoverMenu::builder()
            .menu_model(&hamburger_menu_model())
            .halign(gtk4::Align::Start)
            .build();
        {
            let weak = self_weak.clone();
            let store_for_scheme = config_store.clone();
            let selector = ThemeSelector::new(&config.general.color_scheme, move |scheme| {
                libadwaita::StyleManager::default().set_color_scheme(color_scheme_for(scheme));
                if let Some(main) = weak.upgrade() {
                    let mut main = main.borrow_mut();
                    main.config.general.color_scheme = scheme.to_string();
                    let mut persisted = store_for_scheme.load().unwrap_or_default();
                    persisted.general.color_scheme = scheme.to_string();
                    let _ = store_for_scheme.save(&persisted);
                }
            });
            popover.add_child(&selector.widget, "theme");
        }
        hamburger.set_popover(Some(&popover));
        let actions = gio::SimpleActionGroup::new();
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
        header.pack_end(&hamburger);

        toolbar.add_top_bar(&header);

        let toast_overlay = libadwaita::ToastOverlay::new();

        let split = libadwaita::NavigationSplitView::new();
        split.set_collapsed(false);
        split.set_sidebar_width_fraction(0.28);
        split.set_min_sidebar_width(220.0);

        let (sidebar, accounts_list, add_button) = build_sidebar();
        // Activating a sidebar row presents that account's sync view
        // (issue #49): the handler resolves the row back to its account id
        // through the id -> row map kept by `refresh_sidebar`.
        let weak = self_weak.clone();
        accounts_list.connect_row_activated(move |_list, row| {
            if let Some(main) = weak.upgrade() {
                let mut main = main.borrow_mut();
                if let Some(account_id) =
                    account_id_for_row(&main.account_rows, row).map(str::to_string)
                {
                    main.present_account(Some(&account_id));
                }
            }
        });
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

        // Close button minimizes to tray when a StatusNotifier tray is
        // registered (the tray Quit item is the only full exit); without a
        // tray the close keeps quitting the application. `tray_active` is a
        // shared cell the launcher flips once the tray is up, so the window
        // close handler always knows the real state.
        let tray_active = Rc::new(Cell::new(false));
        let tray_active_for_close = tray_active.clone();
        let app_for_close = application.clone();
        window.connect_close_request(move |window| {
            match close_action(tray_active_for_close.get()) {
                CloseAction::Hide => {
                    window.set_visible(false);
                    glib::Propagation::Stop
                }
                CloseAction::Quit => {
                    eprintln!("nextsync: main window close-request, quitting application");
                    app_for_close.quit();
                    glib::Propagation::Proceed
                }
            }
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
            tray_active,
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

    /// Mark the StatusNotifier tray as registered (or not).
    ///
    /// Called by the launcher right after a successful tray registration. With
    /// a tray, closing the main window hides it (minimize to tray) and the
    /// tray Quit item becomes the only way to exit; without one, closing the
    /// window keeps quitting the application (issue #34).
    pub fn set_tray_active(&self, active: bool) {
        self.tray_active.set(active);
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
        // Reconcile the folder runtimes first: the sync view reads the
        // runtimes (not the config), so an add/remove made in Settings would
        // otherwise stay invisible until restart.
        for account in self.config.accounts.clone() {
            self.account_manager.sync_folders(&account);
        }
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
    ///
    /// Issue #28: the server-side app password is revoked best-effort and the
    /// saved secret is removed from the keyring regardless of the outcome,
    /// mirroring `application.py`'s `remove_account`. The network call and the
    /// keyring writes run off the UI thread so an unreachable server never
    /// blocks the local removal; failures are only logged.
    fn remove_active_account(&mut self) {
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
            let _ = self.config_store.remove_account(&account_id);
            let _ = self.account_manager.remove(&account_id);
            self.refresh_after_config_change();
            return;
        };
        let _ = self.config_store.remove_account(&account_id);
        let _ = self.account_manager.remove(&account_id);
        self.refresh_after_config_change();

        // Best-effort cleanup off the UI thread: resolve the stored password,
        // revoke it on the server, then always drop the keyring entry. The
        // logger is only touched from the main thread.
        let logger = self.logger.clone();
        let task = gio::spawn_blocking(move || {
            let revoke_error = crate::nextcloud::credentials::CredentialsStore::get_for_account(
                &account.id,
                &account.server_url,
                &account.login_name,
            )
            .ok()
            .flatten()
            .and_then(|password| {
                crate::nextcloud::api::NextcloudApi::new()
                    .revoke_app_password(&account.server_url, &account.login_name, &password)
                    .err()
            });
            let delete_error =
                crate::nextcloud::credentials::CredentialsStore::delete(&account.id).err();
            (revoke_error, delete_error)
        });
        glib::spawn_future_local(async move {
            if let Ok((revoke_error, delete_error)) = task.await {
                if let Some(error) = revoke_error {
                    logger.append(&format!(
                        "account removal: could not revoke the app password on the server: {error}"
                    ));
                }
                if let Some(error) = delete_error {
                    logger.append(&format!(
                        "account removal: could not clear the saved password: {error}"
                    ));
                }
            }
        });
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
        // Keep the highlight on the active account across rebuilds so a
        // refresh never leaves the sidebar without a selection (issue #49).
        self.select_active_sidebar_row();
    }

    /// Select the sidebar row of the active account (or none when no
    /// account is active).
    fn select_active_sidebar_row(&self) {
        let row = self
            .active_account_id
            .as_ref()
            .and_then(|account_id| self.account_rows.get(account_id));
        self.accounts_list.select_row(row);
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
        self.select_active_sidebar_row();
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
/// The header menu model: a custom `theme` section (see [`ThemeSelector`])
/// plus Preferences and About — About last.
fn hamburger_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();
    let theme_section = gio::Menu::new();
    let theme_item = gio::MenuItem::new(None, None);
    theme_item.set_attribute_value("custom", Some(&"theme".to_variant()));
    theme_section.append_item(&theme_item);
    menu.append_section(None, &theme_section);
    let items = gio::Menu::new();
    let preferences_item = gio::MenuItem::new(Some(t("Preferences")), Some("app.preferences"));
    preferences_item.set_icon(&gio::ThemedIcon::new("preferences-system-symbolic"));
    items.append_item(&preferences_item);
    let about_item = gio::MenuItem::new(Some(t("About")), Some("app.about"));
    about_item.set_icon(&gio::ThemedIcon::new("nextsync-info-symbolic"));
    items.append_item(&about_item);
    menu.append_section(None, &items);
    menu
}

/// CSS-drawn circle theme selector (the gnome-text-editor `EditorThemeSelector`
/// pattern): three GtkCheckButtons with `.theme-selector` classes — follow
/// (half-filled), light (white) and dark (black) — with a check overlay on
/// the active one. Order follows gnome-text-editor: system · light · dark.
struct ThemeSelector {
    widget: gtk4::Box,
}

impl ThemeSelector {
    /// Build the selector for the persisted scheme; `on_select` fires with
    /// "system" | "light" | "dark" whenever the active circle changes.
    fn new<F: Fn(&str) + 'static>(active_scheme: &str, on_select: F) -> Self {
        static CSS: &str = r#"
            /* Match GNOME Text Editor's EditorThemeSelector: the checkbutton
             * itself is the colored circle (44 px touch target), and the radio
             * indicator becomes a small check badge at the bottom-right when
             * the option is selected. */
            checkbutton.theme-selector {
                min-width: 44px;
                min-height: 44px;
                padding: 1px;
                border-radius: 9999px;
                background-clip: content-box;
                box-shadow: inset 0 0 0 1px @borders;
            }
            checkbutton.theme-selector.follow {
                background-image: linear-gradient(to bottom right, #fff 49.99%, #202020 50.01%);
            }
            checkbutton.theme-selector.light {
                background-color: #fff;
            }
            checkbutton.theme-selector.dark {
                background-color: #202020;
            }
            checkbutton.theme-selector:hover {
                box-shadow: inset 0 0 0 1px @borders,
                            0 0 0 3px alpha(currentColor, 0.2);
            }
            checkbutton.theme-selector:checked {
                box-shadow: inset 0 0 0 2px @theme_selected_bg_color;
            }
            checkbutton.theme-selector radio {
                min-width: 12px;
                min-height: 12px;
                padding: 2px;
                border: none;
                border-radius: 9999px;
                background: none;
                box-shadow: none;
                -gtk-icon-source: none;
                transform: translate(27px, 14px);
                transition: all 150ms ease;
            }
            checkbutton.theme-selector radio:checked {
                -gtk-icon-source: -gtk-icontheme("object-select-symbolic");
                background-color: @theme_selected_bg_color;
                color: @theme_selected_fg_color;
            }
        "#;
        let provider = gtk4::CssProvider::new();
        #[allow(deprecated)]
        provider.load_from_data(CSS);
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("display"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let widget = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(12)
            .margin_top(10)
            .margin_bottom(6)
            .hexpand(true)
            .build();

        let make_circle = |style_class: &'static str,
                           tooltip: &str,
                           _scheme: &'static str,
                           group: Option<&gtk4::CheckButton>| {
            let button = gtk4::CheckButton::builder()
                .css_classes(["theme-selector", style_class])
                .tooltip_text(tooltip)
                .halign(gtk4::Align::Center)
                .hexpand(true)
                .focus_on_click(false)
                .build();
            if let Some(group) = group {
                button.set_group(Some(group));
            }
            button
        };
        let follow = make_circle("follow", t("Follow system style"), "system", None);
        let light = make_circle("light", t("Light style"), "light", Some(&follow));
        let dark = make_circle("dark", t("Dark style"), "dark", Some(&follow));
        match active_scheme {
            "light" => light.set_active(true),
            "dark" => dark.set_active(true),
            _ => follow.set_active(true),
        }
        let on_select = std::rc::Rc::new(on_select);
        widget.append(&follow);
        widget.append(&light);
        widget.append(&dark);
        for (button, scheme) in [(follow, "system"), (light, "light"), (dark, "dark")] {
            let on_select = on_select.clone();
            button.connect_toggled(move |button| {
                if button.is_active() {
                    on_select(scheme);
                }
            });
        }
        Self { widget }
    }
}

/// Map the persisted color-scheme preference to a libadwaita color scheme.
///
/// Unknown values fall back to following the desktop (`system`).
pub fn color_scheme_for(preference: &str) -> libadwaita::ColorScheme {
    match preference {
        "light" => libadwaita::ColorScheme::ForceLight,
        "dark" => libadwaita::ColorScheme::ForceDark,
        _ => libadwaita::ColorScheme::Default,
    }
}

/// The all-ok light icon for the account summary card, from the aggregate
/// state: green when healthy, amber when paused/offline, red on problems.
pub fn summary_light_for(state: crate::state::AppState) -> &'static str {
    use crate::state::AppState;
    match state {
        AppState::IdleOk | AppState::IdleManualOnly | AppState::SyncQueued | AppState::Syncing => {
            "nextsync-status-ok-symbolic"
        }
        AppState::PausedUser | AppState::PausedBattery | AppState::Offline => {
            "nextsync-status-paused-symbolic"
        }
        AppState::Error
        | AppState::AuthRequired
        | AppState::KeyringLocked
        | AppState::DeleteReview => "nextsync-status-error-symbolic",
        AppState::Unconfigured => "nextsync-status-offline-symbolic",
    }
}

/// Host part of a server URL (`https://cloud.example.com` ->
/// `cloud.example.com`); the raw URL when it does not parse as expected.
pub use crate::util::url::server_host;

/// Resolve an activated sidebar row back to its account id through the
/// id -> row map maintained by [`MainWindow::refresh_sidebar`].
///
/// GTK widgets compare by identity (reference-counted objects), so a row
/// detached by a later refresh no longer matches any entry.
pub fn account_id_for_row<'a>(
    rows: &'a std::collections::HashMap<String, gtk4::ListBoxRow>,
    row: &gtk4::ListBoxRow,
) -> Option<&'a str> {
    rows.iter()
        .find(|(_, candidate)| candidate == &row)
        .map(|(account_id, _)| account_id.as_str())
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
    fn close_action_hides_with_tray_and_quits_without() {
        assert_eq!(close_action(true), CloseAction::Hide);
        assert_eq!(close_action(false), CloseAction::Quit);
    }

    #[test]
    fn account_id_for_row_resolves_rows_by_identity() {
        crate::ui::test_helpers::gtk_smoke(|| {
            let row_a = gtk4::ListBoxRow::new();
            let row_b = gtk4::ListBoxRow::new();
            let mut rows = std::collections::HashMap::new();
            rows.insert("acct-a".to_string(), row_a.clone());
            rows.insert("acct-b".to_string(), row_b.clone());
            assert_eq!(account_id_for_row(&rows, &row_a), Some("acct-a"));
            assert_eq!(account_id_for_row(&rows, &row_b), Some("acct-b"));
            // A row the sidebar no longer tracks resolves to nothing.
            let detached = gtk4::ListBoxRow::new();
            assert_eq!(account_id_for_row(&rows, &detached), None);
        });
    }

    /// Activating a sidebar row presents that account, and the selection
    /// survives a sidebar rebuild (issue #49).
    #[test]
    fn sidebar_activation_switches_the_presented_account() {
        crate::ui::test_helpers::gtk_smoke(|| {
            use crate::storage::config::AccountConfig;
            let dir = tempfile::tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));

            let mut account_a = window_account();
            account_a.folders.clear();
            account_a.server_url = "https://cloud-a.example.com".to_string();
            let mut account_b = AccountConfig {
                id: "acct-window-2".to_string(),
                server_url: "https://cloud-b.example.com".to_string(),
                login_name: "bob".to_string(),
                ..AccountConfig::default()
            };
            account_b.folders.clear();
            let id_a = store.add_account(&account_a).unwrap();
            let id_b = store.add_account(&account_b).unwrap();
            account_a.id = id_a.clone();
            account_b.id = id_b.clone();

            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let mut manager = AccountManager::new(std::rc::Rc::new(std::cell::RefCell::new(
                crate::core::debounce::FakeTimeoutSource::default(),
            )));
            let config = crate::storage::config::Config {
                accounts: vec![account_a.clone(), account_b.clone()],
                ..Default::default()
            };
            manager.start(&config);
            // The activation handler goes through the shared cell, so build
            // the window with a real weak pointer (what the launcher does).
            let main = std::rc::Rc::new_cyclic(|weak: &Weak<std::cell::RefCell<MainWindow>>| {
                std::cell::RefCell::new(MainWindow::new(
                    &app,
                    config,
                    store,
                    manager,
                    crate::core::log::LogBuffer::new(),
                    None,
                    weak.clone(),
                ))
            });

            // Startup presents the first account with its row selected.
            assert_eq!(
                main.borrow().active_account_id.as_deref(),
                Some(id_a.as_str())
            );
            assert_eq!(
                main.borrow().accounts_list.selected_row().as_ref(),
                main.borrow().account_rows.get(&id_a)
            );

            // Activating the second row (what a click does) switches the
            // presented account and the selection.
            let row_b = main.borrow().account_rows.get(&id_b).unwrap().clone();
            row_b.activate();
            assert_eq!(
                main.borrow().active_account_id.as_deref(),
                Some(id_b.as_str())
            );
            assert_eq!(
                main.borrow().accounts_list.selected_row().as_ref(),
                main.borrow().account_rows.get(&id_b)
            );
            assert_eq!(
                main.borrow()
                    .account_view
                    .as_ref()
                    .unwrap()
                    ._account_runtime
                    .account
                    .id,
                id_b
            );

            // A sidebar rebuild (refresh after a config change) keeps the
            // highlight on the active account.
            main.borrow_mut().refresh_sidebar();
            assert_eq!(
                main.borrow().accounts_list.selected_row().as_ref(),
                main.borrow().account_rows.get(&id_b)
            );
        });
    }

    #[test]
    fn server_host_strips_scheme_and_trailing_slash() {
        assert_eq!(
            server_host("https://cloud.example.com"),
            "cloud.example.com"
        );
        assert_eq!(
            server_host("https://cloud.example.com/"),
            "cloud.example.com"
        );
        assert_eq!(server_host("cloud.example.com"), "cloud.example.com");
    }

    #[test]
    fn summary_light_maps_severity() {
        use crate::state::AppState;
        assert_eq!(
            summary_light_for(AppState::IdleOk),
            "nextsync-status-ok-symbolic"
        );
        assert_eq!(
            summary_light_for(AppState::Syncing),
            "nextsync-status-ok-symbolic"
        );
        assert_eq!(
            summary_light_for(AppState::PausedUser),
            "nextsync-status-paused-symbolic"
        );
        assert_eq!(
            summary_light_for(AppState::Offline),
            "nextsync-status-paused-symbolic"
        );
        assert_eq!(
            summary_light_for(AppState::Error),
            "nextsync-status-error-symbolic"
        );
        assert_eq!(
            summary_light_for(AppState::DeleteReview),
            "nextsync-status-error-symbolic"
        );
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

        set_locale(Locale::English);
        let menu = hamburger_menu_model();
        // Custom theme section first, then Preferences + About — About last.
        assert_eq!(menu.n_items(), 2);
        let theme_section = menu
            .item_link(0, gio::MENU_LINK_SECTION)
            .expect("theme section");
        let mut custom = None;
        let iter = theme_section.iterate_item_attributes(0);
        while let Some((key, value)) = iter.next() {
            if key == "custom" {
                custom = value.str().map(str::to_string);
            }
        }
        assert_eq!(
            custom.as_deref(),
            Some("theme"),
            "first section is the custom theme selector"
        );
        let expected: [(&str, &str); 2] =
            [("Preferences", "app.preferences"), ("About", "app.about")];
        let _ = &expected;
        // The Spanish catalog covers every menu entry (the menu is
        // user-visible on every launch).
        set_locale(Locale::Spanish);
        let menu = hamburger_menu_model();
        let items_section = menu
            .item_link(1, gio::MENU_LINK_SECTION)
            .expect("items section");
        let labels: Vec<String> = (0..2)
            .map(|index| {
                let mut label = None;
                let iter = items_section.iterate_item_attributes(index);
                while let Some((key, value)) = iter.next() {
                    if key == "label" {
                        label = value.str().map(str::to_string);
                    }
                }
                label.expect("label")
            })
            .collect();
        assert_eq!(
            labels,
            vec!["Preferencias".to_string(), "Acerca de".to_string(),]
        );
        reset_locale();
    }

    #[test]
    fn color_scheme_preference_maps_to_libadwaita() {
        assert_eq!(color_scheme_for("system"), libadwaita::ColorScheme::Default);
        assert_eq!(
            color_scheme_for("light"),
            libadwaita::ColorScheme::ForceLight
        );
        assert_eq!(color_scheme_for("dark"), libadwaita::ColorScheme::ForceDark);
        // Unknown values follow the desktop.
        assert_eq!(
            color_scheme_for("nonsense"),
            libadwaita::ColorScheme::Default
        );
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
            assert!(window.settings_view.is_none());
            assert!(window.root_stack.child_by_name("settings").is_none());
            assert_eq!(
                window.root_stack.visible_child_name().as_deref(),
                Some("sync")
            );
            reset_locale();
        });
    }

    /// Adding a folder through the store (what Settings does) plus the
    /// `on_folder_changed` callback path must repaint the sync view without
    /// a restart (issue #13).
    #[test]
    fn folder_add_and_remove_refresh_in_place() {
        crate::ui::test_helpers::gtk_smoke(|| {
            use crate::storage::config::FolderConfig;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            let store = ConfigStore::with_path(path.clone());

            let mut account = window_account();
            account.folders.clear();
            let account_id = store.add_account(&account).unwrap();
            account.id = account_id.clone();

            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let mut manager = AccountManager::new(std::rc::Rc::new(std::cell::RefCell::new(
                crate::core::debounce::FakeTimeoutSource::default(),
            )));
            let config = crate::storage::config::Config {
                accounts: vec![account.clone()],
                ..Default::default()
            };
            manager.start(&config);
            let mut window = MainWindow::new(
                &app,
                config,
                store.clone(),
                manager,
                crate::core::log::LogBuffer::new(),
                None,
                Weak::new(),
            );
            window.present_account(Some(&account_id));
            let view = window.account_view.as_ref().expect("account view");
            assert!(
                view._account_runtime.account.folders.is_empty(),
                "starts with no folders"
            );

            // What Settings' Add Folder flow does: mutate the store, then
            // fire on_folder_changed (refresh_after_config_change).
            store
                .add_folder(
                    &account_id,
                    &FolderConfig {
                        id: String::new(),
                        local_root: dir.path().join("one").to_string_lossy().into_owned(),
                        remote_path: "/one".to_string(),
                        space_id: None,
                    },
                )
                .unwrap();
            window.refresh_after_config_change();
            let view = window.account_view.as_ref().expect("account view");
            assert_eq!(
                view._account_runtime.account.folders.len(),
                1,
                "added folder is visible without restart"
            );

            // And the remove path (trash button in the folder list).
            let folder_id = view._account_runtime.account.folders[0].id.clone();
            store.remove_folder(&account_id, &folder_id).unwrap();
            window.refresh_after_config_change();
            let view = window.account_view.as_ref().expect("account view");
            assert!(
                view._account_runtime.account.folders.is_empty(),
                "removed folder disappears without restart"
            );
        });
    }
}
