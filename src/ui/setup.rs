//! Account setup wizard (Task 5.3).
//!
//! Port of `ui/setup.py` (v0.4.0) plus the **provider selector** introduced by
//! the Rust rewrite plan. A single [`libadwaita::ApplicationWindow`] with a
//! `gtk4::Stack` walks the user through: welcome (provider selection) → server
//! → authentication (browser flow v2 or manual sign-in) → folders → summary →
//! first-sync confirmation → account creation.
//!
//! # Deviations from `setup.py` (motivated)
//!
//! - **Provider selector (new)**: the rewrite lets the user pick Nextcloud or
//!   OpenCloud before entering any detail (plan Task 5.3). The Python wizard
//!   is Nextcloud-only. For OpenCloud the sign-in asks for an **app password**
//!   (the `--token` of `opencloudcmd`, HTTP Basic, no browser/device flow —
//!   see `memory/opencloud.md`); the browser sign-in row is therefore hidden
//!   for OpenCloud.
//! - **Browser flow v2 (port of `login_flow.py`)**: the "Sign in with
//!   browser" row starts [`crate::nextcloud::login_flow::LoginFlowV2`]. The
//!   HTTP work runs in `gio::spawn_blocking` and the polling timer is a
//!   `glib::timeout_add_seconds_local` source on the main loop (like the
//!   Python GLib timer); cancellation is generation-based (a stale async
//!   continuation or timer tick becomes a no-op). While waiting, the wizard
//!   shows the login URL (ellipsized so a long URL never stretches the
//!   window) with a copy-to-clipboard button next to the Python "Open Browser
//!   Again" and "Cancel" actions.
//! - **OpenCloud remote folder is optional**: OpenCloud folders mirror a whole
//!   space by default; the remote field normalizes like Nextcloud's (blank or
//!   `/` = the space root, which omits the `--remote-folder` flag).
//! - **OpenCloud credential check**: OpenCloud has no OCS user endpoint, so
//!   the wizard validates username + app token against the LibreGraph API
//!   (`GET /graph/v1.0/me`, `Basic user:app-token`; verified against a real
//!   deployment, which also returns the display name). 401/403 surface as a
//!   rejected-credentials error, same as Nextcloud.
//! - **Space discovery**: for OpenCloud the spaces are listed through
//!   LibreGraph (`NextcloudApi::list_opencloud_spaces`, `GET
//!   /graph/v1.0/drives`; the user's own personal space plus project spaces;
//!   other users' personal spaces and the virtual shares aggregate are
//!   excluded). The `opencloudcmd <url>` query mode
//!   (`opencloud_list_spaces`, parsing the `Short ID | DisplayName | ID`
//!   table verified in `opencloud-eu/desktop` `src/cmd/cmd.cpp`) remains as
//!   a fallback. The id is assigned from the discovery (never editable:
//!   without a discovered space the Add Folder dialog is blocked with an
//!   inline error, because the driver requires a space id to sync).
//! - **Not the app's main window**: the Python wizard is the first-run main
//!   window and quits the application on close. In the rewrite it is a
//!   secondary window opened from the main window, so closing it does not quit
//!   the app, and on successful completion it closes itself after invoking
//!   [`SetupCallbacks::on_complete`].
//! - **First-sync probe errors finish setup directly** (matching the Python
//!   `on_remote` error branch), and the conflicted-copy wording keeps the
//!   literal `{name}` / `.<ext>.` placeholders of the Python message.
//! - **i18n (Task 6.1)**: user-visible strings go through
//!   [`crate::util::i18n::t`]; msgids missing from the Spanish catalog fall
//!   back to the English source.

use std::cell::RefCell;
use std::rc::Rc;

use gio::AppInfo;
use glib::ControlFlow;
use libadwaita::prelude::*;

use crate::core::sync_safety::local_folder_is_empty;
use crate::nextcloud::api::{ApiError, NextcloudApi};
use crate::nextcloud::command::find_binary;
use crate::nextcloud::credentials::CredentialsStore;
use crate::nextcloud::driver::{opencloud_list_spaces, Provider};
use crate::nextcloud::login_flow::{
    LoginFlowError, LoginFlowResult, LoginFlowStart, LoginFlowV2, PollOutcome, MAX_POLLS,
    POLL_INTERVAL_SECONDS,
};
use crate::storage::config::{
    account_id, default_sync_root, expanduser, normalize_remote_path, normalize_server_url,
    AccountConfig, Config, ConfigError, ConfigStore, FolderConfig,
};
use crate::util::i18n::t;

const WINDOW_TITLE: &str = "Set Up NextSync";

/// Callback invoked once an account has been created and persisted.
pub type SetupCompleteCallback = Rc<dyn Fn(AccountConfig)>;

/// Callbacks the setup wizard invokes during its lifecycle.
#[derive(Clone, Default)]
pub struct SetupCallbacks {
    /// Invoked with the freshly created (validated) account after the wizard
    /// finishes, so the application can start its runtime and refresh the
    /// sidebar.
    pub on_complete: Option<SetupCompleteCallback>,
}

/// A folder pair collected by the wizard, before validation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WizardFolder {
    local_root: String,
    remote_path: String,
    space_id: Option<String>,
}

/// One space discovered from an OpenCloud server.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpaceInfo {
    id: String,
    display_name: String,
}

/// Mutable flow state of the wizard, shared between the page handlers.
#[derive(Debug, Default)]
struct WizardState {
    provider: Provider,
    server: String,
    username: String,
    authentication_type: String,
    trust_invalid: bool,
    folders: Vec<WizardFolder>,
    /// Discovered OpenCloud space id (assigned to every folder).
    space_id: Option<String>,
}

/// What the polling timer should do at a given tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollDecision {
    /// Stop the timer (flow cancelled, session gone or budget exhausted).
    Stop,
    /// Keep the timer running but skip this round (a poll is in flight).
    Skip,
    /// Issue poll number N (the caller posts it in `gio::spawn_blocking`).
    Poll,
}

/// Live state of the browser sign-in flow (the Rust analogue of the Python
/// `LoginFlowV2` timer fields, minus the HTTP client which is created per
/// blocking call so nothing non-`Send` crosses threads).
#[derive(Debug, Default)]
struct BrowserFlowState {
    /// The initiated session (`poll.endpoint`/`token`/`login`), if any.
    start: Option<LoginFlowStart>,
    /// Monotonic token: bumped on every cancel so stale continuations of a
    /// previous flow (timers, in-flight polls, stores) become no-ops.
    generation: u64,
    /// Polls issued so far (the Python `_poll_count`).
    poll_count: usize,
    /// Whether a poll HTTP call is still pending (the Python
    /// `_poll_in_flight` guard against overlapping requests).
    poll_in_flight: bool,
}

impl BrowserFlowState {
    /// Cancel the flow and invalidate every pending continuation.
    fn cancel(&mut self) {
        self.generation += 1;
        self.start = None;
        self.poll_count = 0;
        self.poll_in_flight = false;
    }

    /// Decide (and account) what the timer tick at `generation` should do,
    /// replicating the Python `_poll` bookkeeping.
    fn begin_poll(&mut self, generation: u64) -> PollDecision {
        if self.generation != generation || self.start.is_none() {
            return PollDecision::Stop;
        }
        if self.poll_in_flight {
            return PollDecision::Skip;
        }
        self.poll_count += 1;
        if self.poll_count > MAX_POLLS {
            self.start = None;
            return PollDecision::Stop;
        }
        self.poll_in_flight = true;
        PollDecision::Poll
    }
}

/// Every widget the handlers need to reach across pages.
#[derive(Clone)]
struct SetupWidgets {
    provider_row: libadwaita::ComboRow,
    provider_warning: libadwaita::Banner,
    server_title: gtk4::Label,
    server_entry: libadwaita::EntryRow,
    trust_invalid: gtk4::CheckButton,
    server_error: gtk4::Label,
    auth_title: gtk4::Label,
    opencloud_hint: gtk4::Label,
    browser_group: libadwaita::PreferencesGroup,
    browser_row: libadwaita::ActionRow,
    waiting_box: gtk4::Box,
    login_url_label: gtk4::Label,
    username_entry: libadwaita::EntryRow,
    password_entry: libadwaita::PasswordEntryRow,
    auth_error: gtk4::Label,
    manual_button: gtk4::Button,
    folder_list: gtk4::ListBox,
    space_label: gtk4::Label,
    folder_error: gtk4::Label,
    summary_list: gtk4::ListBox,
    summary_hint: gtk4::Label,
    start_button: gtk4::Button,
}

