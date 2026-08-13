//! GTK4/libadwaita user interface. Fase 5.

pub mod activity;
pub mod conflict_resolver;
pub mod folder_status;
pub mod log_view;
pub mod main_window;
pub mod settings;
pub mod setup;
pub mod tray;
pub mod tray_state;

#[cfg(test)]
mod test_helpers {
    use std::sync::mpsc;
    use std::sync::OnceLock;

    /// Run a GTK smoke test on a single shared worker thread, skipping when no
    /// display is available.
    ///
    /// GTK is single-threaded, so two plain `#[test]`s that call
    /// `gtk4::init()` conflict (the second panics with "Attempted to
    /// initialize GTK from two different threads"). This mirrors
    /// `gtk4::test_synced` but tolerates an unavailable display instead of
    /// panicking, keeping CI headless green. All GTK smoke tests must go
    /// through this helper.
    pub(crate) fn gtk_smoke(function: impl FnOnce() + Send + std::panic::UnwindSafe + 'static) {
        static WORKER: OnceLock<glib::ThreadPool> = OnceLock::new();
        static AVAILABLE: OnceLock<bool> = OnceLock::new();

        let pool = WORKER.get_or_init(|| {
            let pool = glib::ThreadPool::exclusive(1)
                .expect("could not create the GTK test worker thread");
            let (tx, rx) = mpsc::sync_channel(1);
            pool.push(move || {
                let _ = tx.send(gtk4::init().is_ok());
            })
            .expect("could not schedule GTK initialization");
            let available = rx.recv().expect("GTK initialization did not answer");
            let _ = AVAILABLE.set(available);
            pool
        });

        if !*AVAILABLE.get_or_init(|| false) {
            eprintln!("skipped: no display available");
            return;
        }

        let (tx, rx) = mpsc::sync_channel(1);
        pool.push(move || {
            let _ = tx.send(std::panic::catch_unwind(function));
        })
        .expect("could not schedule the GTK test");
        if let Ok(Err(payload)) = rx.recv() {
            std::panic::resume_unwind(payload);
        }
    }
}
