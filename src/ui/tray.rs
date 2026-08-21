//! StatusNotifier tray (ksni 0.3, blocking).
//!
//! Fase 5 (Task 5.5): replicates `ui/tray.py` (v0.4.0) with `ksni`, keeping the
//! Open/Settings/Quit menu (Python item ids 1, 7 and 8 — the v0.4.0 reference
//! no longer ships the account submenu or the Conflicts item) and the
//! monochrome Lucide cloud glyphs (issue #22).
//!
//! ## Threading
//!
//! `ksni::Tray` requires `Send`: the item is moved to the DBus service thread
//! where `menu()` and `activate()` run, while the GTK app is single-threaded
//! and shares its windows through `Rc`. The menu callbacks therefore never
//! touch GTK directly: [`TrayItem`] only holds a [`async_channel::Sender`] of
//! [`TrayAction`], and the [`Tray`] wrapper forwards those actions to the
//! [`TrayCallbacks`] on the GLib main loop (`MainContext::default().spawn_local`,
//! the same thing as `glib::spawn_future_local`), where `Rc` is safe. This is
//! the canonical ksni pattern (see its `examples/realworld.rs`).
//!
//! State updates go the other way. `Tray::update_state` calls
//! `Handle::update`, which runs the closure under the ksni service mutex (on
//! the calling thread, the GLib main thread) and then diffs and re-emits the
//! `PropertiesChanged` / `New*` DBus signals automatically — verified in the
//! ksni 0.3.6 source (`Handle::update` under the `blocking` feature).
//!
//! ## Icons
//!
//! The item publishes the bare themed icon name and lets the tray host resolve
//! it, mirroring the fix #18 decision of the Python client (no rasterized
//! pixmaps). The full-color scalable SVGs (`nextsync-tray-cloud*.svg`)
//! use a fixed white stroke for the panel; the monochrome symbolic SVGs
//! (`nextsync-status-<key>-symbolic.svg`) use the GTK reference color and
//! are installed into the hicolor symbolic theme by the packaging.

use std::rc::Rc;

use async_channel::Sender;
use glib::MainContext;
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Status, ToolTip};

use crate::state::AppState;
use crate::ui::tray_state::{presentation_for, TrayPresentation};
use crate::util::i18n::t;

/// Commands requested by the tray, forwarded from the ksni service thread to
/// the GLib main loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Open (or raise) the main window.
    Open,
    /// Open the settings window.
    Settings,
    /// Open the activity/conflicts window.
    Conflicts,
    /// Pause or resume every account at once (issue #42).
    PauseAll(bool),
    /// Quit the application.
    Quit,
}

/// Actions wired by the application, invoked on the GLib main thread.
#[derive(Clone)]
pub struct TrayCallbacks {
    /// Open the main window (also triggered by a left-click fallback).
    pub open_window: Rc<dyn Fn()>,
    /// Open the settings window.
    pub open_settings: Rc<dyn Fn()>,
    /// Open the activity/conflicts window (optional; hides the menu item).
    pub open_conflicts: Option<Rc<dyn Fn()>>,
    /// Pause or resume every account (issue #42). Receives the desired
    /// state: `true` pauses.
    pub pause_all: Rc<dyn Fn(bool)>,
    /// Whether every account is currently paused (drives the menu label).
    pub all_paused: Rc<dyn Fn() -> bool>,
    /// Quit the application.
    pub quit: Rc<dyn Fn()>,
}

/// Icon name published on the StatusNotifier item for a state.
///
/// One glyph per situation (issues #76 and #87): unconfigured shows the
/// crossed-out cloud, the all-synced aggregate (`icon_key` "ok") a cloud
/// with a check, syncing states (`icon_key` "syncing") the cloud-sync
/// swirl, and problem states (`icon_key` "error") cloud-alert. Everything
/// else (paused, battery, plain offline) keeps the plain cloud.
pub fn icon_name_for(state: AppState, presentation: &TrayPresentation) -> &'static str {
    if presentation.icon_key == "offline" && state == AppState::Unconfigured {
        "nextsync-tray-cloud-off"
    } else if presentation.icon_key == "ok" {
        "nextsync-tray-cloud-check"
    } else if presentation.icon_key == "syncing" {
        "nextsync-tray-cloud-sync"
    } else if presentation.icon_key == "error" {
        "nextsync-tray-cloud-alert"
    } else {
        "nextsync-tray-cloud"
    }
}