impl SetupWidgets {
    fn new() -> Self {
        let provider_row = libadwaita::ComboRow::builder()
            .title(t("Sync provider"))
            .tooltip_text(t("Choose the synchronization provider"))
            .build();
        let model = gtk4::StringList::new(&["Nextcloud", "OpenCloud"]);
        provider_row.set_model(Some(&model));

        let provider_warning = libadwaita::Banner::new("");

        let server_title = title_label(t("Connect to Nextcloud"));
        let server_entry = libadwaita::EntryRow::new();
        server_entry.set_title(t("Nextcloud server URL"));
        server_entry.set_tooltip_text(Some(t("The address of your Nextcloud or OpenCloud server")));
        server_entry.set_text("https://");
        let trust_invalid =
            gtk4::CheckButton::with_label(t("Allow invalid or self-signed certificates"));
        trust_invalid.set_tooltip_text(Some(t(
            "This weakens connection security. Enable only for a server you trust.",
        )));
        let server_error = error_label("");

        let auth_title = title_label(t("Sign In"));
        let opencloud_hint = dim_label(
            t("OpenCloud requires an app password. Create one in the server account settings (App Tokens) and enter it below."),
        );
        opencloud_hint.set_visible(false);

        let browser_row = libadwaita::ActionRow::builder()
            .title(t("Sign in with browser"))
            .subtitle(t("Recommended. Supports two-factor authentication."))
            .tooltip_text(t("Sign in by authorizing the app in your browser"))
            .activatable(true)
            .build();
        let browser_icon = gtk4::Image::builder()
            .icon_name("web-browser-symbolic")
            .pixel_size(16)
            .build();
        browser_row.add_prefix(&browser_icon);
        let browser_next = gtk4::Image::builder()
            .icon_name("go-next-symbolic")
            .pixel_size(16)
            .build();
        browser_row.add_suffix(&browser_next);
        let browser_group = libadwaita::PreferencesGroup::new();
        browser_group.add(&browser_row);

        let waiting_box = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        waiting_box.set_visible(false);
        let spinner = gtk4::Spinner::new();
        spinner.set_spinning(true);
        spinner.set_halign(gtk4::Align::Center);
        waiting_box.append(&spinner);
        waiting_box.append(&centered_label(t(
            "Waiting for authorization in your browser…",
        )));
        let login_url_label = gtk4::Label::builder()
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .max_width_chars(40)
            .selectable(true)
            .halign(gtk4::Align::Center)
            .css_classes(["dim-label"])
            .build();
        waiting_box.append(&login_url_label);

        let username_entry = libadwaita::EntryRow::new();
        username_entry.set_title(t("Username"));
        username_entry.set_tooltip_text(Some(t("Your account user name")));
        let password_entry = libadwaita::PasswordEntryRow::new();
        password_entry.set_title(t("Password or app password"));
        password_entry.set_tooltip_text(Some(t("Your account password or app password")));
        let auth_error = error_label("");
        let manual_button = gtk4::Button::with_label(t("Sign In"));
        manual_button.add_css_class("suggested-action");

        let folder_list = gtk4::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        let space_label = dim_label("");
        space_label.set_visible(false);
        let folder_error = error_label("");

        let summary_list = gtk4::ListBox::builder()
            .css_classes(["boxed-list"])
            .selection_mode(gtk4::SelectionMode::None)
            .build();
        let summary_hint = dim_label(
            t("The chosen folders will be mirrored in both directions using the Nextcloud synchronization engine."),
        );
        let start_button = gtk4::Button::with_label(t("Start Synchronizing"));
        start_button.add_css_class("suggested-action");
        start_button.set_tooltip_text(Some(t("Finish setup and start synchronizing")));

        Self {
            provider_row,
            provider_warning,
            server_title,
            server_entry,
            trust_invalid,
            server_error,
            auth_title,
            opencloud_hint,
            browser_group,
            browser_row,
            waiting_box,
            login_url_label,
            username_entry,
            password_entry,
            auth_error,
            manual_button,
            folder_list,
            space_label,
            folder_error,
            summary_list,
            summary_hint,
            start_button,
        }
    }
}

/// Shared context every wizard handler needs (stack, widgets, state, stores).
#[derive(Clone)]
struct SetupContext {
    stack: gtk4::Stack,
    widgets: SetupWidgets,
    state: Rc<RefCell<WizardState>>,
    browser: Rc<RefCell<BrowserFlowState>>,
    config_store: ConfigStore,
    callbacks: SetupCallbacks,
    window: libadwaita::ApplicationWindow,
}

/// The account setup wizard: a stack-based multi-page window.
pub struct SetupWindow {
    window: libadwaita::ApplicationWindow,
    /// Kept so tests can drive the wizard pages; production only reads
    /// `window` (the pages hold their own clones of the context).
    #[cfg_attr(not(test), allow(dead_code))]
    context: SetupContext,
}

impl SetupWindow {
    /// Build the wizard window (already wired, not yet shown).
    pub fn new(
        application: &libadwaita::Application,
        config_store: ConfigStore,
        callbacks: SetupCallbacks,
    ) -> Self {
        let window = libadwaita::ApplicationWindow::builder()
            .application(application)
            .title(t(WINDOW_TITLE))
            .default_width(620)
            .default_height(680)
            .build();

        let toolbar = libadwaita::ToolbarView::new();
        let header = gtk4::HeaderBar::new();
        toolbar.add_top_bar(&header);

        let stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(200)
            .build();
        toolbar.set_content(Some(&stack));
        window.set_content(Some(&toolbar));

        let context = SetupContext {
            stack: stack.clone(),
            widgets: SetupWidgets::new(),
            state: Rc::new(RefCell::new(WizardState::default())),
            browser: Rc::new(RefCell::new(BrowserFlowState::default())),
            config_store,
            callbacks,
            window: window.clone(),
        };
        let context_for_pages = context.clone();
        build_welcome_page(&context_for_pages);
        build_server_page(&context_for_pages);
        build_authentication_page(&context_for_pages);
        build_folders_page(&context_for_pages);
        build_summary_page(&context_for_pages);
        stack.set_visible_child_name("welcome");

        Self { window, context }
    }

    /// The underlying window, for presentation.
    pub fn window(&self) -> &libadwaita::ApplicationWindow {
        &self.window
    }

    /// Present the wizard window.
    pub fn present(&self) {
        self.window.present();
    }
}

#[cfg(test)]
impl SetupWindow {
    /// The wizard widgets, for smoke tests.
    fn widgets(&self) -> &SetupWidgets {
        &self.context.widgets
    }

    /// Re-run the provider-specific authentication layout for a provider.
    fn configure_authentication_for(&self, provider: Provider) {
        self.context.state.borrow_mut().provider = provider;
        configure_authentication(&self.context);
    }
}

// ---------------------------------------------------------------------------
// Page builders
// ---------------------------------------------------------------------------

fn build_welcome_page(ctx: &SetupContext) {
    let (page, content) = page();
    let status = libadwaita::StatusPage::builder()
        .icon_name("io.github.gnacho.nextsync")
        .title("NextSync")
        .description(t(
            "A lightweight desktop synchronizer for Nextcloud and OpenCloud.",
        ))
        .vexpand(true)
        .build();
    content.append(&status);

    content.append(
        &dim_label(
            t("Your complete file tree will be stored physically on this computer and synchronized in both directions."),
        ),
    );

    let provider_group = libadwaita::PreferencesGroup::builder()
        .title(t("Provider"))
        .build();
    provider_group.add(&ctx.widgets.provider_row);
    content.append(&provider_group);

    update_provider_warning(&ctx.widgets.provider_warning, Provider::default());
    content.append(&ctx.widgets.provider_warning);

    let warning = ctx.widgets.provider_warning.clone();
    let provider_row = ctx.widgets.provider_row.clone();
    provider_row.connect_selected_notify(move |row| {
        update_provider_warning(&warning, provider_from_combo(row));
    });

    let continue_button = gtk4::Button::with_label(t("Continue"));
    continue_button.set_tooltip_text(Some(t("Continue to the next step")));
    continue_button.add_css_class("suggested-action");
    continue_button.add_css_class("pill");
    continue_button.set_halign(gtk4::Align::Center);
    {
        let ctx = ctx.clone();
        continue_button.connect_clicked(move |_| {
            update_provider_warning(
                &ctx.widgets.provider_warning,
                provider_from_combo(&ctx.widgets.provider_row),
            );
            ctx.stack.set_visible_child_name("server");
        });
    }
    content.append(&continue_button);

    ctx.stack.add_named(&page, Some("welcome"));
}

