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

fn main() {
    let application = libadwaita::Application::builder()
        .application_id(APPLICATION_ID)
        .build();

    application.connect_startup(|application| {
        let app = application.downcast_ref::<libadwaita::Application>().unwrap();
        let config = match ConfigStore::new().and_then(|store| store.load()) {
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

        let window = MainWindow::new(
            app,
            config,
            account_manager,
            None,
            None,
            None,
        );
        window.window().present();
    });

    application.run();
}
