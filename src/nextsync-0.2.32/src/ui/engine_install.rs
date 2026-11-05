//! "Install the sync engine" dialog (issue #218).
//!
//! Wherever the user meets the missing-engine state (the setup wizard's
//! finish gate, a folder row in `EngineMissing` error), this dialog offers
//! the one-click route: the exact `pkexec <pkg-manager> …` plan from
//! [`crate::core::engine_install`], or the command/guidance to do it by hand
//! when no automatic route exists. Nothing runs until the user clicks
//! Install, and after a successful install the caller's `on_installed`
//! callback re-checks the binary and retries the folder.

use std::cell::Cell;
use std::rc::Rc;

use libadwaita::prelude::*;

use crate::core::engine_install::{install_plan, HostFacts, InstallPlan};
use crate::nextcloud::driver::Provider;
use crate::ui::setup::{engine_install_hint, engine_present_for};
use crate::util::i18n::t;

/// The translated dialog body for one provider/plan pair (pure, testable).
pub(crate) fn install_dialog_text(provider: Provider, plan: &InstallPlan) -> String {
    let hint = engine_install_hint(provider, false).unwrap_or_default();
    match plan {
        InstallPlan::Automatic { command, .. } => {
            format!(
                "{hint}\n\n{}\n{command}",
                t("This command will run with administrator rights:")
            )
        }
        InstallPlan::Manual {
            command: Some(command),
        } => format!(
            "{hint}\n\n{}\n{command}",
            t("Run this command in a terminal:")
        ),
        InstallPlan::Manual { command: None } => format!(
            "{hint}\n\n{}",
            t("No automatic installation is available for this distribution yet.")
        ),
    }
}

/// The translated failure body: what went wrong plus the manual fallback.
pub(crate) fn install_failure_text(
    provider: Provider,
    plan: &InstallPlan,
    code: Option<i32>,
    stderr: &str,
) -> String {
    let mut parts = vec![t("The installation failed.").to_string()];
    if let Some(code) = code {
        parts.push(t("Exit code: {code}").replace("{code}", &code.to_string()));
    }
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        // Keep the tail only: package-manager output is long and the end
        // carries the actual error.
        let tail: String = stderr
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        parts.push(tail.to_string());
    }
    if let Some(command) = plan.command() {
        parts.push(t("Run this command in a terminal:").to_string());
        parts.push(command.to_string());
    }
    if provider == Provider::OpenCloud {
        parts.push(
            t("On distributions without this package, install it from the AUR (for example: yay -S opencloud-desktop).")
                .to_string(),
        );
    }
    parts.join("\n\n")
}

/// Present the install dialog transient for `parent`.
///
/// `on_installed` runs on the UI thread after the plan succeeded and the
/// engine binary is visible on `$PATH` (the caller retries the folder or,
/// in the wizard, tells the user they can finish the setup).
pub fn present_engine_install_dialog(
    parent: &impl IsA<gtk4::Widget>,
    provider: Provider,
    on_installed: Rc<dyn Fn()>,
) {
    let facts = HostFacts::detect();
    let plan = install_plan(provider, &facts);
    let dialog = libadwaita::AlertDialog::new(
        Some(t("Sync Engine Not Installed")),
        Some(&install_dialog_text(provider, &plan)),
    );
    dialog.add_response("cancel", t("Cancel"));
    let can_install = matches!(plan, InstallPlan::Automatic { .. });
    if can_install {
        dialog.add_response("install", t("Install"));
        dialog.set_response_appearance("install", libadwaita::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("install"));
    }
    if plan.command().is_some() {
        dialog.add_response("copy", t("Copy Command"));
    }

    let in_flight = Rc::new(Cell::new(false));
    let on_installed = Rc::new(on_installed);
    dialog.connect_response(None, move |dialog, response| {
        match response {
            "copy" => {
                if let Some(display) = gtk4::gdk::Display::default() {
                    display
                        .clipboard()
                        .set_text(plan.command().unwrap_or_default());
                }
                dialog.set_body(t("Command copied to the clipboard."));
            }
            "install" => {
                // Guard against a second click while the installer runs.
                if in_flight.replace(true) {
                    return;
                }
                let InstallPlan::Automatic { argv, .. } = plan.clone() else {
                    return;
                };
                dialog.set_body(t("Installing…"));
                let handle = gio::spawn_blocking(move || {
                    let mut command = std::process::Command::new(&argv[0]);
                    command.args(&argv[1..]).output()
                });
                let dialog_w = dialog.clone();
                let plan_w = plan.clone();
                let on_installed = on_installed.clone();
                glib::spawn_future_local(async move {
                    let outcome = handle.await;
                    // The binary may sit in a directory the running session
                    // did not have on `$PATH`; re-check before declaring
                    // success and keep the manual fallback otherwise.
                    let ran_ok = matches!(&outcome, Ok(Ok(output)) if output.status.success());
                    if ran_ok && engine_present_for(provider) {
                        dialog_w.force_close();
                        on_installed();
                        return;
                    }
                    let (code, stderr) = match outcome {
                        Ok(Ok(output)) => (
                            output.status.code(),
                            String::from_utf8_lossy(&output.stderr).into_owned(),
                        ),
                        Ok(Err(error)) => (None, error.to_string()),
                        // The blocking task panicked (not a spawn failure of
                        // the package manager itself).
                        Err(_) => (None, t("The installer crashed.").to_string()),
                    };
                    present_install_failure_dialog(&dialog_w, provider, &plan_w, code, &stderr);
                });
            }
            _ => {}
        }
    });

    dialog.present(Some(parent));
}

