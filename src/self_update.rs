//! `daycare-runner update`: replace this binary with the site's current release.
//!
//! The server refuses any companion that is not the current release (HTTP 426),
//! and until now the fix was a person re-running the curl installer before
//! every visit. This does what the installer does, from inside the runner:
//! read the release pointer, download, check the pinned sha256, and rename the
//! new build over this one.
//!
//! Trust is the installer's: HTTPS plus a sha256 pinned by the same origin.
//! The pointer is `releases/current.json`:
//!
//! ```json
//! {"release": "<id>", "targets": {"aarch64-apple-darwin": {"url": "...", "sha256": "..."}}}
//! ```
//!
//! keyed by Rust target triple, so one release can carry a binary per platform
//! and each runner takes its own. A site that predates that file still serves
//! `install.sh`, whose three `RUNNER_*` lines pin the one Apple Silicon build,
//! so an `aarch64-apple-darwin` runner reads those instead.

use crate::{Error, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Set on the process a self-update re-executes, so a server and pointer that
/// disagree cannot loop update → retry → update.
pub const UPDATED_ENV: &str = "DAYCARE_RUNNER_SELF_UPDATED";

/// The Rust target triple this binary was built for (from `build.rs`).
pub const TARGET: &str = env!("DAYCARE_RUNNER_TARGET");

/// The only target the pre-`current.json` installer ever shipped.
const LEGACY_INSTALLER_TARGET: &str = "aarch64-apple-darwin";

/// A release binary is a few MB; anything this large is not one.
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

/// `releases/current.json` as the site serves it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseManifest {
    pub release: String,
    pub targets: BTreeMap<String, TargetBuild>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TargetBuild {
    pub url: String,
    pub sha256: String,
}

/// The current release as it applies to one target: where this machine's
/// binary is and what it must hash to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasePointer {
    pub release: String,
    pub url: String,
    pub sha256: String,
}

