//! Windows Subsystem for Linux: detection, and the Windows side of the seal.
//!
//! The Linux build runs unchanged inside WSL. Two things differ from a plain
//! Linux machine, and both are about the Windows host rather than the distro:
//!
//! - Claude Code inside WSL can inherit Windows enterprise policy
//!   (`wslInheritsWindowsSettings`, set in `C:\Program Files\ClaudeCode` or the
//!   `SOFTWARE\Policies\ClaudeCode` registry key). Daycare refuses any active
//!   enterprise policy source, so under WSL it also checks the Windows ones,
//!   through the `/mnt/c` mount and `reg.exe` interop. It does not try to work
//!   out whether a policy is inherited: any Windows-side Claude policy refuses
//!   the turn, and so does being unable to look.
//! - The distro cannot hold the Windows host awake, so `visit start` says so.

use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// What `status` reports about a WSL host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WslInfo {
    /// 1 or 2, from the kernel release string.
    pub version: u8,
    /// `WSL_DISTRO_NAME`, when the environment still carries it.
    pub distro: Option<String>,
    /// Whether Windows executables can be launched from the distro. The policy
    /// guard needs it; without it every Daycare turn is refused.
    pub interop: bool,
}

impl WslInfo {
    pub fn describe(&self) -> String {
        let distro = self
            .distro
            .as_deref()
            .map(|name| format!(", distro {name}"))
            .unwrap_or_default();
        let interop = if self.interop {
            ""
        } else {
            ", Windows interop OFF (visits are refused until it is on)"
        };
        format!("Linux on WSL{}{distro}{interop}", self.version)
    }
}

/// Printed by `visit start` under WSL, in place of the keep-awake promise.
pub const HOST_SLEEP_MESSAGE: &str = "This is WSL: the runner cannot keep the Windows host awake. \
     If Windows sleeps, the visit stops until it wakes. Plug it in and set Windows power \
     settings so it does not sleep while a visit runs.";

/// Any WSL signal requires Windows policy checks. A Microsoft kernel with
/// scrubbed environment and missing interop is ambiguous, not proof of a plain
/// container: do not skip policy checks just because the sources are absent.
/// No container exemption is made without positive proof of isolation.
pub fn detect() -> Option<WslInfo> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let osrelease = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
    let interop_entry = ["WSLInterop", "WSLInterop-late"]
        .iter()
        .any(|name| Path::new("/proc/sys/fs/binfmt_misc").join(name).exists());
    detect_from(
        osrelease.as_deref(),
        std::env::var("WSL_DISTRO_NAME").ok(),
        interop_entry,
    )
}

fn detect_from(
    osrelease: Option<&str>,
    distro: Option<String>,
    interop_entry: bool,
) -> Option<WslInfo> {
    let release = osrelease.unwrap_or("").to_ascii_lowercase();
    let kernel_says_wsl = release.contains("microsoft") || release.contains("wsl");
    let distro = distro.filter(|name| !name.trim().is_empty());
    if !kernel_says_wsl && distro.is_none() && !interop_entry {
        return None;
    }
    // WSL2 kernels are "…-microsoft-standard-WSL2"; WSL1 reports a fake
    // "…-Microsoft" release with no "standard".
    let version = if release.contains("wsl2") || release.contains("microsoft-standard") {
        2
    } else if release.contains("microsoft") {
        1
    } else {
        2
    };
    Some(WslInfo {
        version,
        distro,
        interop: interop_entry,
    })
}

/// How long one `reg.exe` or `wslpath` call may take. Interop's first launch
/// after the distro starts can take a few seconds.
const INTEROP_TIMEOUT: Duration = Duration::from_secs(20);

/// Where the Windows-side policy lives, as seen from inside the distro.
#[derive(Debug, Clone)]
pub struct WindowsPolicySources {
    /// `C:\Program Files`, mounted (normally `/mnt/c/Program Files`).
    pub program_files: PathBuf,
    /// `C:\Windows\System32\reg.exe`, mounted.
    pub reg_exe: PathBuf,
    /// Working directory for `reg.exe`: a Windows path, so interop does not
    /// warn about a UNC cwd.
    pub windows_cwd: PathBuf,
}

impl WindowsPolicySources {
    pub fn discover() -> Result<Self> {
        let drive = system_drive_root()?;
        Ok(WindowsPolicySources {
            program_files: drive.join("Program Files"),
            reg_exe: drive.join("Windows").join("System32").join("reg.exe"),
            windows_cwd: drive,
        })
    }

    /// The directory Claude Code reads managed settings and managed
    /// CLAUDE.md from on Windows.
    pub fn claude_policy_dir(&self) -> PathBuf {
        self.program_files.join("ClaudeCode")
    }
}