/// The failure path: what happened, and the manual command as fallback.
fn present_install_failure_dialog(
    parent: &impl IsA<gtk4::Widget>,
    provider: Provider,
    plan: &InstallPlan,
    code: Option<i32>,
    stderr: &str,
) {
    let dialog = libadwaita::AlertDialog::new(
        Some(t("Installation Failed")),
        Some(&install_failure_text(provider, plan, code, stderr)),
    );
    if let Some(command) = plan.command() {
        dialog.add_response("copy", t("Copy Command"));
        let command = command.to_string();
        dialog.connect_response(None, move |dialog, response| {
            if response == "copy" {
                if let Some(display) = gtk4::gdk::Display::default() {
                    display.clipboard().set_text(&command);
                }
                dialog.set_body(t("Command copied to the clipboard."));
            }
        });
    }
    dialog.add_response("ok", t("OK"));
    dialog.present(Some(parent));
}

/// The wizard's follow-up once the engine landed: finish is now unblocked.
pub fn present_engine_installed_dialog(parent: &impl IsA<gtk4::Widget>) {
    let dialog = libadwaita::AlertDialog::new(
        Some(t("Sync Engine Installed")),
        Some(t(
            "The sync engine was installed. You can finish the setup now.",
        )),
    );
    dialog.add_response("ok", t("OK"));
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::engine_install::HostFacts;
    use crate::util::i18n::{reset_locale, set_locale, Locale};

    fn automatic(provider: Provider) -> InstallPlan {
        install_plan(
            provider,
            &HostFacts {
                pacman: true,
                ..HostFacts::default()
            },
        )
    }

    #[test]
    fn dialog_text_names_the_command_for_automatic_plans() {
        set_locale(Locale::English);
        let text = install_dialog_text(Provider::Nextcloud, &automatic(Provider::Nextcloud));
        assert!(text.contains("nextcloud-client"));
        assert!(text.contains("This command will run with administrator rights:"));
        assert!(text.contains("pkexec pacman -S --needed --noconfirm nextcloud-client"));
        reset_locale();
    }

    #[test]
    fn dialog_text_offers_the_terminal_command_for_manual_plans() {
        set_locale(Locale::English);
        let plan = InstallPlan::Manual {
            command: Some("yay -S --noconfirm opencloud-desktop".to_string()),
        };
        let text = install_dialog_text(Provider::OpenCloud, &plan);
        assert!(text.contains("Run this command in a terminal:"));
        assert!(text.contains("yay -S --noconfirm opencloud-desktop"));
        // No automatic route at all: generic guidance, no command.
        let text = install_dialog_text(Provider::Nextcloud, &InstallPlan::Manual { command: None });
        assert!(text.contains("No automatic installation is available"));
        reset_locale();
    }

    #[test]
    fn failure_text_carries_the_code_the_error_tail_and_the_fallback() {
        set_locale(Locale::English);
        let plan = automatic(Provider::Nextcloud);
        let text = install_failure_text(
            Provider::Nextcloud,
            &plan,
            Some(1),
            "error: target not found",
        );
        assert!(text.contains("The installation failed."));
        assert!(text.contains("Exit code: 1"));
        assert!(text.contains("error: target not found"));
        assert!(text.contains("pkexec pacman -S --needed --noconfirm nextcloud-client"));
        // OpenCloud failures add the AUR hint.
        let text = install_failure_text(Provider::OpenCloud, &plan, None, "");
        assert!(text.contains("install it from the AUR"));
        reset_locale();
    }
}
