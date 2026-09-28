//! Getting the owner's ChatGPT login into the sealed `CODEX_HOME`, and a
//! refreshed login back out.
//!
//! A sealed `CODEX_HOME` is the only way to keep the owner's global AGENTS.md
//! out of a visit, and an empty one is "Not logged in". On macOS the keychain
//! store does not help (`cli_auth_credentials_store = keyring|auto` in a
//! sealed home is still "Not logged in"; Mac check, codex 0.155), so the login
//! is copied: `~/.codex/auth.json` into the sealed home at 0600 before a turn.
//!
//! ChatGPT refresh tokens rotate. If Codex refreshes inside the sealed home,
//! the owner's own copy now holds a spent refresh token, and their next
//! ordinary `codex` run would be logged out. So after every turn the newer
//! login is written back, under a lock, atomically, at 0600.
//!
//! `CodexLogin` is the interface; `CopiedLogin` is the one implementation.
//! A keychain-backed login would be a second one if Codex ever supports it.

use crate::paths::{create_private_dir, write_atomic};
use crate::{Error, Result};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub const AUTH_FILE: &str = "auth.json";

pub trait CodexLogin: Send + Sync {
    /// Make the sealed home logged in as the owner, before any Codex child.
    fn bring_in(&self, sealed_home: &Path) -> Result<()>;
    /// After a Codex child exits, hand a refreshed login back to the owner.
    fn carry_back(&self, sealed_home: &Path) -> Result<()>;
}

/// The owner's `auth.json`, copied in and reconciled back.
pub struct CopiedLogin {
    /// The owner's own Codex home (`$CODEX_HOME`, else `~/.codex`).
    owner_home: PathBuf,
}

impl CopiedLogin {
    pub fn new(owner_home: PathBuf) -> Self {
        CopiedLogin { owner_home }
    }

    /// The owner's Codex home as their own `codex` would find it.
    pub fn discover() -> Result<Self> {
        if let Some(home) = std::env::var_os("CODEX_HOME").filter(|home| !home.is_empty()) {
            return Ok(CopiedLogin::new(PathBuf::from(home)));
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| Error::new("HOME is not set; cannot find the Codex login"))?;
        Ok(CopiedLogin::new(PathBuf::from(home).join(".codex")))
    }

    fn owner_auth(&self) -> PathBuf {
        self.owner_home.join(AUTH_FILE)
    }
}

impl CodexLogin for CopiedLogin {
    fn bring_in(&self, sealed_home: &Path) -> Result<()> {
        reconcile(&self.owner_auth(), sealed_home).map(|_| ())
    }

    fn carry_back(&self, sealed_home: &Path) -> Result<()> {
        reconcile(&self.owner_auth(), sealed_home).map(|_| ())
    }
}

/// What `reconcile` decided. Returned for tests; the caller only needs Ok.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synced {
    Same,
    CopiedIn,
    CopiedBack,
}

/// Bring the sealed copy and the owner's copy into agreement, under a lock
/// every runner shares. The owner's own `codex` does not take this lock; it
/// only ever sees the owner's file replaced in one rename, and only by a login
/// for the same account with a newer `last_refresh`.
pub fn reconcile(owner_auth: &Path, sealed_home: &Path) -> Result<Synced> {
    create_private_dir(sealed_home)?;
    let _lock = lock(&sealed_home.join("auth.lock"))?;
    let sealed_auth = sealed_home.join(AUTH_FILE);

    let owner_text = match std::fs::read_to_string(owner_auth) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::new(format!(
                "Codex is not logged in ({} is missing). Run `codex login` and sign in with ChatGPT",
                owner_auth.display()
            )))
        }
        Err(error) => return Err(error.into()),
    };
    let owner = Login::parse(&owner_text, owner_auth)?;

    let sealed_text = match std::fs::read_to_string(&sealed_auth) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let Some(sealed_text) = sealed_text else {
        copy_in(&owner_text, &sealed_auth)?;
        return Ok(Synced::CopiedIn);
    };
    if sealed_text == owner_text {
        return Ok(Synced::Same);
    }
    // A sealed copy that no longer parses is the runner's own mess; the
    // owner's login is the truth.
    let Ok(sealed) = Login::parse(&sealed_text, &sealed_auth) else {
        copy_in(&owner_text, &sealed_auth)?;
        return Ok(Synced::CopiedIn);
    };
    // The newer refresh holds the live refresh token, unless the owner has
    // since logged in as someone else: then follow them.
    let sealed_wins =
        sealed.account_id == owner.account_id && sealed.last_refresh > owner.last_refresh;
    if !sealed_wins {
        copy_in(&owner_text, &sealed_auth)?;
        return Ok(Synced::CopiedIn);
    }
    // Replace the owner's file in one step at 0600. Follow a symlinked
    // auth.json to the file it names rather than replacing the link.
    let target = owner_auth
        .canonicalize()
        .unwrap_or_else(|_| owner_auth.to_path_buf());
    write_atomic(&target, sealed_text.as_bytes(), 0o600)?;
    Ok(Synced::CopiedBack)
}

