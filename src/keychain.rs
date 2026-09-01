use crate::{Error, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SERVICE: &str = "claude-daycare";

/// How long any `security(1)` call may run before it is killed. On macOS 26.3,
/// `add-generic-password` blocks forever inside a keychain-unlock UI prompt
/// when the session has no usable login keychain (seen 2026-08-28 during the
/// friend-install cold gate). A hung child must become an error the caller can
/// fall back from, never a hang the user has to Ctrl-C.
const SECURITY_TIMEOUT: Duration = Duration::from_secs(10);

/// Run a child process with a hard deadline, feeding it `stdin_data` if given.
/// On expiry the child is killed and the call fails with "timed out".
fn run_with_deadline(
    mut command: Command,
    stdin_data: Option<&[u8]>,
    deadline: Duration,
) -> Result<std::process::Output> {
    command
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| Error::new(format!("could not run keychain helper: {error}")))?;
    if let Some(data) = stdin_data {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::new("child stdin unavailable"))?;
        stdin.write_all(data)?;
        // Dropping the handle closes the pipe so the child sees EOF.
    }
    let expires = Instant::now() + deadline;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if Instant::now() >= expires {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new(format!(
                "keychain call timed out after {}s — macOS is likely showing a \
                 keychain prompt this process cannot answer",
                deadline.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_security(args: &[&str], stdin_data: Option<&[u8]>) -> Result<std::process::Output> {
    let mut command = Command::new("/usr/bin/security");
    command.args(args);
    run_with_deadline(command, stdin_data, SECURITY_TIMEOUT)
}

/// Whether the keychain holds any item for `service`. Attribute lookup only —
/// never the secret, so it cannot trigger an authorization prompt. Used by the
/// enroll preflight to confirm a Claude Code login exists.
pub fn keychain_has_service(service: &str) -> bool {
    matches!(
        run_security(&["find-generic-password", "-s", service], None),
        Ok(output) if output.status.success()
    )
}

/// The selected acting credential is read once per turn and handed to the child
/// Claude process through an environment variable. It is never written to config,
/// argv, the turn archive, or an error message, so every path that touches it
/// goes through this trait — and tests get an in-memory one instead of the real
/// keychain.
pub trait TokenStore {
    fn store(&self, account: &str, token: &str) -> Result<()>;
    fn read(&self, account: &str) -> Result<Option<String>>;
    fn delete(&self, account: &str) -> Result<()>;
    /// Where the token lives, for `status` and `enroll` output. Never the token.
    fn location(&self) -> String;
}

/// macOS `security(1)` generic passwords. Passing the token as `-w <value>`
/// would expose it in `ps` output, so this uses the bare `-w` form: with no
/// inline value `security` prompts "password data for new item:" then "retype
/// password for new item:", and reads both from stdin. Verified against
/// macOS 25.3 on 2026-08-05 — one line each, or it stores an empty secret.
pub struct MacKeychain;

impl TokenStore for MacKeychain {
    fn store(&self, account: &str, token: &str) -> Result<()> {
        // Answer both prompts (enter + retype); the token never reaches argv.
        let mut stdin_data = Vec::with_capacity(token.len() * 2 + 2);
        for _ in 0..2 {
            stdin_data.extend_from_slice(token.as_bytes());
            stdin_data.push(b'\n');
        }
        let output = run_security(
            &[
                "add-generic-password",
                "-U",
                "-s",
                SERVICE,
                "-a",
                account,
                "-w",
            ],
            Some(&stdin_data),
        )?;
        if !output.status.success() {
            return Err(Error::new(format!(
                "keychain write failed (security exit {})",
                exit_code(&output.status)
            )));
        }
        Ok(())
    }

    fn read(&self, account: &str) -> Result<Option<String>> {
        let output = run_security(
            &["find-generic-password", "-s", SERVICE, "-a", account, "-w"],
            None,
        )?;
        if !output.status.success() {
            return Ok(None);
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Ok(None);
        }
        Ok(Some(token))
    }

    fn location(&self) -> String {
        format!("macOS keychain (service {SERVICE})")
    }

    fn delete(&self, account: &str) -> Result<()> {
        // Best-effort: an absent item is not an error, and neither is a
        // timed-out call — the resilient wrapper deletes the file copy too.
        let _ = run_security(
            &["delete-generic-password", "-s", SERVICE, "-a", account],
            None,
        );
        Ok(())
    }
}

fn exit_code(status: &std::process::ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string())
}

/// Env var naming a 0600 JSON file to hold device tokens instead of the
/// keychain. The test harness sets it so no test can write to the real
/// keychain; it is also the fallback on machines without `security(1)`.
pub const TOKEN_FILE_ENV: &str = "DAYCARE_TOKEN_FILE";

/// The store this machine should use. `fallback_file` is where the resilient
/// wrapper parks a token the keychain refused (normally
/// `~/.claude-daycare/tokens.json`, inside the 0700 config root).
///
/// Selecting the file store via `DAYCARE_TOKEN_FILE` is a **downgrade**: it
/// takes the device token out of the keychain and leaves it at rest on disk.
/// That is a weakening of hard rule 6, so it is never silent — every process
/// that resolves a store this way says so on stderr, and `status` prints the
/// active store.
pub fn default_store(fallback_file: PathBuf) -> Box<dyn TokenStore> {
    match std::env::var_os(TOKEN_FILE_ENV) {
        Some(path) => {
            let store = FileTokenStore::new(PathBuf::from(path));
            warn_token_file_downgrade(&store.path);
            Box::new(store)
        }
        None => Box::new(ResilientStore::new(
            MacKeychain,
            FileTokenStore::new(fallback_file),
        )),
    }
}

/// Keychain-first store that can never lose a token to a keychain failure.
///
/// By the time `store` runs during enroll, the server has already burned the
/// one-time pairing code — so "the keychain hung or errored" must degrade to
/// "token saved in a 0600 file under the 0700 config dir", never to an error
/// that strands a half-enrolled machine (2026-08-28: `security
/// add-generic-password` blocked >60s in a keychain-unlock UI on a session
/// with no login keychain). The downgrade is loud on stderr, and both writes
/// and reads try the keychain first, so a healthy machine never touches the
/// file.
pub struct ResilientStore<P: TokenStore, F: TokenStore> {
    primary: P,
    fallback: F,
}

impl<P: TokenStore, F: TokenStore> ResilientStore<P, F> {
    pub fn new(primary: P, fallback: F) -> Self {
        ResilientStore { primary, fallback }
    }
}

impl<P: TokenStore, F: TokenStore> TokenStore for ResilientStore<P, F> {
    fn store(&self, account: &str, token: &str) -> Result<()> {
        match self.primary.store(account, token) {
            Ok(()) => {
                // A keychain write that succeeds owns the credential; drop any
                // stale file copy from an earlier degraded run.
                let _ = self.fallback.delete(account);
                Ok(())
            }
            Err(error) => {
                eprintln!(
                    "!! keychain write failed: {error}\n\
                     !! Storing the token in {} instead so pairing is not lost.\n\
                     !! It is 0600, but it is on disk at rest.",
                    self.fallback.location()
                );
                self.fallback.store(account, token)
            }
        }
    }

    fn read(&self, account: &str) -> Result<Option<String>> {
        match self.primary.read(account) {
            Ok(Some(token)) => Ok(Some(token)),
            Ok(None) => self.fallback.read(account),
            Err(primary_error) => match self.fallback.read(account)? {
                Some(token) => Ok(Some(token)),
                None => Err(primary_error),
            },
        }
    }

    fn delete(&self, account: &str) -> Result<()> {
        let primary = self.primary.delete(account);
        let fallback = self.fallback.delete(account);
        primary.and(fallback)
    }

    fn location(&self) -> String {
        format!(
            "{} (fallback: {})",
            self.primary.location(),
            self.fallback.location()
        )
    }
}

/// Printed once per process, not once per read, so a polling `run` does not
/// bury the turn output — but it is unmissable when it appears.
fn warn_token_file_downgrade(path: &Path) {
    use std::sync::Once;
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "!! {TOKEN_FILE_ENV} is set: the device token is stored in a plain file,\n\
             !! not the macOS keychain. It is 0600, but it is on disk at rest.\n\
             !!   file: {}\n\
             !! Unset {TOKEN_FILE_ENV} to use the keychain.",
            path.display()
        );
    });
}

