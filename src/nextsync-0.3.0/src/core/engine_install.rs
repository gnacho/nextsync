//! Install plans for the provider's sync engine package (issue #218).
//!
//! NextSync has no sync engine of its own: it delegates to `nextcloudcmd`
//! (Nextcloud) or `opencloudcmd` (OpenCloud). When the binary is missing the
//! app can now offer one-click installation instead of sending the user to a
//! terminal. This module is the pure, testable core of that feature: given the
//! provider and the package manager detected on the host, it returns the
//! exact `argv` to run under `pkexec` (fixed arguments, never a shell string,
//! never user input) plus a display/copyable form of the same command. The UI
//! layer owns the dialog, the spawn and the retry.
//!
//! Package names were verified against the distro indexes (2026-09):
//! `nextcloud-client` on Arch/Fedora/openSUSE provides `nextcloudcmd`;
//! Debian/Ubuntu split it into `nextcloud-desktop-cmd`; OpenCloud ships as
//! `opencloud-desktop` (Arch repos on Arch derivatives, AUR on plain Arch) and
//! is not packaged by apt/dnf/zypper yet, so those combinations fall back to
//! manual guidance.

use crate::nextcloud::command::find_binary;
use crate::nextcloud::driver::Provider;

/// A system package manager able to install the engine package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// Arch Linux and derivatives (`pacman -S`).
    Pacman,
    /// Debian/Ubuntu and derivatives (`apt-get install`).
    Apt,
    /// Fedora and derivatives (`dnf install`).
    Dnf,
    /// openSUSE (`zypper install`).
    Zypper,
}

impl PackageManager {
    /// Stable lowercase name (logs, tests).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pacman => "pacman",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Zypper => "zypper",
        }
    }
}

/// An AUR helper usable as a manual fallback for AUR-only packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurHelper {
    Paru,
    Yay,
}

impl AurHelper {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paru => "paru",
            Self::Yay => "yay",
        }
    }
}

/// Presence of the package-management tools on the host.
///
/// Pure data: production builds it from `$PATH` lookups via [`HostFacts::detect`],
/// tests hand-roll it, so the policy below never touches the filesystem.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFacts {
    pub pacman: bool,
    pub apt: bool,
    pub dnf: bool,
    pub zypper: bool,
    pub paru: bool,
    pub yay: bool,
}

impl HostFacts {
    /// Probe the host for package managers and AUR helpers.
    pub fn detect() -> Self {
        Self {
            pacman: find_binary("pacman").is_some(),
            apt: find_binary("apt-get").is_some(),
            dnf: find_binary("dnf").is_some(),
            zypper: find_binary("zypper").is_some(),
            paru: find_binary("paru").is_some(),
            yay: find_binary("yay").is_some(),
        }
    }

    /// The package manager to drive, by direct evidence (the install binary
    /// exists on `$PATH`). Priority mirrors the distro families above; hosts
    /// with several tools (containers) pick the first match.
    pub fn package_manager(&self) -> Option<PackageManager> {
        if self.pacman {
            Some(PackageManager::Pacman)
        } else if self.apt {
            Some(PackageManager::Apt)
        } else if self.dnf {
            Some(PackageManager::Dnf)
        } else if self.zypper {
            Some(PackageManager::Zypper)
        } else {
            None
        }
    }

    /// An AUR helper, when present (`paru` preferred over `yay`).
    pub fn aur_helper(&self) -> Option<AurHelper> {
        if self.paru {
            Some(AurHelper::Paru)
        } else if self.yay {
            Some(AurHelper::Yay)
        } else {
            None
        }
    }
}

/// How the app can get the provider's engine installed on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPlan {
    /// Run `argv` (already prefixed with `pkexec`) with administrator rights.
    /// `command` is the same invocation as display/copy text.
    Automatic { argv: Vec<String>, command: String },
    /// No automatic route for this combination. `command` is what to run in a
    /// terminal, when one is known; `None` means generic manual guidance.
    Manual { command: Option<String> },
}

impl InstallPlan {
    /// The display/copyable command, when there is one.
    pub fn command(&self) -> Option<&str> {
        match self {
            Self::Automatic { command, .. } => Some(command),
            Self::Manual { command } => command.as_deref(),
        }
    }
}

