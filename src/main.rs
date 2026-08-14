//! NextSync - Nextcloud desktop client for GNOME, rewritten in Rust.
//!
//! Thin binary launcher: builds the libadwaita application, loads the
//! configuration, starts the account runtimes, presents the main window and
//! registers the StatusNotifier tray. All logic lives in the `nextsync`
//! library.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use libadwaita::prelude::*;

use nextsync::core::account_runtime::AccountManager;
use nextsync::core::debounce::GlibTimeoutSource;
use nextsync::nextcloud::credentials::CredentialsStore;
use nextsync::nextcloud::push::NotifyPushClient;
use nextsync::state::StateSnapshot;
use nextsync::storage::config::ConfigStore;
use nextsync::ui::main_window::MainWindow;
use nextsync::ui::tray::{Tray, TrayCallbacks};

const APPLICATION_ID: &str = "io.github.gnacho.nextsync";

/// Shared holder for the main window, built once in `startup` and presented
/// on every `activate` (the canonical GTK flow).
type WindowSlot = Rc<RefCell<Option<Rc<RefCell<MainWindow>>>>>;

/// Shared holder for the tray, kept alive for the whole session.
type TraySlot = Rc<RefCell<Option<Tray>>>;

/// Shared holder for the tray's state subscription.
type TraySubscriptionSlot = Rc<RefCell<Option<nextsync::state::Subscription>>>;