fn build_server_page(ctx: &SetupContext) {
    let (page, content) = page();
    content.append(&ctx.widgets.server_title);
    content.append(&dim_label(t(
        "Enter the address you normally use to open the server in a browser.",
    )));

    let group = libadwaita::PreferencesGroup::new();
    group.add(&ctx.widgets.server_entry);
    group.add(&ctx.widgets.trust_invalid);
    content.append(&group);
    content.append(&ctx.widgets.server_error);

    let actions = action_box();
    actions.append(&back_button(&ctx.stack, "welcome"));
    let continue_button = gtk4::Button::with_label(t("Continue"));
    continue_button.set_tooltip_text(Some(t("Continue to the next step")));
    continue_button.add_css_class("suggested-action");
    {
        let ctx = ctx.clone();
        continue_button.connect_clicked(move |_| {
            let server = match normalize_server_url(&ctx.widgets.server_entry.text()) {
                Ok(server) => server,
                Err(error) => {
                    ctx.widgets.server_error.set_text(&error.to_string());
                    return;
                }
            };
            let provider = provider_from_combo(&ctx.widgets.provider_row);
            {
                let mut state = ctx.state.borrow_mut();
                state.provider = provider;
                state.server = server.clone();
                state.trust_invalid = ctx.widgets.trust_invalid.is_active();
            }
            ctx.widgets.server_error.set_text("");
            configure_authentication(&ctx);
            ctx.stack.set_visible_child_name("authentication");
        });
    }
    actions.append(&continue_button);
    content.append(&actions);

    ctx.stack.add_named(&page, Some("server"));
}

fn build_authentication_page(ctx: &SetupContext) {
    let (page, content) = page();
    content.append(&ctx.widgets.auth_title);
    content.append(&ctx.widgets.opencloud_hint);

    content.append(&ctx.widgets.browser_group);
    {
        let ctx_for_browser = ctx.clone();
        ctx.widgets.browser_row.connect_activated(move |_| {
            browser_login(&ctx_for_browser);
        });
    }

    content.append(&ctx.widgets.waiting_box);
    let waiting_actions = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk4::Align::Center)
        .build();
    let copy = gtk4::Button::with_label(t("Copy Link"));
    copy.set_tooltip_text(Some(t("Copy the login link to the clipboard")));
    {
        let url_label = ctx.widgets.login_url_label.clone();
        copy.connect_clicked(move |_| {
            let url = url_label.text().to_string();
            if url.is_empty() {
                return;
            }
            if let Some(display) = gtk4::gdk::Display::default() {
                display.clipboard().set_text(&url);
            }
        });
    }
    waiting_actions.append(&copy);
    let reopen = gtk4::Button::with_label(t("Open Browser Again"));
    reopen.set_tooltip_text(Some(t("Open the login page in the browser again")));
    {
        let url_label = ctx.widgets.login_url_label.clone();
        reopen.connect_clicked(move |_| {
            let url = url_label.text().to_string();
            if !url.is_empty() {
                open_login_url(&url);
            }
        });
    }
    waiting_actions.append(&reopen);
    let cancel = gtk4::Button::with_label(t("Cancel"));
    cancel.set_tooltip_text(Some(t("Cancel the browser sign-in")));
    {
        let ctx_for_cancel = ctx.clone();
        cancel.connect_clicked(move |_| {
            cancel_browser_flow(&ctx_for_cancel);
            ctx_for_cancel.widgets.waiting_box.set_visible(false);
        });
    }
    waiting_actions.append(&cancel);
    ctx.widgets.waiting_box.append(&waiting_actions);

    let manual_group = libadwaita::PreferencesGroup::builder()
        .title(t("Manual Sign In"))
        .build();
    manual_group.add(&ctx.widgets.username_entry);
    manual_group.add(&ctx.widgets.password_entry);
    content.append(&manual_group);
    content.append(&ctx.widgets.auth_error);

    let actions = action_box();
    actions.append(&back_button(&ctx.stack, "server"));
    let ctx_for_login = ctx.clone();
    let manual_button = ctx.widgets.manual_button.clone();
    manual_button.connect_clicked(move |_| {
        manual_login(&ctx_for_login);
    });
    actions.append(&manual_button);
    content.append(&actions);

    ctx.stack.add_named(&page, Some("authentication"));
}

fn build_folders_page(ctx: &SetupContext) {
    let (page, content) = page();
    content.append(&title_label(t("Synchronization Folders")));
    content.append(&dim_label(
        t("Choose the local folders to mirror from this account. You can add several, or finish now and add folders later from Settings."),
    ));
    content.append(&ctx.widgets.space_label);
    content.append(&ctx.widgets.folder_list);
    content.append(&ctx.widgets.folder_error);

    let add_list = gtk4::ListBox::builder()
        .css_classes(["boxed-list"])
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    let add_row = libadwaita::ActionRow::builder()
        .title(t("Add Folder"))
        .subtitle(t("Mirror another local folder from this account"))
        .tooltip_text(t("Add a local folder to synchronize"))
        .activatable(true)
        .build();
    let add_icon = gtk4::Image::builder()
        .icon_name("folder-new-symbolic")
        .pixel_size(16)
        .build();
    add_row.add_prefix(&add_icon);
    let next = gtk4::Image::builder()
        .icon_name("go-next-symbolic")
        .pixel_size(16)
        .build();
    add_row.add_suffix(&next);
    add_list.append(&add_row);
    content.append(&add_list);

    let actions = action_box();
    actions.append(&back_button(&ctx.stack, "authentication"));
    let review = gtk4::Button::with_label(t("Review Setup"));
    review.add_css_class("suggested-action");
    {
        let ctx = ctx.clone();
        review.connect_clicked(move |_| {
            folders_continue(&ctx);
        });
    }
    actions.append(&review);
    content.append(&actions);

    {
        let ctx = ctx.clone();
        add_row.connect_activated(move |_| {
            ctx.widgets.folder_error.set_text("");
            present_add_folder_dialog(&ctx, None, None);
        });
    }

    ctx.stack.add_named(&page, Some("folders"));
}

fn build_summary_page(ctx: &SetupContext) {
    let (page, content) = page();
    content.append(&title_label(t("Ready to Synchronize")));
    content.append(&ctx.widgets.summary_list);
    content.append(&ctx.widgets.summary_hint);

    let actions = action_box();
    actions.append(&back_button(&ctx.stack, "folders"));
    let ctx_for_start = ctx.clone();
    let start_button = ctx.widgets.start_button.clone();
    start_button.connect_clicked(move |_| {
        start_syncing(&ctx_for_start);
    });
    actions.append(&start_button);
    content.append(&actions);

    ctx.stack.add_named(&page, Some("summary"));
}

// ---------------------------------------------------------------------------
// Flow handlers
// ---------------------------------------------------------------------------

/// Re-label the authentication page widgets for the selected provider.
fn configure_authentication(ctx: &SetupContext) {
    let opencloud = ctx.state.borrow().provider == Provider::OpenCloud;
    ctx.widgets.server_title.set_text(if opencloud {
        t("Connect to OpenCloud")
    } else {
        t("Connect to Nextcloud")
    });
    ctx.widgets.auth_title.set_text(if opencloud {
        t("Connect to OpenCloud")
    } else {
        t("Sign In")
    });
    ctx.widgets.password_entry.set_title(if opencloud {
        t("App password")
    } else {
        t("Password or app password")
    });
    ctx.widgets.opencloud_hint.set_visible(opencloud);
    // The browser flow is Nextcloud-only (OpenCloud has no login/v2 endpoint).
    if opencloud {
        cancel_browser_flow(ctx);
        ctx.widgets.waiting_box.set_visible(false);
    }
    ctx.widgets.browser_group.set_visible(!opencloud);
    ctx.widgets.manual_button.set_sensitive(true);
}

