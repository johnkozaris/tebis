//! Shared package-manager detection + install-command rendering.
//!
//! phonemizer.rs and onnxruntime.rs both drive `PackageManager` to install
//! a different package. Extract the enum + detection + shared command
//! rendering here. Each ensurer keeps its own probe/output flow.

use std::path::PathBuf;

/// Platform package managers we drive.
#[allow(dead_code, reason = "Linux-only variants compile on macOS for testing")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    Brew,
    MacPorts,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
}

impl PackageManager {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Brew => "brew",
            Self::MacPorts => "port",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Pacman => "pacman",
            Self::Zypper => "zypper",
            Self::Apk => "apk",
        }
    }
}

/// First supported package manager on PATH. `None` → manual install.
pub fn detect_package_manager() -> Option<PackageManager> {
    #[cfg(target_os = "macos")]
    {
        if binary_on_path("brew") {
            return Some(PackageManager::Brew);
        }
        if binary_on_path("port") {
            return Some(PackageManager::MacPorts);
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        for (pm, bin) in [
            (PackageManager::Apt, "apt-get"),
            (PackageManager::Dnf, "dnf"),
            (PackageManager::Pacman, "pacman"),
            (PackageManager::Zypper, "zypper"),
            (PackageManager::Apk, "apk"),
        ] {
            if binary_on_path(bin) {
                return Some(pm);
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn binary_on_path(name: &str) -> bool {
    crate::audio::espeak::which_in_path(name).is_some()
}

/// Display form — mirror `install_argv` joined by spaces.
pub fn install_cmd_display(pm: PackageManager, pkg: &str) -> String {
    match pm {
        PackageManager::Brew => format!("brew install {pkg}"),
        PackageManager::MacPorts => format!("sudo port install {pkg}"),
        PackageManager::Apt => format!("sudo apt install -y {pkg}"),
        PackageManager::Dnf => format!("sudo dnf install -y {pkg}"),
        PackageManager::Pacman => format!("sudo pacman -S --noconfirm {pkg}"),
        PackageManager::Zypper => format!("sudo zypper install -y {pkg}"),
        PackageManager::Apk => format!("sudo apk add {pkg}"),
    }
}

/// Argv for `Command::new(argv[0]).args(&argv[1..])`. Linux PMs prepend `sudo`.
pub fn install_argv(pm: PackageManager, pkg: &str) -> Vec<String> {
    let pkg = pkg.to_string();
    match pm {
        PackageManager::Brew => vec!["brew".into(), "install".into(), pkg],
        PackageManager::MacPorts => vec!["sudo".into(), "port".into(), "install".into(), pkg],
        PackageManager::Apt => {
            vec!["sudo".into(), "apt".into(), "install".into(), "-y".into(), pkg]
        }
        PackageManager::Dnf => {
            vec!["sudo".into(), "dnf".into(), "install".into(), "-y".into(), pkg]
        }
        PackageManager::Pacman => vec![
            "sudo".into(),
            "pacman".into(),
            "-S".into(),
            "--noconfirm".into(),
            pkg,
        ],
        PackageManager::Zypper => vec![
            "sudo".into(),
            "zypper".into(),
            "install".into(),
            "-y".into(),
            pkg,
        ],
        PackageManager::Apk => vec!["sudo".into(), "apk".into(), "add".into(), pkg],
    }
}

/// Shared ensure-or-install outcome. Every ensurer returns this shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    Ready(PathBuf),
    UserDeclined,
    /// Install command failed or the thing still isn't present after.
    InstallFailed,
    /// No package manager, or PM-specific branch routes the user manual.
    NoPackageManager,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[PackageManager] = &[
        PackageManager::Brew,
        PackageManager::MacPorts,
        PackageManager::Apt,
        PackageManager::Dnf,
        PackageManager::Pacman,
        PackageManager::Zypper,
        PackageManager::Apk,
    ];

    #[test]
    fn display_matches_argv_joined() {
        for &pm in ALL {
            let display = install_cmd_display(pm, "foo");
            let argv = install_argv(pm, "foo");
            assert_eq!(display, argv.join(" "), "drift for {pm:?}");
        }
    }

    #[test]
    fn linux_pms_use_sudo() {
        assert!(!install_cmd_display(PackageManager::Brew, "x").starts_with("sudo"));
        for pm in [
            PackageManager::Apt,
            PackageManager::Dnf,
            PackageManager::Pacman,
            PackageManager::Zypper,
            PackageManager::Apk,
        ] {
            assert!(
                install_cmd_display(pm, "x").starts_with("sudo "),
                "{pm:?} missing sudo prefix"
            );
        }
    }

    #[test]
    fn argv_contains_pkg_name() {
        for &pm in ALL {
            let argv = install_argv(pm, "my-pkg");
            assert!(argv.iter().any(|a| a == "my-pkg"), "argv for {pm:?}: {argv:?}");
        }
    }
}