/// Themed status icon name for an `icon_key`, mirroring the
/// `nextsync-status-<key>-symbolic` SVGs of tray.py. The v0.4.0 item publishes
/// the tray glyph (see [`icon_name_for`]) instead of these; this mapping is
/// kept for parity and as the reference for the packaged status icons.
pub fn status_icon_key_to_name(icon_key: &str) -> &'static str {
    match icon_key {
        "ok" => "nextsync-status-ok",
        "syncing" => "nextsync-status-syncing",
        "paused" => "nextsync-status-paused",
        "battery" => "nextsync-status-battery",
        "offline" => "nextsync-status-offline",
        "error" => "nextsync-status-error",
        _ => "nextsync-tray-cloud",
    }
}

/// Number of items in the tray menu (Open, Log, Quit).
pub const MENU_ITEM_COUNT: usize = 3;

/// The StatusNotifier item. Only `Send` data lives here, satisfying the
/// `ksni::Tray` bound; user actions leave through the [`TrayAction`] channel.
struct TrayItem {
    state: AppState,
    presentation: TrayPresentation,
    show_conflicts: bool,
    /// Whether every account is paused (pause/resume-all label, issue #42).
    all_paused: bool,
    actions: Sender<TrayAction>,
}

impl TrayItem {
    fn new(state: AppState, actions: Sender<TrayAction>, show_conflicts: bool) -> Self {
        Self {
            state,
            presentation: presentation_for(state),
            show_conflicts,
            all_paused: false,
            actions,
        }
    }

    /// Replace the application state and recompute the presentation.
    fn apply_state(&mut self, state: AppState) {
        self.state = state;
        self.presentation = presentation_for(state);
    }

    /// Record whether every account is paused (drives the pause/resume menu
    /// label, issue #42).
    fn set_all_paused(&mut self, paused: bool) {
        self.all_paused = paused;
    }