/// Validate the manual credentials, store them in the keyring and advance.
fn manual_login(ctx: &SetupContext) {
    let username = ctx.widgets.username_entry.text().trim().to_string();
    let password = ctx.widgets.password_entry.text().to_string();
    if username.is_empty() || password.is_empty() {
        ctx.widgets
            .auth_error
            .set_text(t("Enter a username and password or app password."));
        return;
    }
    ctx.widgets.manual_button.set_sensitive(false);
    ctx.widgets.auth_error.set_text(t("Checking account…"));

    let server = ctx.state.borrow().server.clone();
    let provider = ctx.state.borrow().provider;
    let server_for_validate = server.clone();
    let username_for_validate = username.clone();
    let password_for_validate = password.clone();
    let validate = gio::spawn_blocking(move || {
        if provider == Provider::OpenCloud {
            NextcloudApi::new().validate_opencloud_credentials(
                &server_for_validate,
                &username_for_validate,
                &password_for_validate,
            )
        } else {
            NextcloudApi::new().validate_credentials(
                &server_for_validate,
                &username_for_validate,
                &password_for_validate,
            )
        }
    });

    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let validation = match validate.await {
            Ok(result) => result,
            Err(_) => Err(ApiError::Transport),
        };
        match validation {
            Ok(_display_name) => {
                ctx.state.borrow_mut().username = username.clone();
                ctx.state.borrow_mut().authentication_type = "manual".to_string();
                let account_id = account_id(&server, &username);
                let password_for_store = password.clone();
                let store = gio::spawn_blocking(move || {
                    CredentialsStore::set(&account_id, &password_for_store)
                        .map_err(|error| format!("{error}"))
                });
                let ctx_for_store = ctx.clone();
                glib::spawn_future_local(async move {
                    let stored = match store.await {
                        Ok(result) => result,
                        Err(_) => Err("the keyring call panicked".to_string()),
                    };
                    match stored {
                        Ok(()) => {
                            ctx_for_store.widgets.auth_error.set_text("");
                            ctx_for_store.widgets.password_entry.set_text("");
                            if ctx_for_store.state.borrow().provider == Provider::OpenCloud {
                                start_space_discovery(
                                    &ctx_for_store,
                                    &server,
                                    &username,
                                    &password,
                                );
                            }
                            ctx_for_store.stack.set_visible_child_name("folders");
                        }
                        Err(message) => {
                            ctx_for_store.widgets.manual_button.set_sensitive(true);
                            ctx_for_store.widgets.auth_error.set_text(t(&format!(
                                "Could not store the account password: {message}"
                            )));
                        }
                    }
                });
            }
            Err(error) => {
                ctx.widgets.manual_button.set_sensitive(true);
                ctx.widgets.auth_error.set_text(&error.to_string());
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Browser sign-in flow (Login Flow v2)
// ---------------------------------------------------------------------------

/// Open `url` with the user's default browser (`launch_default_for_uri`).
fn open_login_url(url: &str) {
    if let Err(error) = AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>) {
        eprintln!("Setup: could not open the login URL: {error}");
    }
}

/// Cancel the browser flow (timer continuations become no-ops).
fn cancel_browser_flow(ctx: &SetupContext) {
    ctx.browser.borrow_mut().cancel();
}

/// Start the browser sign-in: initiate the flow, open the login URL and let
/// the polling timer take over. Mirrors the Python `_browser_login`.
fn browser_login(ctx: &SetupContext) {
    cancel_browser_flow(ctx);
    ctx.widgets.auth_error.set_text("");
    ctx.widgets.login_url_label.set_text("");
    ctx.widgets.waiting_box.set_visible(true);

    let server = ctx.state.borrow().server.clone();
    let generation = ctx.browser.borrow().generation;
    let initiate = gio::spawn_blocking(move || LoginFlowV2::new().initiate(&server));

    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        if ctx.browser.borrow().generation != generation {
            return;
        }
        let start = match initiate.await {
            Ok(Ok(start)) => start,
            Ok(Err(error)) => {
                browser_flow_failed(&ctx, &error);
                return;
            }
            Err(_) => {
                browser_flow_failed(&ctx, &LoginFlowError::Transport);
                return;
            }
        };
        if ctx.browser.borrow().generation != generation {
            return;
        }
        ctx.browser.borrow_mut().start = Some(start.clone());
        open_login_url(&start.login_url);
        ctx.widgets.login_url_label.set_text(&start.login_url);
        schedule_browser_poll(&ctx, generation);
    });
}

/// Install the recurring poll timer (one source per flow generation).
fn schedule_browser_poll(ctx: &SetupContext, generation: u64) {
    let ctx_for_poll = ctx.clone();
    glib::timeout_add_seconds_local(POLL_INTERVAL_SECONDS, move || {
        browser_poll_tick(&ctx_for_poll, generation)
    });
}

/// One timer tick: decide, issue the poll off-thread and keep waiting.
fn browser_poll_tick(ctx: &SetupContext, generation: u64) -> ControlFlow {
    let start = {
        let mut browser = ctx.browser.borrow_mut();
        match browser.begin_poll(generation) {
            PollDecision::Stop => {
                // The Python flow only stops by itself when the 20-minute
                // budget runs out; cancellation is user-driven.
                if browser.poll_count > MAX_POLLS {
                    drop(browser);
                    ctx.widgets.waiting_box.set_visible(false);
                    ctx.widgets
                        .auth_error
                        .set_text(t("Browser authorization expired after 20 minutes."));
                }
                return ControlFlow::Break;
            }
            PollDecision::Skip => return ControlFlow::Continue,
            PollDecision::Poll => browser.start.clone(),
        }
        .expect("begin_poll only issues polls with a live session")
    };

    let poll = gio::spawn_blocking(move || LoginFlowV2::new().poll(&start));
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        ctx.browser.borrow_mut().poll_in_flight = false;
        if ctx.browser.borrow().generation != generation {
            return;
        }
        match poll.await {
            Ok(Ok(PollOutcome::Pending)) => {}
            Ok(Ok(PollOutcome::Authorized(result))) => browser_flow_succeeded(&ctx, result),
            // The Python ignores transport failures during polling (a flaky
            // network just skips one round) and so does this driver.
            Ok(Err(LoginFlowError::Transport)) => {}
            Ok(Err(error)) => browser_flow_failed(&ctx, &error),
            Err(_) => eprintln!("Setup: the browser sign-in poll panicked"),
        }
    });
    ControlFlow::Continue
}

/// The user authorized the app: adopt the credentials and store the secret.
fn browser_flow_succeeded(ctx: &SetupContext, result: LoginFlowResult) {
    cancel_browser_flow(ctx);
    ctx.widgets.waiting_box.set_visible(false);
    let server = match normalize_server_url(&result.server) {
        Ok(server) => server,
        Err(error) => {
            ctx.widgets.auth_error.set_text(&error.to_string());
            return;
        }
    };
    {
        let mut state = ctx.state.borrow_mut();
        state.server = server.clone();
        state.username = result.login_name.clone();
        state.authentication_type = "browser".to_string();
    }
    ctx.widgets
        .auth_error
        .set_text(t("Saving account securely…"));

    let account_id = account_id(&server, &result.login_name);
    let app_password = result.app_password;
    let store = gio::spawn_blocking(move || {
        CredentialsStore::set(&account_id, &app_password).map_err(|error| error.to_string())
    });

    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let stored = store
            .await
            .unwrap_or_else(|_| Err("the keyring call panicked".to_string()));
        match stored {
            Ok(()) => {
                ctx.widgets.auth_error.set_text("");
                ctx.widgets.password_entry.set_text("");
                ctx.stack.set_visible_child_name("folders");
            }
            Err(message) => {
                ctx.widgets.auth_error.set_text(
                    &t("Could not store the account password: {message}").replacen(
                        "{message}",
                        &message,
                        1,
                    ),
                );
            }
        }
    });
}

/// The flow failed: cancel it and surface the (translated) error.
fn browser_flow_failed(ctx: &SetupContext, error: &LoginFlowError) {
    cancel_browser_flow(ctx);
    ctx.widgets.waiting_box.set_visible(false);
    ctx.widgets.auth_error.set_text(&error.message());
}

/// Discover the OpenCloud spaces in the background and remember the first one.
///
/// The native WebDAV listing (`PROPFIND /remote.php/dav/spaces/`) runs first;
/// the `opencloudcmd` query mode remains as a fallback for servers where the
/// WebDAV listing is unavailable.
fn start_space_discovery(ctx: &SetupContext, server: &str, username: &str, password: &str) {
    let server = server.to_string();
    let username = username.to_string();
    let password = password.to_string();
    let discovery = gio::spawn_blocking(move || -> Vec<SpaceInfo> {
        let native = NextcloudApi::new().list_opencloud_spaces(&server, &username, &password);
        match native {
            Ok(spaces) => spaces
                .into_iter()
                .map(|space| SpaceInfo {
                    display_name: space.display_name.unwrap_or_else(|| space.id.clone()),
                    id: space.id,
                })
                .collect(),
            Err(_) => match opencloud_list_spaces(&server, &username, &password, None) {
                Ok(output) => parse_spaces_list(&output),
                Err(_) => Vec::new(),
            },
        }
    });
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let spaces = discovery.await.unwrap_or_else(|_| Vec::new());
        let space_id = spaces.first().map(|space| space.id.clone());
        ctx.state.borrow_mut().space_id = space_id.clone();
        update_space_label(&ctx, space_id.as_deref());
    });
}

/// Show an inline error on the folders page (used when the Add Folder
/// dialog cannot even open).
fn present_folder_error(ctx: &SetupContext, message: &str) {
    ctx.widgets.folder_error.set_text(message);
}

fn update_space_label(ctx: &SetupContext, space_id: Option<&str>) {
    match space_id {
        Some(id) => {
            ctx.widgets.space_label.set_text(t(&format!("Space: {id}")));
            ctx.widgets.space_label.set_visible(true);
        }
        None => {
            ctx.widgets.space_label.set_text(t(
                "No space discovered for this account. Sign in again to retry the discovery.",
            ));
            ctx.widgets.space_label.set_visible(true);
        }
    }
}

