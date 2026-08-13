//! NextSync - Nextcloud desktop client for GNOME, rewritten in Rust.
//!
//! Thin binary launcher: builds the libadwaita application, loads the
//! configuration, starts the account runtimes and presents the main window.
//! All logic lives in the `nextsync` library.

use std::cell::RefCell;
use std::rc::Rc;

use libadwaita::prelude::*;

use nextsync::core::account_runtime::AccountManager;
use nextsync::core::debounce::GlibTimeoutSource;
use nextsync::storage::config::ConfigStore;
use nextsync::ui::main_window::MainWindow;

const APPLICATION_ID: &str = "io.github.gnacho.nextsync";

/// Shared holder for the main window, built once in `startup` and presented
/// on every `activate` (the canonical GTK flow).
type WindowSlot = Rc<RefCell<Option<Rc<RefCell<MainWindow>>>>>;

fn main() {
    let application = libadwaita::Application::builder()
        .application_id(APPLICATION_ID)
        .build();

    let window_slot: WindowSlot = Rc::new(RefCell::new(None));

    {
        let window_slot = window_slot.clone();
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

            let source: Rc<RefCell<dyn nextsync::core::debounce::TimeoutSource>> =
                Rc::new(RefCell::new(GlibTimeoutSource::new()));
            let mut account_manager = AccountManager::new(source);
            account_manager.start(&config);

            let main_window = Rc::new(RefCell::new(MainWindow::new(
                app,
                config,
                config_store,
                account_manager,
                None,
            )));
            let weak = Rc::downgrade(&main_window);
            main_window.borrow_mut().install_settings_handler(weak);
            main_window
                .borrow_mut()
                .install_add_account_handler(Rc::downgrade(&main_window));
            *window_slot.borrow_mut() = Some(main_window);
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