/// Build the install plan for one provider on the given host.
///
/// The argv is fixed per (provider, package manager): no user input ever
/// reaches it, so it can be spawned directly without a shell.
pub fn install_plan(provider: Provider, facts: &HostFacts) -> InstallPlan {
    let Some(pm) = facts.package_manager() else {
        return manual_without_package_manager(provider, facts);
    };
    let Some(package) = package_for(provider, pm) else {
        // The package manager is supported but does not package this engine
        // (OpenCloud on apt/dnf/zypper today): point at a manual route.
        return manual_without_package_manager(provider, facts);
    };
    match pm {
        PackageManager::Pacman => automatic(&["pacman", "-S", "--needed", "--noconfirm", package]),
        PackageManager::Apt => automatic(&["apt-get", "install", "-y", package]),
        PackageManager::Dnf => automatic(&["dnf", "install", "-y", package]),
        PackageManager::Zypper => automatic(&["zypper", "--non-interactive", "install", package]),
    }
}

/// The fallback when no automatic route exists. OpenCloud is AUR-only on
/// Arch hosts, so an AUR helper (when present) is the best manual pointer.
fn manual_without_package_manager(provider: Provider, facts: &HostFacts) -> InstallPlan {
    let command = match (provider, facts.aur_helper()) {
        (Provider::OpenCloud, Some(helper)) => Some(format!(
            "{} -S --noconfirm opencloud-desktop",
            helper.as_str()
        )),
        _ => None,
    };
    InstallPlan::Manual { command }
}

/// The distro package that provides the provider's engine binary, when the
/// distro packages it at all.
fn package_for(provider: Provider, pm: PackageManager) -> Option<&'static str> {
    match (provider, pm) {
        (Provider::Nextcloud, PackageManager::Pacman) => Some("nextcloud-client"),
        (Provider::Nextcloud, PackageManager::Apt) => Some("nextcloud-desktop-cmd"),
        (Provider::Nextcloud, PackageManager::Dnf) => Some("nextcloud-client"),
        (Provider::Nextcloud, PackageManager::Zypper) => Some("nextcloud-client"),
        // `opencloud-desktop` is in the Arch repos (and AUR on plain Arch);
        // apt/dnf/zypper do not package it yet.
        (Provider::OpenCloud, PackageManager::Pacman) => Some("opencloud-desktop"),
        (Provider::OpenCloud, _) => None,
    }
}