/// Present the Add Folder dialog. `previous`/`error` re-open a failed attempt
/// with the typed values and an inline message (the AlertDialog closes on
/// response, so errors are shown in the rebuilt dialog).
fn present_add_folder_dialog(
    ctx: &SetupContext,
    previous: Option<(String, String)>,
    error: Option<String>,
) {
    let opencloud = ctx.state.borrow().provider == Provider::OpenCloud;
    let discovered_space = ctx.state.borrow().space_id.clone();
    if opencloud && discovered_space.is_none() {
        // The engine requires a space id; without one the folder cannot
        // sync, so block the dialog instead of accepting a doomed folder.
        present_folder_error(
            ctx,
            t("No OpenCloud space was discovered for this account. Sign in again to retry the discovery."),
        );
        return;
    }
    let (previous_local, previous_remote) = previous.unwrap_or_default();
    let local_default = if previous_local.is_empty() {
        default_sync_root().to_string_lossy().into_owned()
    } else {
        previous_local
    };

    let dialog = libadwaita::AlertDialog::new(
        Some(t("Add Folder")),
        Some(t(
            "Choose a local folder and an optional remote folder to mirror from this account.",
        )),
    );
    let entry_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    let local_entry = libadwaita::EntryRow::new();
    local_entry.set_title(t("Local folder"));
    local_entry.set_text(&local_default);
    let choose = gtk4::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk4::Align::Center)
        .css_classes(["flat"])
        .build();
    let entry_for_picker = local_entry.clone();
    choose.connect_clicked(move |_| {
        choose_local_folder(entry_for_picker.clone());
    });
    local_entry.add_suffix(&choose);
    entry_box.append(&local_entry);

    let remote_entry = libadwaita::EntryRow::new();
    remote_entry.set_title(t("Remote folder (optional, default /)"));
    remote_entry.set_text(if previous_remote.is_empty() {
        "/"
    } else {
        &previous_remote
    });
    entry_box.append(&remote_entry);

    if let Some(message) = error {
        entry_box.append(&error_label(&message));
    }

    dialog.set_extra_child(Some(&entry_box));
    dialog.add_response("cancel", t("Cancel"));
    dialog.add_response("add", t("Add"));
    dialog.set_response_appearance("add", libadwaita::ResponseAppearance::Suggested);

    let window = ctx.window.clone();
    let ctx = ctx.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response != "add" {
            return;
        }
        let local_root = local_entry.text().to_string();
        let remote_text = remote_entry.text().to_string();
        let provider = ctx.state.borrow().provider;
        let discovered_space = ctx.state.borrow().space_id.clone();
        match validate_add_folder(
            provider,
            &local_root,
            &remote_text,
            discovered_space.as_deref(),
        ) {
            Ok(folder) => {
                if ctx
                    .state
                    .borrow()
                    .folders
                    .iter()
                    .any(|item| item.local_root == folder.local_root)
                {
                    present_add_folder_dialog(
                        &ctx,
                        Some((local_root, remote_text)),
                        Some(t("This local folder is already added.").to_string()),
                    );
                    return;
                }
                ctx.state.borrow_mut().folders.push(folder.clone());
                append_folder_row(&ctx, &folder);
                ctx.widgets.folder_error.set_text("");
            }
            Err(message) => {
                present_add_folder_dialog(&ctx, Some((local_root, remote_text)), Some(message));
            }
        }
    });
    dialog.present(Some(&window));
}

fn append_folder_row(ctx: &SetupContext, folder: &WizardFolder) {
    let remote_label = if folder.remote_path.is_empty() {
        "/"
    } else {
        folder.remote_path.as_str()
    };
    let row = libadwaita::ActionRow::builder()
        .title(folder.local_root.as_str())
        .subtitle(t("Remote: {remote}").replacen("{remote}", remote_label, 1))
        .build();
    let icon = gtk4::Image::builder()
        .icon_name("folder-symbolic")
        .pixel_size(16)
        .build();
    row.add_prefix(&icon);
    let remove = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .valign(gtk4::Align::Center)
        .tooltip_text(t("Remove folder"))
        .css_classes(["flat"])
        .build();
    let list = ctx.widgets.folder_list.clone();
    let row_for_remove = row.clone();
    let local_root = folder.local_root.clone();
    let state = ctx.state.clone();
    remove.connect_clicked(move |_| {
        list.remove(&row_for_remove);
        state
            .borrow_mut()
            .folders
            .retain(|item| item.local_root != local_root);
    });
    row.add_suffix(&remove);
    ctx.widgets.folder_list.append(&row);
}

/// Rebuild the summary page from the current state and navigate to it.
fn folders_continue(ctx: &SetupContext) {
    ctx.widgets.folder_error.set_text("");
    {
        let state = ctx.state.borrow();
        ctx.widgets.summary_list.remove_all();
        append_summary_row(
            &ctx.widgets.summary_list,
            t("Server"),
            &state.server,
            "network-server-symbolic",
        );
        append_summary_row(
            &ctx.widgets.summary_list,
            t("Account"),
            &state.username,
            "avatar-default-symbolic",
        );
        if state.folders.is_empty() {
            append_summary_row(
                &ctx.widgets.summary_list,
                t("No Folders"),
                t("Connected without synchronization folders. Add them later from Settings."),
                "folder-symbolic",
            );
            ctx.widgets.start_button.set_label(t("Finish Setup"));
            ctx.widgets.summary_hint.set_text(
                t("The account will be connected without synchronizing any folder. You can add folders later from Settings."),
            );
        } else {
            ctx.widgets.start_button.set_label(t("Start Synchronizing"));
            ctx.widgets.summary_hint.set_text(
                t("The chosen folders will be mirrored in both directions using the Nextcloud synchronization engine."),
            );
            for folder in &state.folders {
                append_summary_row(
                    &ctx.widgets.summary_list,
                    t("Local Folder"),
                    &folder.local_root,
                    "folder-symbolic",
                );
                let remote = if folder.remote_path.is_empty() {
                    "/"
                } else {
                    folder.remote_path.as_str()
                };
                append_summary_row(
                    &ctx.widgets.summary_list,
                    t("Remote Folder"),
                    remote,
                    "folder-remote-symbolic",
                );
            }
        }
        if state.provider == Provider::OpenCloud {
            let space = state.space_id.as_deref().unwrap_or("Not set");
            append_summary_row(
                &ctx.widgets.summary_list,
                t("Space"),
                t(space),
                "drive-multidisk-symbolic",
            );
        }
        append_summary_row(
            &ctx.widgets.summary_list,
            t("Local Detection"),
            t("Filesystem monitor"),
            "folder-saved-search-symbolic",
        );
        append_summary_row(
            &ctx.widgets.summary_list,
            t("Remote Detection"),
            t("Server push + every 10 minutes"),
            "network-transmit-receive-symbolic",
        );
    }
    ctx.stack.set_visible_child_name("summary");
}

/// Start synchronization: without folders it finishes immediately; otherwise
/// it probes the first remote folder and asks for the first-sync confirmation.
fn start_syncing(ctx: &SetupContext) {
    if ctx.state.borrow().folders.is_empty() {
        finish_setup(ctx);
        return;
    }
    let (server, username) = {
        let state = ctx.state.borrow();
        (state.server.clone(), state.username.clone())
    };
    let account_id = account_id(&server, &username);
    let remote_path = ctx.state.borrow().folders[0].remote_path.clone();
    let provider = ctx.state.borrow().provider;
    let space_id = ctx.state.borrow().folders[0].space_id.clone();
    let server_for_probe = server.clone();
    let username_for_probe = username.clone();
    let probe = gio::spawn_blocking(move || -> Result<Option<bool>, ApiError> {
        let password = match CredentialsStore::get_for_account(
            &account_id,
            &server_for_probe,
            &username_for_probe,
        ) {
            Ok(Some(password)) => password,
            _ => return Ok(None),
        };
        if provider == Provider::OpenCloud {
            // OpenCloud folders mirror a whole space; the Nextcloud files/
            // tree does not exist there.
            let Some(space_id) = space_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                return Ok(None);
            };
            return NextcloudApi::new()
                .probe_opencloud_space(&server_for_probe, &username_for_probe, &password, space_id)
                .map(Some);
        }
        NextcloudApi::new()
            .probe_remote(
                &server_for_probe,
                &username_for_probe,
                &password,
                &remote_path,
            )
            .map(Some)
    });
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let probe_result = match probe.await {
            Ok(result) => result,
            Err(_) => Err(ApiError::Transport),
        };
        match probe_result {
            Ok(Some(has_children)) => {
                let remote_empty = !has_children;
                let local_empty = ctx
                    .state
                    .borrow()
                    .folders
                    .iter()
                    .all(|folder| local_folder_is_empty(&folder.local_root));
                present_first_sync_dialog(&ctx, &server, &username, local_empty, remote_empty);
            }
            Ok(None) | Err(_) => finish_setup(&ctx),
        }
    });
}