/// The mount point of `C:`. `wslpath` knows the automount root even when
/// `/etc/wsl.conf` moves it; `/mnt/c` is the default when `wslpath` is absent.
fn system_drive_root() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if Path::new("/usr/bin/wslpath").exists() {
        let mut command = Command::new("/usr/bin/wslpath");
        command.args(["-u", r"C:\"]);
        if let Ok(output) = crate::keychain::run_helper_with_deadline(
            command,
            None,
            INTEROP_TIMEOUT,
            "wslpath",
            "WSL did not answer",
        ) {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if output.status.success() && path.starts_with('/') {
                candidates.push(PathBuf::from(path));
            }
        }
    }
    candidates.push(PathBuf::from("/mnt/c"));
    candidates
        .into_iter()
        .find(|root| root.join("Windows").is_dir())
        .ok_or_else(|| {
            Error::new(
                "refusing to run a Daycare turn: this is WSL, and the Windows drive C: is not \
                 mounted, so Windows-side Claude enterprise policy cannot be checked. Enable \
                 automount in /etc/wsl.conf and restart WSL",
            )
        })
}

/// Refuse the turn if Windows has any Claude Code enterprise policy, or if it
/// cannot be read. `guard_policy_dir` is the same file check Daycare runs on
/// the native policy directory.
pub fn guard_windows_claude_policy(
    sources: &WindowsPolicySources,
    guard_policy_dir: impl Fn(&Path) -> Result<()>,
) -> Result<()> {
    match std::fs::metadata(&sources.program_files) {
        Ok(metadata) if metadata.is_dir() => {}
        _ => {
            return Err(Error::new(format!(
                "refusing to run a Daycare turn: this is WSL and {} cannot be read, so \
                 Windows-side Claude enterprise policy cannot be checked",
                sources.program_files.display()
            )))
        }
    }
    guard_policy_dir(&sources.claude_policy_dir())?;
    for hive in ["HKLM", "HKCU"] {
        if registry_has_claude_policy(sources, hive)? {
            return Err(Error::new(format!(
                "refusing to run a Daycare turn because Windows has Claude enterprise policy in \
                 {hive}\\SOFTWARE\\Policies\\ClaudeCode, which Claude Code in WSL can inherit. \
                 Managed instructions cannot be excluded from the child; use an unmanaged \
                 machine for Daycare"
            )));
        }
    }
    Ok(())
}

/// Lists `SOFTWARE\Policies` rather than querying `…\ClaudeCode` directly:
/// `reg.exe` exits 1 both for a missing key and for real failures, with a
/// localized message, while listing a key that always exists succeeds or
/// fails unambiguously.
fn registry_has_claude_policy(sources: &WindowsPolicySources, hive: &str) -> Result<bool> {
    let key = format!(r"{hive}\SOFTWARE\Policies");
    let mut command = Command::new(&sources.reg_exe);
    command
        .args(["query", &key])
        .current_dir(&sources.windows_cwd);
    let unreadable = |detail: String| {
        Error::new(format!(
            "refusing to run a Daycare turn: this is WSL and the Windows registry key {key} \
             could not be read ({detail}), so Windows-side Claude enterprise policy cannot be \
             checked. Windows interop must be on (it is by default)"
        ))
    };
    let output = crate::keychain::run_helper_with_deadline(
        command,
        None,
        INTEROP_TIMEOUT,
        "reg.exe",
        "Windows interop did not answer",
    )
    .map_err(|error| unreadable(error.to_string()))?;
    if !output.status.success() {
        return Err(unreadable(format!(
            "reg.exe exit {}",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into())
        )));
    }
    // reg.exe writes the console code page, or UTF-16 on some hosts; dropping
    // NULs reads ASCII key names correctly from either.
    let text: String = String::from_utf8_lossy(&output.stdout)
        .chars()
        .filter(|c| *c != '\0')
        .collect();
    Ok(text.lines().any(|line| {
        line.trim()
            .to_ascii_lowercase()
            .ends_with(r"\policies\claudecode")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl2_wsl1_and_plain_linux_are_told_apart() {
        let wsl2 = detect_from(
            Some("5.15.167.4-microsoft-standard-WSL2\n"),
            Some("Ubuntu".into()),
            true,
        )
        .unwrap();
        assert_eq!(wsl2.version, 2);
        assert_eq!(wsl2.distro.as_deref(), Some("Ubuntu"));
        assert!(wsl2.interop);
        assert_eq!(wsl2.describe(), "Linux on WSL2, distro Ubuntu");

        let wsl1 = detect_from(Some("4.4.0-19041-Microsoft"), None, true).unwrap();
        assert_eq!(wsl1.version, 1);

        assert_eq!(detect_from(Some("6.8.0-138-generic"), None, false), None);
        assert_eq!(detect_from(None, Some("  ".into()), false), None);
    }

    #[test]
    fn wsl_is_still_detected_when_the_environment_was_scrubbed() {
        let info = detect_from(Some("5.15.0-microsoft-standard-WSL2"), None, true).unwrap();
        assert_eq!(info.distro, None);
        assert!(info.interop);
        let no_interop = detect_from(
            Some("5.15.0-microsoft-standard-WSL2"),
            Some("Ubuntu".into()),
            false,
        )
        .unwrap();
        assert!(no_interop.describe().contains("interop OFF"));
    }

    #[test]
    fn a_microsoft_kernel_without_distro_or_interop_markers_still_requires_policy_checks() {
        for (release, version) in [
            ("5.15.0-microsoft-standard-WSL2", 2),
            ("4.4.0-19041-Microsoft", 1),
        ] {
            for distro in [None, Some("  ".into())] {
                let info = detect_from(Some(release), distro, false)
                    .expect("missing markers cannot prove a plain container");
                assert_eq!(info.version, version);
                assert_eq!(info.distro, None);
                assert!(!info.interop);
                assert!(info.describe().contains("visits are refused"));
            }
        }
    }

    #[test]
    fn positive_wsl_markers_require_policy_checks_even_if_kernel_release_is_unreadable() {
        assert!(detect_from(None, Some("Ubuntu".into()), false).is_some());
        assert!(detect_from(None, None, true).is_some());
        assert_eq!(detect_from(None, None, false), None);
    }

    /// A fake Windows drive: `Program Files`, and a `reg.exe` that prints the
    /// given key listing (or fails).
    fn fake_drive(reg_output: &str, reg_exit: i32) -> WindowsPolicySources {
        let root = crate::testdir::unique_dir("daycare-wsl-drive");
        std::fs::create_dir_all(root.join("Program Files")).unwrap();
        let system32 = root.join("Windows").join("System32");
        std::fs::create_dir_all(&system32).unwrap();
        let reg = system32.join("reg.exe");
        crate::testdir::write_executable(
            &reg,
            &format!("#!/bin/sh\nprintf '%s' '{reg_output}'\nexit {reg_exit}\n"),
        );
        WindowsPolicySources {
            program_files: root.join("Program Files"),
            reg_exe: reg,
            windows_cwd: root,
        }
    }

    const NO_CLAUDE_POLICY: &str = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Policies\\Microsoft\r\n";

    #[test]
    fn a_windows_host_without_claude_policy_passes() {
        let sources = fake_drive(NO_CLAUDE_POLICY, 0);
        guard_windows_claude_policy(&sources, |_| Ok(())).unwrap();
    }

    #[test]
    fn registry_claude_policy_is_refused() {
        let listing = "\r\nHKEY_CURRENT_USER\\SOFTWARE\\Policies\\Microsoft\r\n\
                       HKEY_CURRENT_USER\\SOFTWARE\\Policies\\ClaudeCode\r\n";
        let sources = fake_drive(listing, 0);
        let error = guard_windows_claude_policy(&sources, |_| Ok(())).unwrap_err();
        assert!(
            error.to_string().contains("Policies\\ClaudeCode"),
            "{error}"
        );
    }

    #[test]
    fn an_unreadable_registry_is_refused_not_ignored() {
        let sources = fake_drive("", 1);
        let error = guard_windows_claude_policy(&sources, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("could not be read"), "{error}");

        let mut missing = fake_drive(NO_CLAUDE_POLICY, 0);
        missing.reg_exe = missing.windows_cwd.join("no-such-reg.exe");
        let error = guard_windows_claude_policy(&missing, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("could not be read"), "{error}");
    }

    #[test]
    fn an_unmounted_program_files_is_refused() {
        let mut sources = fake_drive(NO_CLAUDE_POLICY, 0);
        sources.program_files = sources.windows_cwd.join("not-mounted");
        let error = guard_windows_claude_policy(&sources, |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("cannot be read"), "{error}");
    }

    #[test]
    fn the_program_files_policy_dir_goes_through_the_native_file_check() {
        let sources = fake_drive(NO_CLAUDE_POLICY, 0);
        let expected = sources.claude_policy_dir();
        let error = guard_windows_claude_policy(&sources, |dir| {
            assert_eq!(dir, expected);
            Err(Error::new("policy file found"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "policy file found");
    }
}