impl ReleaseManifest {
    /// This target's build, or an error naming what the release does carry.
    pub fn for_target(&self, target: &str) -> Result<ReleasePointer> {
        let build = self.targets.get(target).ok_or_else(|| {
            let carried: Vec<&str> = self.targets.keys().map(String::as_str).collect();
            Error::new(format!(
                "release {} has no build for {target} (it carries: {})",
                self.release,
                carried.join(", ")
            ))
        })?;
        Ok(ReleasePointer {
            release: self.release.clone(),
            url: build.url.clone(),
            sha256: build.sha256.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A `(dev)` build has no release to compare, so it is never replaced.
    DevBuild,
    /// Already the release the site points at.
    Current { release: String },
    /// Replaced. `skill_error` is set when the new binary's `skill install`
    /// failed; the binary update itself still stands.
    Updated {
        from: String,
        to: String,
        path: PathBuf,
        skill_error: Option<String>,
    },
}

/// Bring `exe` up to the site's current release. `mine` is the release baked
/// into the running binary (`None` on a dev build). `note` receives progress
/// lines for a person.
pub fn update(
    base_url: &str,
    mine: Option<&str>,
    exe: &Path,
    note: &mut dyn FnMut(&str),
) -> Result<Outcome> {
    let Some(mine) = mine else {
        return Ok(Outcome::DevBuild);
    };
    let base_url = base_url.trim_end_matches('/');
    if !https_or_loopback(base_url) {
        return Err(Error::new(format!(
            "will not update over plain HTTP from {base_url}; the platform URL must be HTTPS"
        )));
    }
    let agent = agent();
    let pointer = fetch_pointer(&agent, base_url, TARGET)?;
    if pointer.release == mine {
        return Ok(Outcome::Current {
            release: pointer.release,
        });
    }

    note(&format!(
        "Updating daycare-runner {mine} -> {}...",
        pointer.release
    ));
    let bytes = download(&agent, &pointer.url)?;
    let actual = sha256_hex(&bytes);
    if actual != pointer.sha256 {
        return Err(Error::new(format!(
            "checksum mismatch for release {}: the download does not match the published sha256. \
             Nothing was replaced.",
            pointer.release
        )));
    }

    let path = replace_exe(exe, &bytes, &pointer.release)?;
    let skill_error = install_skill(&path).err().map(|error| error.to_string());
    Ok(Outcome::Updated {
        from: mine.to_string(),
        to: pointer.release,
        path,
        skill_error,
    })
}

/// True for the platform's "this build is too old" refusal, however deep in a
/// command it surfaced. Some callers rewrap errors, which keeps the text but
/// drops the status.
pub fn is_update_required(error: &Error) -> bool {
    error.http_status() == Some(426) || error.message().contains("HTTP 426")
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("daycare-runner/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn fetch_pointer(agent: &ureq::Agent, base_url: &str, target: &str) -> Result<ReleasePointer> {
    let json_url = format!("{base_url}/releases/current.json");
    let pointer = match agent.get(&json_url).call() {
        Ok(response) => {
            let text = response.into_string()?;
            serde_json::from_str::<ReleaseManifest>(&text)
                .map_err(|error| {
                    Error::new(format!("{json_url} was not a release pointer: {error}"))
                })?
                .for_target(target)?
        }
        Err(ureq::Error::Status(404, _)) if target == LEGACY_INSTALLER_TARGET => {
            let script_url = format!("{base_url}/install.sh");
            let script = agent
                .get(&script_url)
                .call()
                .map_err(|error| {
                    Error::transport(format!("could not fetch {script_url}: {error}"))
                })?
                .into_string()?;
            pointer_from_install_script(&script).ok_or_else(|| {
                Error::new(format!(
                    "{script_url} does not pin a release (RUNNER_VERSION, RUNNER_URL, RUNNER_SHA256)"
                ))
            })?
        }
        Err(error) => {
            return Err(Error::transport(format!(
                "could not fetch {json_url}: {error}"
            )))
        }
    };
    validate_pointer(&pointer)?;
    Ok(pointer)
}

/// The installer pins its release as three shell assignments; read them the
/// way `sh` would for these simple double-quoted values.
pub fn pointer_from_install_script(script: &str) -> Option<ReleasePointer> {
    let value = |name: &str| {
        script.lines().find_map(|line| {
            let rest = line.trim().strip_prefix(name)?.strip_prefix('=')?;
            Some(rest.trim_matches('"').trim_matches('\'').to_string())
        })
    };
    Some(ReleasePointer {
        release: value("RUNNER_VERSION")?,
        url: value("RUNNER_URL")?,
        sha256: value("RUNNER_SHA256")?,
    })
}

/// The pointer decides what code runs next, so it is held to the installer's
/// standard: a plausible release id, a real sha256, and a download over HTTPS
/// (plain HTTP only to loopback).
fn validate_pointer(pointer: &ReleasePointer) -> Result<()> {
    let release_ok = !pointer.release.is_empty()
        && pointer.release.len() <= 64
        && pointer
            .release
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if !release_ok {
        return Err(Error::new(format!(
            "release pointer names an implausible release {:?}",
            pointer.release
        )));
    }
    let sha_ok = pointer.sha256.len() == 64
        && pointer
            .sha256
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if !sha_ok {
        return Err(Error::new(
            "release pointer's sha256 is not 64 lowercase hex digits",
        ));
    }
    if !https_or_loopback(&pointer.url) {
        return Err(Error::new(format!(
            "release pointer's download is not HTTPS: {}",
            pointer.url
        )));
    }
    Ok(())
}

/// HTTPS, or plain HTTP to this machine (tests and a local platform). Plain
/// HTTP anywhere else would let anyone on the path choose the code we run.
pub fn https_or_loopback(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..host_end];
    if authority.contains('@') {
        return false;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn download(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let response = agent
        .get(url)
        .call()
        .map_err(|error| Error::transport(format!("could not download {url}: {error}")))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BINARY_BYTES {
        return Err(Error::new(format!(
            "{url} is larger than any runner release"
        )));
    }
    Ok(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Write the new build beside `exe`, prove it runs and is the release the
/// pointer promised, then rename it over `exe`. The rename is atomic within a
/// directory, and it gives the file a new inode — which is what macOS wants
/// for a signed binary: rewriting one in place poisons its cached signature.
/// A running process keeps its old inode, so replacing a live binary is safe.
fn replace_exe(exe: &Path, bytes: &[u8], release: &str) -> Result<PathBuf> {
    // Replace the file, not a symlink to it.
    let target = fs::canonicalize(exe)
        .map_err(|error| Error::new(format!("could not resolve {}: {error}", exe.display())))?;
    let dir = target
        .parent()
        .ok_or_else(|| Error::new(format!("{} has no directory", target.display())))?;
    let temp = dir.join(format!(".daycare-runner.update-{}", std::process::id()));
    let staged = stage(&temp, bytes).and_then(|()| check_version(&temp, release));
    if let Err(error) = staged {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    fs::rename(&temp, &target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        Error::new(format!("could not replace {}: {error}", target.display()))
    })?;
    Ok(target)
}

fn stage(temp: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::File::create(temp).map_err(|error| {
        Error::new(format!(
            "could not write beside the runner ({}): {error}",
            temp.display()
        ))
    })?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    make_executable(temp)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// A download that matches its sha can still be the wrong architecture or a
/// publishing mistake. Running `--version` before the rename catches both
/// while the old binary is still in place.
fn check_version(path: &Path, release: &str) -> Result<()> {
    // ETXTBSY: a fork elsewhere in this process can briefly hold the just-
    // written file open until its exec. Waiting a moment clears it.
    let mut attempt = 0;
    let output = loop {
        match Command::new(path)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
        {
            Err(error) if error.raw_os_error() == Some(26) && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            result => {
                break result.map_err(|error| {
                    Error::new(format!("the downloaded runner would not start: {error}"))
                })?
            }
        }
    };
    let version = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !version.contains(&format!("(release {release})")) {
        return Err(Error::new(format!(
            "the downloaded runner does not report release {release} (it said {:?}); nothing was replaced",
            version.trim()
        )));
    }
    Ok(())
}

/// The skill ships inside the binary, so the new binary is the one to write it.
fn install_skill(exe: &Path) -> Result<()> {
    let output = Command::new(exe)
        .args(["skill", "install", "--json"])
        .stdin(Stdio::null())
        .env(UPDATED_ENV, "1")
        .output()
        .map_err(|error| Error::new(format!("could not run skill install: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(Error::new(format!(
        "`daycare-runner skill install` failed: {}",
        stdout.trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "47eacfc4b104bd246fd4b812161664e99c2bdc9a36841d52a3c1167c11216623";

    #[test]
    fn the_installer_pins_are_read_as_a_pointer() {
        let script = format!(
            "#!/bin/sh\nset -eu\n\nRUNNER_VERSION=\"b5433d3bf\"\n\
             RUNNER_URL=\"https://claudedaycare.com/releases/daycare-runner-b5433d3bf-47eacfc4\"\n\
             RUNNER_SHA256=\"{SHA}\"\n\nsay() {{ :; }}\n"
        );
        assert_eq!(
            pointer_from_install_script(&script),
            Some(ReleasePointer {
                release: "b5433d3bf".into(),
                url: "https://claudedaycare.com/releases/daycare-runner-b5433d3bf-47eacfc4".into(),
                sha256: SHA.into(),
            })
        );
        assert_eq!(pointer_from_install_script("RUNNER_VERSION=\"x\"\n"), None);
    }

    #[test]
    fn a_pointer_must_name_a_plausible_release_a_real_sha_and_https() {
        let good = ReleasePointer {
            release: "b5433d3bf".into(),
            url: "https://claudedaycare.com/releases/x".into(),
            sha256: SHA.into(),
        };
        assert!(validate_pointer(&good).is_ok());

        let http_elsewhere = ReleasePointer {
            url: "http://evil.test/x".into(),
            ..good.clone()
        };
        assert!(validate_pointer(&http_elsewhere).is_err());
        let http_same_origin = ReleasePointer {
            url: "http://127.0.0.1:9/releases/x".into(),
            ..good.clone()
        };
        assert!(validate_pointer(&http_same_origin).is_ok());

        let short_sha = ReleasePointer {
            sha256: "abc".into(),
            ..good.clone()
        };
        assert!(validate_pointer(&short_sha).is_err());
        let odd_release = ReleasePointer {
            release: "../../x y".into(),
            ..good
        };
        assert!(validate_pointer(&odd_release).is_err());
    }

    #[test]
    fn plain_http_is_accepted_only_to_loopback() {
        for ok in [
            "https://claudedaycare.com/x",
            "http://127.0.0.1:9/x",
            "http://localhost/x",
            "http://[::1]:9/x",
            "http://127.0.0.1",
        ] {
            assert!(https_or_loopback(ok), "{ok}");
        }
        for bad in [
            "http://claudedaycare.com/x",
            "http://127.0.0.1.evil.test/x",
            "http://localhost.evil.test/x",
            "http://127.0.0.1@evil.test/x",
            "http://10.0.0.5/x",
            "ftp://127.0.0.1/x",
        ] {
            assert!(!https_or_loopback(bad), "{bad}");
        }
    }

    #[test]
    fn an_update_from_a_plain_http_platform_elsewhere_is_refused_before_any_request() {
        let error = update(
            "http://daycare.example.test",
            Some("old0"),
            Path::new("/nonexistent"),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(error.message().contains("plain HTTP"), "{error}");
    }

    #[test]
    fn a_dev_build_is_never_replaced_and_touches_nothing() {
        let mut notes = Vec::new();
        let outcome = update(
            "http://127.0.0.1:1",
            None,
            Path::new("/nonexistent"),
            &mut |line| notes.push(line.to_string()),
        )
        .unwrap();
        assert_eq!(outcome, Outcome::DevBuild);
        assert!(notes.is_empty());
    }

    #[test]
    fn update_required_is_recognised_with_or_without_the_status() {
        assert!(is_update_required(&Error::new("x").with_status(426)));
        assert!(is_update_required(&Error::new(
            "visit start failed: runner_update_required (HTTP 426)"
        )));
        assert!(!is_update_required(&Error::new("x").with_status(409)));
    }
}