fn present_first_sync_dialog(
    ctx: &SetupContext,
    server: &str,
    username: &str,
    local_empty: bool,
    remote_empty: bool,
) {
    let account = format!("{username}@{server}");
    let (count, folder_label) = {
        let state = ctx.state.borrow();
        (state.folders.len(), folders_short_names(&state.folders))
    };
    // Issue #35: the review also inspects the local roots for engine
    // journals, so a previously synchronized folder is always confirmed and
    // a merge of two populated trees is stated explicitly.
    let journal_names = {
        let state = ctx.state.borrow();
        let mut names: Vec<String> = state
            .folders
            .iter()
            .flat_map(|folder| {
                crate::core::sync_safety::stale_artifact_names(&expanduser(&folder.local_root))
            })
            .collect();
        names.sort();
        names.dedup();
        names
    };
    let facts = crate::core::sync_safety::FirstSyncFacts {
        local_empty,
        remote_empty: Some(remote_empty),
        journal_names,
    };
    let body = first_sync_body(&account, count, &folder_label, local_empty, remote_empty);
    let local_roots: Vec<String> = ctx
        .state
        .borrow()
        .folders
        .iter()
        .map(|folder| folder.local_root.clone())
        .collect();
    let ctx_for_decision = ctx.clone();
    let on_decision = Rc::new(move |fresh: crate::ui::safety_review::FreshStart| {
        for root in &local_roots {
            crate::ui::safety_review::apply_fresh_start(root, fresh);
        }
        finish_setup(&ctx_for_decision);
    });
    let ctx_for_cancel = ctx.clone();
    crate::ui::safety_review::present_first_sync_review(
        ctx.window.upcast_ref::<gtk4::Widget>(),
        t("Start Synchronizing?"),
        &body,
        &facts,
        t("Back to setup"),
        on_decision,
        Rc::new(move || ctx_for_cancel.stack.set_visible_child_name("folders")),
    );
}