fn copy_in(owner_text: &str, sealed_auth: &Path) -> Result<()> {
    write_atomic(sealed_auth, owner_text.as_bytes(), 0o600)
}

/// The parts of `auth.json` the reconcile reads. Tokens are never read into
/// anything but the byte copy.
struct Login {
    account_id: Option<String>,
    last_refresh: String,
}

impl Login {
    fn parse(text: &str, path: &Path) -> Result<Login> {
        let value: Value = serde_json::from_str(text).map_err(|_| {
            Error::new(format!(
                "{} is not a Codex login file; run `codex login` again",
                path.display()
            ))
        })?;
        // An API key would bill every visit to the owner's API account instead
        // of their ChatGPT plan. Daycare runs on subscriptions only.
        let has_api_key = value
            .get("OPENAI_API_KEY")
            .is_some_and(|key| !key.is_null());
        let mode = value.get("auth_mode").and_then(Value::as_str);
        if has_api_key || mode.is_some_and(|mode| mode != "chatgpt") {
            return Err(Error::new(
                "Codex is logged in with an API key; Daycare runs on a ChatGPT plan only. \
                 Run `codex logout` and `codex login` with ChatGPT",
            ));
        }
        let tokens = value.get("tokens").filter(|tokens| tokens.is_object());
        let Some(tokens) = tokens else {
            return Err(Error::new(format!(
                "{} holds no ChatGPT login; run `codex login`",
                path.display()
            )));
        };
        Ok(Login {
            account_id: tokens
                .get("account_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            // RFC 3339 in UTC with a fixed layout, so text order is time order.
            last_refresh: value
                .get("last_refresh")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }
}

/// An exclusive advisory lock, released when the file closes.
struct AuthLock(#[allow(dead_code)] File);

fn lock(path: &Path) -> Result<AuthLock> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    flock_exclusive(&file)?;
    Ok(AuthLock(file))
}

#[cfg(unix)]
fn flock_exclusive(file: &File) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock on a descriptor this function borrows for the call.
    let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if status != 0 {
        return Err(Error::new(format!(
            "could not lock the Codex login: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn flock_exclusive(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(account: &str, refresh: &str, token: &str) -> String {
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "auth_mode": "chatgpt",
            "last_refresh": refresh,
            "tokens": {
                "access_token": format!("access-{token}"),
                "refresh_token": format!("refresh-{token}"),
                "id_token": "id",
                "account_id": account,
            }
        })
        .to_string()
    }

    fn setup() -> (PathBuf, PathBuf, PathBuf) {
        let root = crate::testdir::unique_dir("daycare-codex-login");
        let owner = root.join("owner");
        std::fs::create_dir_all(&owner).unwrap();
        (root.clone(), owner.join(AUTH_FILE), root.join("sealed"))
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_first_turn_copies_the_login_in_owner_only() {
        let (_root, owner, sealed) = setup();
        let text = auth("acct", "2026-09-25T17:37:37.1Z", "a");
        std::fs::write(&owner, &text).unwrap();
        assert_eq!(reconcile(&owner, &sealed).unwrap(), Synced::CopiedIn);
        assert_eq!(
            std::fs::read_to_string(sealed.join(AUTH_FILE)).unwrap(),
            text
        );
        #[cfg(unix)]
        assert_eq!(mode(&sealed.join(AUTH_FILE)), 0o600);
        assert_eq!(reconcile(&owner, &sealed).unwrap(), Synced::Same);
    }

    #[test]
    fn a_refresh_inside_the_seal_goes_back_to_the_owner() {
        let (_root, owner, sealed) = setup();
        std::fs::write(&owner, auth("acct", "2026-09-25T17:37:37.1Z", "a")).unwrap();
        reconcile(&owner, &sealed).unwrap();
        // Codex refreshed during the turn: new tokens, newer last_refresh.
        let refreshed = auth("acct", "2026-09-28T10:00:00.1Z", "b");
        std::fs::write(sealed.join(AUTH_FILE), &refreshed).unwrap();
        assert_eq!(reconcile(&owner, &sealed).unwrap(), Synced::CopiedBack);
        assert_eq!(std::fs::read_to_string(&owner).unwrap(), refreshed);
        #[cfg(unix)]
        assert_eq!(mode(&owner), 0o600);
        assert_eq!(reconcile(&owner, &sealed).unwrap(), Synced::Same);
    }

    #[test]
    fn a_refresh_by_the_owner_comes_in() {
        let (_root, owner, sealed) = setup();
        std::fs::write(&owner, auth("acct", "2026-09-25T17:37:37.1Z", "a")).unwrap();
        reconcile(&owner, &sealed).unwrap();
        let theirs = auth("acct", "2026-09-28T10:00:00.1Z", "c");
        std::fs::write(&owner, &theirs).unwrap();
        assert_eq!(reconcile(&owner, &sealed).unwrap(), Synced::CopiedIn);
        assert_eq!(
            std::fs::read_to_string(sealed.join(AUTH_FILE)).unwrap(),
            theirs
        );
    }

    #[test]
    fn when_both_refreshed_the_newer_one_wins() {
        let (_root, owner, sealed) = setup();
        std::fs::write(&owner, auth("acct", "2026-09-25T17:37:37.1Z", "a")).unwrap();
        reconcile(&owner, &sealed).unwrap();
        let ours = auth("acct", "2026-09-28T11:00:00.1Z", "b");
        let theirs = auth("acct", "2026-09-28T10:00:00.1Z", "c");
        std::fs::write(sealed.join(AUTH_FILE), &ours).unwrap();
        std::fs::write(&owner, &theirs).unwrap();
        assert_eq!(reconcile(&owner, &sealed).unwrap(), Synced::CopiedBack);
        assert_eq!(std::fs::read_to_string(&owner).unwrap(), ours);
    }

    #[test]
    fn a_different_account_on_the_owner_side_always_wins() {
        let (_root, owner, sealed) = setup();
        std::fs::write(&owner, auth("acct", "2026-09-25T17:37:37.1Z", "a")).unwrap();
        reconcile(&owner, &sealed).unwrap();
        std::fs::write(
            sealed.join(AUTH_FILE),
            auth("acct", "2026-09-30T00:00:00.1Z", "b"),
        )
        .unwrap();
        let switched = auth("other", "2026-09-26T00:00:00.1Z", "d");
        std::fs::write(&owner, &switched).unwrap();
        assert_eq!(reconcile(&owner, &sealed).unwrap(), Synced::CopiedIn);
        assert_eq!(std::fs::read_to_string(&owner).unwrap(), switched);
    }

    #[test]
    fn no_login_and_api_key_logins_are_refused() {
        let (_root, owner, sealed) = setup();
        let error = reconcile(&owner, &sealed).unwrap_err().to_string();
        assert!(error.contains("not logged in"), "{error}");
        std::fs::write(
            &owner,
            r#"{"OPENAI_API_KEY":"sk-test","auth_mode":"apikey","tokens":null}"#,
        )
        .unwrap();
        let error = reconcile(&owner, &sealed).unwrap_err().to_string();
        assert!(error.contains("API key"), "{error}");
        assert!(!error.contains("sk-test"), "never echo the key: {error}");
        assert!(!sealed.join(AUTH_FILE).exists());
    }
}