/// Owner-only JSON file. Weaker than the keychain — the token is at rest on
/// disk — so it is used only when explicitly requested.
pub struct FileTokenStore {
    path: PathBuf,
}

impl FileTokenStore {
    pub fn new(path: PathBuf) -> Self {
        FileTokenStore { path }
    }

    fn read_all(&self) -> Result<BTreeMap<String, String>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn write_all(&self, entries: &BTreeMap<String, String>) -> Result<()> {
        let bytes = serde_json::to_vec(entries)?;
        crate::paths::write_atomic(&self.path, &bytes, 0o600)
    }
}

impl TokenStore for FileTokenStore {
    fn store(&self, account: &str, token: &str) -> Result<()> {
        let mut entries = self.read_all()?;
        entries.insert(account.to_string(), token.to_string());
        self.write_all(&entries)
    }

    fn read(&self, account: &str) -> Result<Option<String>> {
        Ok(self.read_all()?.get(account).cloned())
    }

    fn delete(&self, account: &str) -> Result<()> {
        let mut entries = self.read_all()?;
        // Removing a key that is not there must not create the file: the
        // resilient store tidies the fallback on every successful keychain
        // write, and a healthy machine should never grow a tokens.json.
        if entries.remove(account).is_some() {
            self.write_all(&entries)?;
        }
        Ok(())
    }