/// Create the local roots, persist the account and the TLS trust choice, then
/// invoke `on_complete` with the validated account and close the wizard.
fn finish_setup(ctx: &SetupContext) {
    let (provider, server, username, authentication_type, folders, trust_invalid) = {
        let state = ctx.state.borrow();
        (
            state.provider,
            state.server.clone(),
            state.username.clone(),
            state.authentication_type.clone(),
            state.folders.clone(),
            state.trust_invalid,
        )
    };
    for folder in &folders {
        let _ = std::fs::create_dir_all(expanduser(&folder.local_root));
    }
    let account = match build_account(provider, &server, &username, &authentication_type, &folders)
    {
        Ok(account) => account,
        Err(error) => {
            eprintln!("Setup: could not build the account: {error}");
            return;
        }
    };
    match ctx.config_store.add_account(&account) {
        Ok(account_id) => {
            if let Err(error) = persist_config(&ctx.config_store, |config| {
                config.network.trust_invalid_certificates = trust_invalid;
            }) {
                eprintln!("Setup: could not save network settings: {error}");
            }
            if let Some(validated) = ctx.config_store.account(&account_id).ok().flatten() {
                if let Some(callback) = &ctx.callbacks.on_complete {
                    callback(validated);
                }
            }
            ctx.window.close();
        }
        Err(error) => {
            let dialog = libadwaita::AlertDialog::new(
                Some(t("Could Not Add the Account")),
                Some(&error.to_string()),
            );
            dialog.add_response("ok", t("OK"));
            dialog.present(Some(&ctx.window));
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Parse the `opencloudcmd` "Listing spaces:" table into spaces.
///
/// The layout (`Short ID | DisplayName | ID`, pipe-separated, space-padded)
/// was verified in `opencloud-eu/desktop` `src/cmd/cmd.cpp` `printSpaces`. The
/// third column is the canonical space id that `opencloudcmd` accepts.
fn parse_spaces_list(output: &str) -> Vec<SpaceInfo> {
    let mut spaces = Vec::new();
    for line in output.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() != 3 {
            continue;
        }
        if cells[0] == "Short ID" {
            continue;
        }
        if cells.iter().all(|cell| cell.chars().all(|ch| ch == '-')) {
            continue;
        }
        if cells[1].is_empty() || cells[2].is_empty() {
            continue;
        }
        spaces.push(SpaceInfo {
            id: cells[2].to_string(),
            display_name: cells[1].to_string(),
        });
    }
    spaces
}

/// Build the validated account from the wizard inputs.
///
/// Mirrors what `ConfigStore::add_account` re-validates (normalized server,
/// trimmed login, recomputed account id); the folder ids are left empty and
/// recomputed by the store's validation.
fn build_account(
    provider: Provider,
    server: &str,
    username: &str,
    authentication_type: &str,
    folders: &[WizardFolder],
) -> Result<AccountConfig, ConfigError> {
    let server_url = normalize_server_url(server)?;
    let login_name = username.trim().to_string();
    let mut folder_configs = Vec::new();
    for folder in folders {
        let root = expanduser(&folder.local_root);
        if !root.is_absolute() {
            return Err(ConfigError::new(
                "The local synchronization folder must be absolute.",
            ));
        }
        let local_root = root.to_string_lossy().into_owned();
        let remote_path = normalize_remote_path(&folder.remote_path)?;
        let space_id = if provider == Provider::OpenCloud {
            let trimmed = folder.space_id.as_deref().map(str::trim).unwrap_or("");
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        } else {
            None
        };
        folder_configs.push(FolderConfig {
            id: String::new(),
            local_root,
            remote_path,
            space_id,
        });
    }
    Ok(AccountConfig {
        id: account_id(&server_url, &login_name),
        server_url,
        login_name,
        authentication_type: authentication_type.to_string(),
        provider,
        folders: folder_configs,
        ..AccountConfig::default()
    })
}

/// Normalize one Add Folder dialog submission into a [`WizardFolder`].
///
/// `space_id` is the space discovered for the account (OpenCloud only); it
/// is not user-editable: without a discovered space the dialog is blocked
/// upstream, so an `Ok` folder for OpenCloud always carries one.
fn validate_add_folder(
    provider: Provider,
    local_root: &str,
    remote_text: &str,
    space_id: Option<&str>,
) -> Result<WizardFolder, String> {
    let root = expanduser(local_root);
    if !root.is_absolute() {
        return Err(t("Choose an absolute local folder.").to_string());
    }
    let remote_path = normalize_remote_path(remote_text).map_err(|error| error.to_string())?;
    let space_id = if provider == Provider::OpenCloud {
        let trimmed = space_id.unwrap_or("").trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    } else {
        None
    };
    Ok(WizardFolder {
        local_root: root.to_string_lossy().into_owned(),
        remote_path,
        space_id,
    })
}

/// Comma-separated base names of the local folders (Python `", ".join(...)`).
fn folders_short_names(folders: &[WizardFolder]) -> String {
    folders
        .iter()
        .filter_map(|folder| std::path::Path::new(&folder.local_root).file_name())
        .filter_map(|name| name.to_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first-sync confirmation body, replicating the Python wording.
///
/// The catalog stores the whole sentence as a `{account}`/`{count}`/`{folders}`
/// (and `{conflict}`) template, so the translation is looked up first and the
/// placeholders substituted afterwards; in English `t` is the identity and
/// the output matches the source template verbatim.
fn first_sync_body(
    account: &str,
    count: usize,
    folders: &str,
    local_empty: bool,
    remote_empty: bool,
) -> String {
    let template = if local_empty && remote_empty {
        t("Connect {account} and start syncing {count} folder(s) ({folders}) now? Both sides are empty; synchronization will keep them in sync as empty mirrors.")
    } else if local_empty {
        t("Connect {account} and start syncing {count} folder(s) ({folders}) now? The remote folders already contain files; they will be downloaded.")
    } else if remote_empty {
        t("Connect {account} and start syncing {count} folder(s) ({folders}) now? The local folders already contain files; they will be uploaded.")
    } else {
        t("Connect {account} and start syncing {count} folder(s) ({folders}) now? Files that changed on both sides will be preserved as {conflict} (Nextcloud conflicted copy <date>).<ext>.")
    };
    fill_first_sync_template(template, account, count, folders).replacen("{conflict}", "{name}", 1)
}

/// Substitute the first-sync template placeholders.
fn fill_first_sync_template(template: &str, account: &str, count: usize, folders: &str) -> String {
    template
        .replacen("{account}", account, 1)
        .replacen("{count}", &count.to_string(), 1)
        .replacen("{folders}", folders, 1)
}

/// Whether the given provider's sync binary is missing, for the welcome banner.
fn update_provider_warning(banner: &libadwaita::Banner, provider: Provider) {
    match provider {
        Provider::Nextcloud if find_binary("nextcloudcmd").is_none() => {
            banner.set_title(
                t("nextcloudcmd is missing. Install the nextcloud-desktop-cmd package before the first synchronization."),
            );
            banner.set_revealed(true);
        }
        Provider::OpenCloud if find_binary("opencloudcmd").is_none() => {
            banner.set_title(
                t("opencloudcmd is missing. Install the OpenCloud desktop package before the first synchronization."),
            );
            banner.set_revealed(true);
        }
        _ => banner.set_revealed(false),
    }
}

// ---------------------------------------------------------------------------
// Widget helpers
// ---------------------------------------------------------------------------

/// The wizard page shell: a top-aligned `Clamp`-wrapped content column.
fn page() -> (gtk4::Box, gtk4::Box) {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let clamp = libadwaita::Clamp::builder()
        .maximum_size(480)
        .tightening_threshold(360)
        .vexpand(true)
        .build();
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(18)
        .margin_top(36)
        .margin_bottom(36)
        .margin_start(18)
        .margin_end(18)
        .build();
    clamp.set_child(Some(&content));
    outer.append(&clamp);
    (outer, content)
}

fn action_box() -> gtk4::Box {
    gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(12)
        .homogeneous(true)
        .build()
}

fn back_button(stack: &gtk4::Stack, page: &'static str) -> gtk4::Button {
    let button = gtk4::Button::with_label(t("Back"));
    button.set_tooltip_text(Some(t("Go back to the previous step")));
    let stack = stack.clone();
    button.connect_clicked(move |_| stack.set_visible_child_name(page));
    button
}

fn title_label(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["title-1"])
        .build()
}

fn dim_label(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build()
}

/// A wrap-aware centered label (the waiting-box texts).
fn centered_label(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .wrap(true)
        .justify(gtk4::Justification::Center)
        .build()
}

fn error_label(text: &str) -> gtk4::Label {
    gtk4::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .css_classes(["error"])
        .build()
}

fn provider_from_combo(row: &libadwaita::ComboRow) -> Provider {
    if row.selected() == 1 {
        Provider::OpenCloud
    } else {
        Provider::Nextcloud
    }
}

fn append_summary_row(list: &gtk4::ListBox, title: &str, subtitle: &str, icon: &str) {
    let row = libadwaita::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    if !icon.is_empty() {
        let image = gtk4::Image::builder()
            .icon_name(icon)
            .pixel_size(16)
            .build();
        row.add_prefix(&image);
    }
    list.append(&row);
}

/// Present a folder chooser and write the selection into the entry row.
fn choose_local_folder(entry: libadwaita::EntryRow) {
    let dialog = gtk4::FileDialog::builder()
        .title(t("Choose NextCloud Folder"))
        .modal(true)
        .build();
    dialog.select_folder(
        None::<&gtk4::Window>,
        None::<&gio::Cancellable>,
        move |result| {
            if let Ok(file) = result {
                if let Some(path) = file.path() {
                    entry.set_text(&path.to_string_lossy());
                }
            }
        },
    );
}

/// Load → mutate → save the top-level configuration.
fn persist_config(
    store: &ConfigStore,
    mutate: impl FnOnce(&mut Config),
) -> Result<(), ConfigError> {
    let mut config = store.load()?;
    mutate(&mut config);
    store.save(&config)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::i18n::{reset_locale, set_locale, Locale};
    use tempfile::tempdir;

    // ---- pure helpers ------------------------------------------------------

    #[test]
    fn parse_spaces_list_extracts_table_rows() {
        let output = concat!(
            "Listing spaces:\n",
            "Short ID | DisplayName | ID\n",
            "--------------- | -------------------- | --------------------\n",
            "space:abcd | Personal | space:abcd\n",
            "space:1234 | Photos   | 4f2f-9e1b\n",
        );
        let spaces = parse_spaces_list(output);
        assert_eq!(spaces.len(), 2);
        assert_eq!(spaces[0].id, "space:abcd");
        assert_eq!(spaces[0].display_name, "Personal");
        assert_eq!(spaces[1].id, "4f2f-9e1b");
        assert_eq!(spaces[1].display_name, "Photos");
    }

    #[test]
    fn parse_spaces_list_ignores_noise() {
        assert!(parse_spaces_list("").is_empty());
        assert!(parse_spaces_list("error: Token not set").is_empty());
        let header_only = concat!(
            "Listing spaces:\n",
            "Short ID | DisplayName | ID\n",
            "--------------- | -------------------- | --------------------\n",
        );
        assert!(parse_spaces_list(header_only).is_empty());
    }

    #[test]
    fn build_account_builds_nextcloud_account() {
        let account = build_account(
            Provider::Nextcloud,
            "https://cloud.example.com",
            "alice",
            "manual",
            &[WizardFolder {
                local_root: "/tmp/nsync-wizard".to_string(),
                remote_path: "/Docs/".to_string(),
                space_id: Some("space:should-be-dropped".to_string()),
            }],
        )
        .unwrap();
        assert_eq!(account.provider, Provider::Nextcloud);
        assert_eq!(account.server_url, "https://cloud.example.com");
        assert_eq!(account.login_name, "alice");
        assert_eq!(account.authentication_type, "manual");
        assert_eq!(account.folders.len(), 1);
        assert_eq!(account.folders[0].remote_path, "/Docs");
        assert_eq!(account.folders[0].space_id, None);
        assert_eq!(account.id, account_id("https://cloud.example.com", "alice"));
    }

    #[test]
    fn build_account_opencloud_keeps_space_id_and_trims() {
        let account = build_account(
            Provider::OpenCloud,
            "https://open.example.com",
            "bob",
            "manual",
            &[WizardFolder {
                local_root: "/tmp/nsync-oc".to_string(),
                remote_path: String::new(),
                space_id: Some(" space:42 ".to_string()),
            }],
        )
        .unwrap();
        assert_eq!(account.provider, Provider::OpenCloud);
        assert_eq!(account.folders[0].space_id.as_deref(), Some("space:42"));
        assert_eq!(account.folders[0].remote_path, "");
    }

    #[test]
    fn build_account_blank_space_id_maps_to_none() {
        let account = build_account(
            Provider::OpenCloud,
            "https://open.example.com",
            "bob",
            "manual",
            &[WizardFolder {
                local_root: "/tmp/nsync-oc2".to_string(),
                remote_path: "/".to_string(),
                space_id: Some("   ".to_string()),
            }],
        )
        .unwrap();
        assert_eq!(account.folders[0].space_id, None);
        assert_eq!(account.folders[0].remote_path, "");
    }

    #[test]
    fn build_account_rejects_invalid_server_and_relative_local_root() {
        assert!(build_account(Provider::Nextcloud, "example.com", "alice", "manual", &[]).is_err());
        let relative = build_account(
            Provider::Nextcloud,
            "https://cloud.example.com",
            "alice",
            "manual",
            &[WizardFolder {
                local_root: "relative".to_string(),
                remote_path: String::new(),
                space_id: None,
            }],
        );
        assert!(relative.is_err());
    }

    #[test]
    fn validate_add_folder_normalizes_inputs() {
        set_locale(Locale::English);
        let folder =
            validate_add_folder(Provider::Nextcloud, "/tmp/nsync-wz", "/Docs/", None).unwrap();
        assert_eq!(folder.local_root, "/tmp/nsync-wz");
        assert_eq!(folder.remote_path, "/Docs");
        assert_eq!(folder.space_id, None);
        let error = validate_add_folder(Provider::Nextcloud, "relative", "/", None).unwrap_err();
        assert!(error.contains("absolute"));
        reset_locale();
    }

    #[test]
    fn validate_add_folder_keeps_the_discovered_space_id() {
        let folder = validate_add_folder(
            Provider::OpenCloud,
            "/tmp/nsync-oc",
            "/",
            Some(" space:42 "),
        )
        .unwrap();
        assert_eq!(folder.space_id.as_deref(), Some("space:42"));
        assert_eq!(folder.remote_path, "");
        let missing =
            validate_add_folder(Provider::OpenCloud, "/tmp/nsync-oc2", "/", None).unwrap();
        assert_eq!(missing.space_id, None);
        // Nextcloud ignores the discovered space entirely.
        let nextcloud =
            validate_add_folder(Provider::Nextcloud, "/tmp/nsync-nc", "/", Some("space:42"))
                .unwrap();
        assert_eq!(nextcloud.space_id, None);
    }

    #[test]
    fn local_folder_is_empty_detects_emptiness() {
        let dir = tempdir().unwrap();
        assert!(local_folder_is_empty(dir.path().to_str().unwrap()));
        std::fs::write(dir.path().join("file.txt"), "x").unwrap();
        assert!(!local_folder_is_empty(dir.path().to_str().unwrap()));
        assert!(!local_folder_is_empty("/nonexistent-path-xyz"));
    }

    #[test]
    fn folders_short_names_joins_folder_names() {
        let folders = vec![
            WizardFolder {
                local_root: "/tmp/A".to_string(),
                remote_path: String::new(),
                space_id: None,
            },
            WizardFolder {
                local_root: "/tmp/B".to_string(),
                remote_path: String::new(),
                space_id: None,
            },
        ];
        assert_eq!(folders_short_names(&folders), "A, B");
    }

    #[test]
    fn first_sync_body_both_empty() {
        set_locale(Locale::English);
        let body = first_sync_body(
            "alice@https://cloud.example.com",
            1,
            "NextCloud",
            true,
            true,
        );
        assert_eq!(
            body,
            "Connect alice@https://cloud.example.com and start syncing 1 folder(s) (NextCloud) now? Both sides are empty; synchronization will keep them in sync as empty mirrors."
        );
        reset_locale();
    }

    #[test]
    fn first_sync_body_local_empty_downloads() {
        set_locale(Locale::English);
        let body = first_sync_body(
            "alice@https://cloud.example.com",
            1,
            "NextCloud",
            true,
            false,
        );
        assert_eq!(
            body,
            "Connect alice@https://cloud.example.com and start syncing 1 folder(s) (NextCloud) now? The remote folders already contain files; they will be downloaded."
        );
        reset_locale();
    }

    #[test]
    fn first_sync_body_remote_empty_uploads() {
        set_locale(Locale::English);
        let body = first_sync_body(
            "alice@https://cloud.example.com",
            1,
            "NextCloud",
            false,
            true,
        );
        assert_eq!(
            body,
            "Connect alice@https://cloud.example.com and start syncing 1 folder(s) (NextCloud) now? The local folders already contain files; they will be uploaded."
        );
        reset_locale();
    }

    #[test]
    fn first_sync_body_both_populated_mentions_conflicts() {
        set_locale(Locale::English);
        let body = first_sync_body("bob@https://cloud.example.com", 2, "A, B", false, false);
        assert_eq!(
            body,
            "Connect bob@https://cloud.example.com and start syncing 2 folder(s) (A, B) now? Files that changed on both sides will be preserved as {name} (Nextcloud conflicted copy <date>).<ext>."
        );
        reset_locale();
    }

    // ---- i18n --------------------------------------------------------------

    #[test]
    fn wizard_title_translates_to_spanish_and_back() {
        set_locale(Locale::Spanish);
        assert_eq!(t(WINDOW_TITLE), "Configurar NextSync");
        assert_eq!(t("Start Synchronizing?"), "¿Empezar a sincronizar?");
        set_locale(Locale::English);
        assert_eq!(t(WINDOW_TITLE), "Set Up NextSync");
        assert_eq!(t("Start Synchronizing?"), "Start Synchronizing?");
        reset_locale();
    }

    #[test]
    fn first_sync_body_translates_the_catalog_template() {
        set_locale(Locale::Spanish);
        let body = first_sync_body("alice@https://cloud.example.com", 2, "A, B", true, false);
        assert_eq!(
            body,
            "¿Conectar alice@https://cloud.example.com y empezar a sincronizar 2 carpeta(s) (A, B)? Las carpetas remotas ya contienen archivos; se descargarán."
        );
        let conflict = first_sync_body("bob@https://cloud.example.com", 1, "Docs", false, false);
        assert!(conflict.starts_with("¿Conectar bob@https://cloud.example.com"));
        assert!(conflict
            .contains("se conservarán como {name} (Nextcloud conflicted copy <date>).<ext>."));
        set_locale(Locale::English);
        reset_locale();
    }

    // ---- browser sign-in state ----------------------------------------------

    fn live_browser_state() -> BrowserFlowState {
        BrowserFlowState {
            start: Some(LoginFlowStart {
                poll_endpoint: "https://cloud.example.com/index.php/login/v2/poll".to_string(),
                poll_token: "token".to_string(),
                login_url: "https://cloud.example.com/index.php/login/v2/flow/X".to_string(),
            }),
            ..BrowserFlowState::default()
        }
    }

    #[test]
    fn begin_poll_issues_polls_for_the_live_generation() {
        let mut state = live_browser_state();
        assert_eq!(state.begin_poll(0), PollDecision::Poll);
        assert_eq!(state.poll_count, 1);
        assert!(state.poll_in_flight);
        // Overlapping ticks are skipped while a poll is in flight.
        assert_eq!(state.begin_poll(0), PollDecision::Skip);
        state.poll_in_flight = false;
        assert_eq!(state.begin_poll(0), PollDecision::Poll);
        assert_eq!(state.poll_count, 2);
    }

    #[test]
    fn begin_poll_stops_for_a_stale_generation() {
        let mut state = live_browser_state();
        state.cancel();
        assert_eq!(state.begin_poll(0), PollDecision::Stop);
        assert_eq!(state.generation, 1);
        assert!(state.start.is_none());
    }

    #[test]
    fn begin_poll_expires_after_the_twenty_minute_budget() {
        let mut state = live_browser_state();
        state.poll_in_flight = false;
        state.poll_count = MAX_POLLS;
        assert_eq!(state.begin_poll(0), PollDecision::Stop);
        assert!(
            state.start.is_none(),
            "the session must be dropped on expiry"
        );
        // A fresh flow starts counting from zero.
        let mut fresh = live_browser_state();
        assert_eq!(fresh.begin_poll(0), PollDecision::Poll);
        assert_eq!(fresh.poll_count, 1);
    }

    #[test]
    fn begin_poll_stops_without_a_session() {
        let mut state = BrowserFlowState::default();
        assert_eq!(state.begin_poll(0), PollDecision::Stop);
    }

    #[test]
    fn browser_flow_messages_translate_to_spanish() {
        set_locale(Locale::Spanish);
        assert_eq!(
            t("Browser authorization expired after 20 minutes."),
            "La autorización del navegador caducó tras 20 minutos."
        );
        assert_eq!(
            t("Could not store the account password: {message}").replacen("{message}", "locked", 1),
            "No se pudo guardar la contraseña de la cuenta: locked"
        );
        assert_eq!(
            t("Browser sign-in was cancelled."),
            "Se canceló el inicio de sesión con el navegador."
        );
        set_locale(Locale::English);
        reset_locale();
    }

    // ---- GTK smoke --------------------------------------------------------

    #[test]
    fn setup_window_construction_smoke() {
        crate::ui::test_helpers::gtk_smoke(|| {
            // The ambient environment is Spanish (LANG=es_ES.UTF-8); pin the
            // locale on the GTK worker thread so the title assertion is
            // deterministic.
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let dir = tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));
            let window = SetupWindow::new(&app, store, SetupCallbacks::default());
            assert_eq!(
                window.window().title().unwrap_or_default().to_string(),
                WINDOW_TITLE
            );
            reset_locale();
        });
    }

    #[test]
    fn browser_sign_in_row_is_present_for_nextcloud() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let dir = tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));
            let window = SetupWindow::new(&app, store, SetupCallbacks::default());
            let widgets = window.widgets();
            assert_eq!(widgets.browser_row.title().as_str(), "Sign in with browser");
            assert_eq!(
                widgets.browser_row.subtitle().unwrap_or_default().as_str(),
                "Recommended. Supports two-factor authentication."
            );
            assert!(widgets.browser_row.is_activatable());
            assert!(widgets.browser_group.get_visible());
            assert!(!widgets.waiting_box.get_visible());
            assert_eq!(widgets.login_url_label.text().as_str(), "");
            reset_locale();
        });
    }

    #[test]
    fn browser_sign_in_row_is_hidden_for_opencloud() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let dir = tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));
            let window = SetupWindow::new(&app, store, SetupCallbacks::default());
            window.configure_authentication_for(Provider::OpenCloud);
            assert!(!window.widgets().browser_group.get_visible());
            assert!(!window.widgets().waiting_box.get_visible());
            // Switching back restores the row.
            window.configure_authentication_for(Provider::Nextcloud);
            assert!(window.widgets().browser_group.get_visible());
            reset_locale();
        });
    }

    #[test]
    fn login_url_label_is_ellipsized_and_keeps_the_full_url() {
        crate::ui::test_helpers::gtk_smoke(|| {
            set_locale(Locale::English);
            let app = libadwaita::Application::builder()
                .application_id("io.github.gnacho.nextsync")
                .build();
            let dir = tempdir().unwrap();
            let store = ConfigStore::with_path(dir.path().join("settings.json"));
            let window = SetupWindow::new(&app, store, SetupCallbacks::default());
            let label = window.widgets().login_url_label.clone();
            assert_eq!(label.ellipsize(), gtk4::pango::EllipsizeMode::End);
            assert!(
                label.max_width_chars() > 0,
                "the login URL label must not stretch the window"
            );
            assert!(!label.wraps(), "wrapping defeats ellipsis");
            assert!(label.is_selectable());
            // The whole URL stays on the widget so "Copy Link" (which reads
            // `label.text()`) copies the complete link even when truncated.
            let url = "https://cloud.example.com/index.php/login/v2/flow?token=AVeryLongTokenValue";
            label.set_text(url);
            assert_eq!(label.text().as_str(), url);
            reset_locale();
        });
    }
}