    /// The menu items: Open, Settings, Conflicts (when wired) and Quit,
    /// following the v0.4.0 tray (`_layout_data` item ids 1, 7, 8 plus the
    /// conflicts entry `application.py` wires via `open_conflicts`).
    ///
    /// The callbacks run on the ksni service thread, so they only post a
    /// [`TrayAction`] with `try_send` (async-channel 2.x `Sender::send` is an
    /// async fn and would need an executor to make progress).
    fn build_menu(&self) -> Vec<MenuItem<Self>> {
        // Issue #84: Settings and Pause Everything left the tray menu; both
        // live in the main window, one Open click away. The menu keeps Open,
        // Log (when wired) and Quit.
        let open = self.actions.clone();
        let quit = self.actions.clone();
        let mut items: Vec<MenuItem<Self>> = vec![StandardItem {
            label: t("Open NextSync").into(),
            icon_name: "nextsync-menu-open".into(),
            activate: Box::new(move |_this: &mut Self| {
                let _ = open.try_send(TrayAction::Open);
            }),
            ..Default::default()
        }
        .into()];
        if self.show_conflicts {
            let conflicts = self.actions.clone();
            items.push(
                StandardItem {
                    // Separate msgid from the window title ("Sync Activity
                    // and Conflicts"): renaming the menu item must not change
                    // the window title (issue #32).
                    label: t("Log").into(),
                    icon_name: "nextsync-menu-log".into(),
                    activate: Box::new(move |_this: &mut Self| {
                        let _ = conflicts.try_send(TrayAction::Conflicts);
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            StandardItem {
                label: t("Quit").into(),
                icon_name: "nextsync-menu-quit".into(),
                activate: Box::new(move |_this: &mut Self| {
                    let _ = quit.try_send(TrayAction::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

impl ksni::Tray for TrayItem {
    /// A left click opens the menu, mirroring the Python `ItemIsMenu = true`
    /// (GNOME AppIndicator shows the exported menu on one click).
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "nextsync".into()
    }

    fn title(&self) -> String {
        t("NextSync — {state}").replace("{state}", self.presentation.label)
    }

    fn status(&self) -> Status {
        match self.presentation.status {
            "NeedsAttention" => Status::NeedsAttention,
            _ => Status::Active,
        }
    }

    fn icon_name(&self) -> String {
        icon_name_for(self.state, &self.presentation).into()
    }

    fn attention_icon_name(&self) -> String {
        self.icon_name()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: self.icon_name(),
            title: self.title(),
            description: self.presentation.label.into(),
            ..Default::default()
        }
    }

    /// Fallback for hosts that do not honor `MENU_ON_ACTIVATE`.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.actions.try_send(TrayAction::Open);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        self.build_menu()
    }
}

/// Owned tray service.
///
/// Keep this value alive for the lifetime of the app: dropping it drops the
/// [`glib::JoinHandle`] of the action dispatcher (which would cancel it), and
/// without the `Handle` no state refresh is possible.
pub struct Tray {
    handle: ksni::blocking::Handle<TrayItem>,
    _dispatcher: glib::JoinHandle<()>,
}

impl Tray {
    /// Register the tray and start forwarding menu actions to the main loop.
    ///
    /// Fails when no DBus session bus or StatusNotifierHost is available; the
    /// caller should log the error and continue without a tray (the app is
    /// fully usable from the main window).
    pub fn new(initial: AppState, callbacks: TrayCallbacks) -> Result<Self, ksni::Error> {
        let show_conflicts = callbacks.open_conflicts.is_some();
        let (sender, receiver) = async_channel::unbounded();
        let item = TrayItem::new(initial, sender, show_conflicts);
        let handle = item.spawn()?;
        let dispatcher = MainContext::default().spawn_local(dispatch(receiver, callbacks));
        Ok(Self {
            handle,
            _dispatcher: dispatcher,
        })
    }

    /// Refresh the tray icon, title and tooltip for a new application state.
    pub fn update_state(&mut self, state: AppState) {
        let _ = self.handle.update(|item| item.apply_state(state));
    }

    /// Record whether every account is paused so the tray menu shows the
    /// right pause/resume-all label (issue #42).
    pub fn update_all_paused(&mut self, paused: bool) {
        let _ = self.handle.update(|item| item.set_all_paused(paused));
    }

    /// Number of menu items (Open, Settings, [Conflicts], Quit).
    pub fn menu_items(&self) -> usize {
        self.handle
            .update(|item| item.build_menu().len())
            .unwrap_or(MENU_ITEM_COUNT)
    }
}

/// Forward tray actions to the application callbacks on the GLib main thread.
async fn dispatch(receiver: async_channel::Receiver<TrayAction>, callbacks: TrayCallbacks) {
    while let Ok(action) = receiver.recv().await {
        match action {
            TrayAction::Open => (callbacks.open_window)(),
            TrayAction::Settings => (callbacks.open_settings)(),
            TrayAction::Conflicts => {
                if let Some(open_conflicts) = &callbacks.open_conflicts {
                    open_conflicts();
                }
            }
            TrayAction::PauseAll(paused) => {
                (callbacks.pause_all)(paused);
            }
            TrayAction::Quit => (callbacks.quit)(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{reset_locale, set_locale, Locale};
    use ksni::Tray as _;

    fn item_with(state: AppState) -> (TrayItem, async_channel::Receiver<TrayAction>) {
        let (tx, rx) = async_channel::unbounded();
        (TrayItem::new(state, tx, true), rx)
    }

    #[test]
    fn menu_has_three_items_when_conflicts_is_wired() {
        set_locale(Locale::English);
        let (item, _rx) = item_with(AppState::IdleOk);
        let menu = item.build_menu();
        assert_eq!(menu.len(), MENU_ITEM_COUNT);
        let labels: Vec<&str> = menu
            .iter()
            .map(|entry| match entry {
                MenuItem::Standard(standard) => standard.label.as_str(),
                _ => panic!("unexpected menu item type"),
            })
            .collect();
        // Settings and Pause Everything live in the main window (issue #84).
        assert_eq!(labels, vec!["Open NextSync", "Log", "Quit"]);
        reset_locale();
    }

    #[test]
    fn menu_omits_conflicts_when_not_wired() {
        set_locale(Locale::English);
        let (tx, _rx) = async_channel::unbounded();
        let item = TrayItem::new(AppState::IdleOk, tx, false);
        let menu = item.build_menu();
        let labels: Vec<&str> = menu
            .iter()
            .map(|entry| match entry {
                MenuItem::Standard(standard) => standard.label.as_str(),
                _ => panic!("unexpected menu item type"),
            })
            .collect();
        assert_eq!(labels, vec!["Open NextSync", "Quit"]);
        reset_locale();
    }

    #[test]
    fn menu_has_no_pause_or_settings_entry() {
        set_locale(Locale::English);
        let (mut item, _rx) = item_with(AppState::PausedUser);
        item.set_all_paused(true);
        let labels: Vec<String> = item
            .build_menu()
            .iter()
            .map(|entry| match entry {
                MenuItem::Standard(standard) => standard.label.clone(),
                _ => panic!("unexpected menu item type"),
            })
            .collect();
        assert!(!labels.iter().any(|l| l.contains("Pause")));
        assert!(!labels.iter().any(|l| l.contains("Resume")));
        assert!(!labels.iter().any(|l| l == "Settings"));
        reset_locale();
    }

    #[test]
    fn menu_labels_translate_to_spanish() {
        set_locale(Locale::Spanish);
        let (item, _rx) = item_with(AppState::IdleOk);
        let labels: Vec<String> = item
            .build_menu()
            .iter()
            .map(|entry| match entry {
                MenuItem::Standard(standard) => standard.label.clone(),
                _ => panic!("unexpected menu item type"),
            })
            .collect();
        assert_eq!(labels, vec!["Abrir NextSync", "Registro", "Salir"]);
        reset_locale();
    }

    #[test]
    fn menu_activate_forwards_the_expected_actions_in_order() {
        let (mut item, rx) = item_with(AppState::IdleOk);
        let menu = item.build_menu();
        for entry in &menu {
            match entry {
                MenuItem::Standard(standard) => (standard.activate)(&mut item),
                _ => panic!("unexpected menu item type"),
            }
        }
        assert_eq!(rx.try_recv().unwrap(), TrayAction::Open);
        assert_eq!(rx.try_recv().unwrap(), TrayAction::Conflicts);
        assert_eq!(rx.try_recv().unwrap(), TrayAction::Quit);
        assert!(rx.try_recv().is_err(), "no extra actions should be sent");
    }

    #[test]
    fn apply_state_recomputes_the_presentation() {
        set_locale(Locale::English);
        let (mut item, _rx) = item_with(AppState::IdleOk);
        assert_eq!(item.presentation.label, "Synchronized");
        item.apply_state(AppState::Error);
        assert_eq!(item.presentation.label, "Synchronization Error");
        assert_eq!(item.status(), Status::NeedsAttention);
        reset_locale();
    }

    #[test]
    fn icon_name_follows_the_python_tray_glyph_choice() {
        let (unconfigured, _rx) = item_with(AppState::Unconfigured);
        assert_eq!(unconfigured.icon_name(), "nextsync-tray-cloud-off");

        let (offline, _rx) = item_with(AppState::Offline);
        assert_eq!(offline.icon_name(), "nextsync-tray-cloud");

        // Everything synced and OK gets the cloud-check glyph (issue #76).
        let (idle, _rx) = item_with(AppState::IdleOk);
        assert_eq!(idle.icon_name(), "nextsync-tray-cloud-check");

        // A running or queued sync shows the swirl (issue #87).
        let (syncing, _rx) = item_with(AppState::Syncing);
        assert_eq!(syncing.icon_name(), "nextsync-tray-cloud-sync");
        let (queued, _rx) = item_with(AppState::SyncQueued);
        assert_eq!(queued.icon_name(), "nextsync-tray-cloud-sync");

        // Problem states show cloud-alert (issue #87).
        let (error, _rx) = item_with(AppState::Error);
        assert_eq!(error.icon_name(), "nextsync-tray-cloud-alert");
        let (auth, _rx) = item_with(AppState::AuthRequired);
        assert_eq!(auth.icon_name(), "nextsync-tray-cloud-alert");

        // Paused keeps the plain cloud.
        let (paused, _rx) = item_with(AppState::PausedUser);
        assert_eq!(paused.icon_name(), "nextsync-tray-cloud");
    }

    #[test]
    fn title_includes_the_state_label() {
        set_locale(Locale::English);
        let (idle, _rx) = item_with(AppState::IdleOk);
        assert_eq!(idle.title(), "NextSync — Synchronized");
        reset_locale();
    }

    #[test]
    fn title_translates_the_state_label_to_spanish() {
        set_locale(Locale::Spanish);
        let (syncing, _rx) = item_with(AppState::Syncing);
        assert_eq!(syncing.title(), "NextSync — Sincronizando…");
        reset_locale();
    }

    #[test]
    fn tooltip_description_is_the_state_label() {
        set_locale(Locale::English);
        let (item, _rx) = item_with(AppState::KeyringLocked);
        let tooltip = item.tool_tip();
        assert_eq!(tooltip.description, "Password Keyring Locked");
        assert_eq!(tooltip.title, "NextSync — Password Keyring Locked");
        assert!(tooltip.icon_pixmap.is_empty(), "no rasterized pixmaps");
        reset_locale();
    }

    #[test]
    fn status_icon_key_mapping_covers_all_keys() {
        for (key, name) in [
            ("ok", "nextsync-status-ok"),
            ("syncing", "nextsync-status-syncing"),
            ("paused", "nextsync-status-paused"),
            ("battery", "nextsync-status-battery"),
            ("offline", "nextsync-status-offline"),
            ("error", "nextsync-status-error"),
        ] {
            assert_eq!(status_icon_key_to_name(key), name);
        }
        assert_eq!(status_icon_key_to_name("bogus"), "nextsync-tray-cloud");
    }
}