    fn location(&self) -> String {
        format!("file {} (0600)", self.path.display())
    }
}

/// Test double. Never touches the real keychain.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    entries: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn store(&self, account: &str, token: &str) -> Result<()> {
        self.entries
            .lock()
            .map_err(|_| Error::new("token store poisoned"))?
            .insert(account.to_string(), token.to_string());
        Ok(())
    }

    fn read(&self, account: &str) -> Result<Option<String>> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| Error::new("token store poisoned"))?
            .get(account)
            .cloned())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.entries
            .lock()
            .map_err(|_| Error::new("token store poisoned"))?
            .remove(account);
        Ok(())
    }

    fn location(&self) -> String {
        "in-memory test store".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A primary that always fails, standing in for a hung or locked keychain.
    struct FailingStore;

    impl TokenStore for FailingStore {
        fn store(&self, _account: &str, _token: &str) -> Result<()> {
            Err(Error::new("keychain call timed out after 10s"))
        }
        fn read(&self, _account: &str) -> Result<Option<String>> {
            Err(Error::new("keychain call timed out after 10s"))
        }
        fn delete(&self, _account: &str) -> Result<()> {
            Err(Error::new("keychain call timed out after 10s"))
        }
        fn location(&self) -> String {
            "always-failing test keychain".to_string()
        }
    }

    #[test]
    fn resilient_store_saves_and_reads_through_fallback_when_primary_fails() {
        let store = ResilientStore::new(FailingStore, MemoryTokenStore::new());
        store.store("device-1", "secret-value").unwrap();
        assert_eq!(
            store.read("device-1").unwrap().as_deref(),
            Some("secret-value")
        );
    }

    #[test]
    fn resilient_read_surfaces_primary_error_when_fallback_is_empty_too() {
        let store = ResilientStore::new(FailingStore, MemoryTokenStore::new());
        let error = store.read("device-1").unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn resilient_store_prefers_primary_and_clears_stale_fallback_copies() {
        let primary = MemoryTokenStore::new();
        let fallback = MemoryTokenStore::new();
        fallback.store("device-1", "stale-old-token").unwrap();
        let store = ResilientStore::new(primary, fallback);
        store.store("device-1", "fresh-token").unwrap();
        assert_eq!(
            store.read("device-1").unwrap().as_deref(),
            Some("fresh-token")
        );
        // The stale fallback copy is gone: a later primary read miss must not
        // resurrect a rotated-away credential.
        assert_eq!(store.fallback.read("device-1").unwrap(), None);
    }

    #[test]
    fn file_store_delete_of_missing_key_does_not_create_the_file() {
        let dir = crate::testdir::unique_dir("daycare-keychain");
        let path = dir.join("tokens.json");
        let store = FileTokenStore::new(path.clone());
        store.delete("device-1").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn run_with_deadline_kills_a_hung_child() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        let error = run_with_deadline(command, None, Duration::from_millis(200)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn run_with_deadline_returns_output_of_a_fast_child() {
        let mut command = Command::new("/bin/echo");
        command.arg("hello");
        let output = run_with_deadline(command, None, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn memory_store_round_trips() {
        let store = MemoryTokenStore::new();
        assert_eq!(store.read("device-1").unwrap(), None);
        store.store("device-1", "secret-value").unwrap();
        assert_eq!(
            store.read("device-1").unwrap().as_deref(),
            Some("secret-value")
        );
        store.delete("device-1").unwrap();
        assert_eq!(store.read("device-1").unwrap(), None);
    }
}