/// Wrap a package-manager invocation in `pkexec` and precompute the display
/// form of the same command.
fn automatic(argv: &[&'static str]) -> InstallPlan {
    let argv: Vec<String> = std::iter::once("pkexec".to_string())
        .chain(argv.iter().map(|arg| (*arg).to_string()))
        .collect();
    let command = argv.join(" ");
    InstallPlan::Automatic { argv, command }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(pacman: bool, apt: bool, dnf: bool, zypper: bool) -> HostFacts {
        HostFacts {
            pacman,
            apt,
            dnf,
            zypper,
            paru: false,
            yay: false,
        }
    }

    // ---- detection ---------------------------------------------------------

    #[test]
    fn package_manager_is_detected_by_binary_presence_in_priority_order() {
        assert_eq!(
            facts(true, true, false, false).package_manager(),
            Some(PackageManager::Pacman),
            "pacman wins on Arch hosts that also see other tools"
        );
        assert_eq!(
            facts(false, true, false, false).package_manager(),
            Some(PackageManager::Apt)
        );
        assert_eq!(
            facts(false, false, true, false).package_manager(),
            Some(PackageManager::Dnf)
        );
        assert_eq!(
            facts(false, false, false, true).package_manager(),
            Some(PackageManager::Zypper)
        );
        assert_eq!(facts(false, false, false, false).package_manager(), None);
    }

    #[test]
    fn aur_helper_prefers_paru_over_yay() {
        let mut host = facts(false, false, false, false);
        assert_eq!(host.aur_helper(), None);
        host.yay = true;
        assert_eq!(host.aur_helper(), Some(AurHelper::Yay));
        host.paru = true;
        assert_eq!(host.aur_helper(), Some(AurHelper::Paru));
    }

    // ---- install plans -----------------------------------------------------

    #[test]
    fn nextcloud_on_pacman_installs_nextcloud_client_via_pkexec() {
        let plan = install_plan(Provider::Nextcloud, &facts(true, false, false, false));
        let InstallPlan::Automatic { argv, command } = plan else {
            panic!("expected an automatic plan, got {plan:?}");
        };
        assert_eq!(
            argv,
            vec![
                "pkexec",
                "pacman",
                "-S",
                "--needed",
                "--noconfirm",
                "nextcloud-client",
            ]
        );
        assert_eq!(
            command,
            "pkexec pacman -S --needed --noconfirm nextcloud-client"
        );
    }

    #[test]
    fn nextcloud_on_apt_installs_the_cmd_split_package() {
        let plan = install_plan(Provider::Nextcloud, &facts(false, true, false, false));
        let InstallPlan::Automatic { argv, command } = plan else {
            panic!("expected an automatic plan, got {plan:?}");
        };
        assert_eq!(
            argv,
            vec![
                "pkexec",
                "apt-get",
                "install",
                "-y",
                "nextcloud-desktop-cmd"
            ]
        );
        assert_eq!(command, "pkexec apt-get install -y nextcloud-desktop-cmd");
    }

    #[test]
    fn nextcloud_on_dnf_and_zypper_install_nextcloud_client() {
        let dnf = install_plan(Provider::Nextcloud, &facts(false, false, true, false));
        let InstallPlan::Automatic { argv, .. } = dnf else {
            panic!("expected an automatic plan, got {dnf:?}");
        };
        assert_eq!(
            argv,
            vec!["pkexec", "dnf", "install", "-y", "nextcloud-client"]
        );

        let zypper = install_plan(Provider::Nextcloud, &facts(false, false, false, true));
        let InstallPlan::Automatic { argv, .. } = zypper else {
            panic!("expected an automatic plan, got {zypper:?}");
        };
        assert_eq!(
            argv,
            vec![
                "pkexec",
                "zypper",
                "--non-interactive",
                "install",
                "nextcloud-client"
            ]
        );
    }

    #[test]
    fn opencloud_on_pacman_installs_opencloud_desktop() {
        let plan = install_plan(Provider::OpenCloud, &facts(true, false, false, false));
        let InstallPlan::Automatic { argv, command } = plan else {
            panic!("expected an automatic plan, got {plan:?}");
        };
        assert_eq!(
            argv,
            vec![
                "pkexec",
                "pacman",
                "-S",
                "--needed",
                "--noconfirm",
                "opencloud-desktop"
            ]
        );
        assert_eq!(
            command,
            "pkexec pacman -S --needed --noconfirm opencloud-desktop"
        );
    }

    #[test]
    fn opencloud_has_no_automatic_route_on_apt_dnf_or_zypper() {
        for host in [
            facts(false, true, false, false),
            facts(false, false, true, false),
            facts(false, false, false, true),
        ] {
            let plan = install_plan(Provider::OpenCloud, &host);
            assert_eq!(plan, InstallPlan::Manual { command: None });
        }
    }

    #[test]
    fn nextcloud_without_a_package_manager_falls_back_to_manual_guidance() {
        let plan = install_plan(Provider::Nextcloud, &facts(false, false, false, false));
        assert_eq!(plan, InstallPlan::Manual { command: None });
    }

    #[test]
    fn opencloud_without_a_package_manager_points_at_an_aur_helper() {
        let mut host = facts(false, false, false, false);
        host.yay = true;
        let plan = install_plan(Provider::OpenCloud, &host);
        assert_eq!(
            plan,
            InstallPlan::Manual {
                command: Some("yay -S --noconfirm opencloud-desktop".to_string())
            }
        );

        host.paru = true;
        let plan = install_plan(Provider::OpenCloud, &host);
        assert_eq!(
            plan,
            InstallPlan::Manual {
                command: Some("paru -S --noconfirm opencloud-desktop".to_string())
            }
        );
    }

    #[test]
    fn plans_never_interpolate_user_input_and_name_packages_not_binaries() {
        // The provider is the only variable; every argv element must come
        // from the fixed tables above (no paths, no free text).
        for provider in [Provider::Nextcloud, Provider::OpenCloud] {
            for host in [
                facts(true, false, false, false),
                facts(false, true, false, false),
                facts(false, false, true, false),
                facts(false, false, false, true),
            ] {
                let plan = install_plan(provider, &host);
                if let InstallPlan::Automatic { argv, .. } = &plan {
                    for arg in argv {
                        assert!(
                            !arg.contains(' '),
                            "argv elements are single tokens: {argv:?}"
                        );
                        assert!(!arg.contains('/'), "no paths in argv: {argv:?}");
                        assert_ne!(arg, "pkexec pacman");
                    }
                    assert_eq!(argv[0], "pkexec", "pkexec is the elevation prefix");
                    assert!(!argv.contains(&"nextcloudcmd".to_string()));
                    assert!(!argv.contains(&"opencloudcmd".to_string()));
                }
                if let Some(command) = plan.command() {
                    assert!(!command.is_empty());
                }
            }
        }
    }
}