fn main() {
    let application = libadwaita::Application::builder()
        .application_id(APPLICATION_ID)
        .build();

    let window_slot: WindowSlot = Rc::new(RefCell::new(None));
    let tray_slot: TraySlot = Rc::new(RefCell::new(None));
    let tray_subscription: TraySubscriptionSlot = Rc::new(RefCell::new(None));

    {
        let window_slot = window_slot.clone();
        let tray_slot = tray_slot.clone();
        let tray_subscription = tray_subscription.clone();
        application.connect_startup(move |application| {
            let app = application
                .downcast_ref::<libadwaita::Application>()
                .unwrap();
            let config_store = match ConfigStore::new() {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("Could not locate configuration: {error}");
                    std::process::exit(1);
                }
            };
            let config = match config_store.load() {
                Ok(config) => config,

                Err(error) => {
                    eprintln!("Could not load configuration: {error}");
                    std::process::exit(1);
                }
            };

            // Apply the persisted color scheme before any window is shown.
            libadwaita::StyleManager::default()
                .set_color_scheme(nextsync::ui::color_scheme_for(&config.general.color_scheme));

            let source: Rc<RefCell<dyn nextsync::core::debounce::TimeoutSource>> =
                Rc::new(RefCell::new(GlibTimeoutSource::new()));
            let logger = nextsync::core::log::LogBuffer::new();
            let mut account_manager = AccountManager::new(source);
            account_manager.start(&config);
            let aggregate = account_manager.aggregate_state();
            // Start the per-folder filesystem watchers and progress forwarders
            // (main-loop consumers, production only).
            account_manager.connect_all_glue();
            // Feed the activity/recent log from every folder's finished runs
            // and raise desktop notifications for problem outcomes.
            let notifier: std::rc::Rc<dyn nextsync::core::notifications::DesktopNotifier> =
                std::rc::Rc::new(nextsync::core::notifications::FreedesktopNotifier);
            let notifications_enabled = config.general.show_notifications;
            for runtime in account_manager.runtimes().values() {
                runtime.connect_logger(&logger, Some(notifier.clone()), notifications_enabled);
            }

            // Show the server's own notifications (shares, comments, mentions)
            // as desktop notifications when the preference is on (issue #31).
            // One watcher per Nextcloud account: OpenCloud has no notifications
            // API, so it is gated the same way the notify_push trigger is.
            // The notify_push `notify_notification` hint pokes the watcher for
            // near-instant delivery; a periodic poll covers the rest. The
            // watchers stay alive through their GLib timers and the push hook.
            for runtime in account_manager.runtimes().values() {
                if !nextsync::nextcloud::push::remote_push_supported(runtime.account.provider) {
                    continue;
                }
                let watcher = std::rc::Rc::new(
                    nextsync::core::server_notifications::ServerNotificationWatcher::new(
                        config_store.clone(),
                        runtime.account.id.clone(),
                        runtime.account.server_url.clone(),
                        runtime.account.login_name.clone(),
                        notifier.clone(),
                        logger.clone(),
                    ),
                );
                runtime.set_on_server_notification(std::rc::Rc::new({
                    let watcher = std::rc::Rc::clone(&watcher);
                    move || watcher.poke()
                }));
                watcher.start();
            }

            // Wire notify_push for every account that has a push client,
            // resolving the keyring password off the main thread so the
            // startup stays instant. A locked/missing credential just leaves
            // the client disabled (remote_interval still polls). The client
            // handle is shared with the runtime, so configuring the clone
            // reaches the running push channel.
            {
                let push_clients: Vec<(NotifyPushClient, String, String, String, bool)> =
                    account_manager
                        .runtimes()
                        .values()
                        .filter_map(|runtime| {
                            runtime.push_client().map(|client| {
                                (
                                    client,
                                    runtime.account.id.clone(),
                                    runtime.account.server_url.clone(),
                                    runtime.account.login_name.clone(),
                                    runtime.account.sync.remote_push_enabled,
                                )
                            })
                        })
                        .collect();
                for (client, account_id, server, username, enabled) in push_clients {
                    let server_for_lookup = server.clone();
                    let username_for_lookup = username.clone();
                    let task = gio::spawn_blocking(move || {
                        CredentialsStore::get_for_account(
                            &account_id,
                            &server_for_lookup,
                            &username_for_lookup,
                        )
                    });
                    glib::spawn_future_local(async move {
                        if let Ok(Ok(Some(password))) = task.await {
                            client.configure(&server, &username, &password, enabled);
                        }
                    });
                }
            }

            // `show_about` lives on `MainWindow`, so it needs the shared cell
            // that does not exist until the `Rc` is built. `new_cyclic` hands
            // us a `Weak` during construction so the header-button callback can
            // be wired up front (the button fires long after construction).
            let main_window: Rc<RefCell<MainWindow>> =
                Rc::new_cyclic(|weak: &Weak<RefCell<MainWindow>>| {
                    let on_show_about: Option<Rc<dyn Fn()>> = Some(Rc::new({
                        let weak = weak.clone();
                        move || {
                            if let Some(main) = weak.upgrade() {
                                main.borrow_mut().show_about();
                            }
                        }
                    }));
                    RefCell::new(MainWindow::new(
                        app,
                        config,
                        config_store,
                        account_manager,
                        logger,
                        on_show_about,
                        weak.clone(),
                    ))
                });
            let weak = Rc::downgrade(&main_window);
            main_window
                .borrow_mut()
                .install_settings_handler(weak.clone());
            main_window
                .borrow_mut()
                .install_add_account_handler(Rc::downgrade(&main_window));
            *window_slot.borrow_mut() = Some(main_window.clone());

            // Register the tray (best effort; the app works without one).
            let tray_callbacks = TrayCallbacks {
                open_window: Rc::new({
                    let weak = weak.clone();
                    move || {
                        if let Some(main) = weak.upgrade() {
                            main.borrow().window().present();
                        }
                    }
                }),
                open_settings: Rc::new({
                    let weak = weak.clone();
                    move || {
                        if let Some(main) = weak.upgrade() {
                            main.borrow_mut().show_preferences();
                        }
                    }
                }),
                open_conflicts: Some(Rc::new({
                    let weak = weak.clone();
                    move || {
                        if let Some(main) = weak.upgrade() {
                            main.borrow_mut().show_conflicts();
                        }
                    }
                })),
                quit: Rc::new({
                    let app = app.clone();
                    move || app.quit()
                }),
            };
            let initial = aggregate.snapshot().state;
            match Tray::new(initial, tray_callbacks) {
                Ok(tray) => {
                    let aggregate = aggregate.clone();
                    let tray_slot_for_sub = tray_slot.clone();
                    let subscription = aggregate.subscribe(move |snapshot: &StateSnapshot| {
                        if let Some(tray) = tray_slot_for_sub.borrow_mut().as_mut() {
                            tray.update_state(snapshot.state);
                        }
                    });
                    // Keep the tray and its state subscription alive.
                    *tray_slot.borrow_mut() = Some(tray);
                    tray_subscription.borrow_mut().replace(subscription);
                }
                Err(_) => {
                    // No DBus session bus or StatusNotifierHost: keep running
                    // without a tray.
                    eprintln!("nextsync: tray unavailable, continuing without it");
                }
            }
        });
    }

    {
        let window_slot = window_slot.clone();
        application.connect_activate(move |_application| {
            if let Some(main) = window_slot.borrow().as_ref() {
                main.borrow().window().present();
            }
        });
    }

    application.run();
}
