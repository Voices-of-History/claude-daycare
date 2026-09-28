use crate::{Error, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Render a path as one literal POSIX-shell word for the human-facing `open`
/// command. Single quotes are closed, emitted literally, and reopened.
pub fn shell_quote_path(path: &Path) -> String {
    shell_quote(&path.to_string_lossy())
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// The companion's own state lives under one root, `~/.claude-daycare`. It is a
/// sibling of `~/.claude`, never inside it: the companion must not read or write
/// the user's global Claude configuration or memory.
///
/// Workspaces are the exception, and they are kept somewhere else on purpose.
/// A workspace is the child's **cwd**, and Claude Code builds project memory by
/// walking every ancestor of the cwd, reading both `<dir>/CLAUDE.md` and
/// `<dir>/.claude/CLAUDE.md` at each level. While workspaces sat under
/// `~/.claude-daycare/workspaces/<id>`, `$HOME` was an ancestor, so every turn
/// silently loaded the operator's `~/.claude/CLAUDE.md` — their private global
/// instructions — into a process whose whole job is to send text to our server.
/// Measured on 2.1.220: from a workspace under `$HOME` the child quoted a
/// heading found only in that file; from an identical workspace outside `$HOME`
/// the same probe came back clean. `--setting-sources project` does not prevent
/// this; it governs settings sources, not memory discovery. `--bare` does
/// disable discovery, but it also forces API-key auth and never reads OAuth or
/// the keychain, which would take a turn off the user's subscription.
///
/// So the fix is where the cwd sits, and `Workspace::guard_ancestors` enforces
/// it rather than trusting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
    workspaces: PathBuf,
}

impl Layout {
    /// `DAYCARE_HOME` exists so tests (and a second enrollment) can point the
    /// whole layout at a scratch directory without touching the real one.
    /// `DAYCARE_WORKSPACE_ROOT` moves only the workspaces, for anyone who wants
    /// them somewhere stable and inspectable.
    pub fn discover() -> Result<Self> {
        let explicit_workspaces = std::env::var_os("DAYCARE_WORKSPACE_ROOT").map(PathBuf::from);
        if let Some(explicit) = std::env::var_os("DAYCARE_HOME") {
            let root = PathBuf::from(explicit);
            let workspaces = explicit_workspaces.unwrap_or_else(|| root.join("workspaces"));
            return Ok(Layout { root, workspaces });
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| Error::new("HOME is not set; cannot locate ~/.claude-daycare"))?;
        let workspaces = match explicit_workspaces {
            Some(dir) => dir,
            None => default_workspace_root()?,
        };
        Ok(Layout {
            root: PathBuf::from(home).join(".claude-daycare"),
            workspaces,
        })
    }

    /// Root and workspaces together. Used by tests, which point the whole layout
    /// at one scratch directory that is already outside `$HOME`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let workspaces = root.join("workspaces");
        Layout { root, workspaces }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// Where the resilient token store parks a credential the keychain refused.
    /// Inside the 0700 root; the file itself is written 0600.
    pub fn fallback_token_file(&self) -> PathBuf {
        self.root.join("tokens.json")
    }

    pub fn identities_file(&self) -> PathBuf {
        self.root.join("identities.json")
    }

    pub fn visits_dir(&self) -> PathBuf {
        self.root.join("visits")
    }

    pub fn memories_dir(&self) -> PathBuf {
        self.root.join("memories")
    }

    pub fn memory_file(&self, identity_id: &str) -> PathBuf {
        self.memories_dir()
            .join(format!("{}.json", sanitize_segment(identity_id)))
    }

    pub fn visit_file(&self, visit_id: &str) -> PathBuf {
        self.visits_dir()
            .join(format!("{}.json", sanitize_segment(visit_id)))
    }

    /// Where a detached visit's stdout and stderr land. A visit that dies in
    /// its startup reads leaves its reason here instead of nowhere.
    pub fn visit_log_file(&self, visit_id: &str) -> PathBuf {
        self.visits_dir()
            .join(format!("{}.log", sanitize_segment(visit_id)))
    }

    pub fn sessions_file(&self) -> PathBuf {
        self.root.join("sessions.json")
    }

    /// The child's cwd for one identity. Deliberately not under `self.root` in
    /// a real install — see the type comment.
    pub fn workspace_dir(&self, actor_id: &str) -> PathBuf {
        self.workspaces.join(sanitize_segment(actor_id))
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspaces
    }

    /// The empty folder the weekly usage meter opens Claude in. It sits beside
    /// the workspaces, outside `$HOME`, and never holds a file.
    pub fn usage_meter_dir(&self) -> PathBuf {
        self.workspaces.join("usage-meter")
    }

    /// Claude's last screen from a usage meter that did not answer.
    pub fn usage_meter_screen_file(&self) -> PathBuf {
        self.root.join("usage-meter-last-screen.txt")
    }

    pub fn turns_dir(&self) -> PathBuf {
        self.root.join("turns")
    }

    pub fn turn_file(&self, command_id: &str) -> PathBuf {
        self.turns_dir()
            .join(format!("{}.jsonl", sanitize_segment(command_id)))
    }

    /// Create the root with owner-only permissions. Turn archives are plaintext
    /// Claude transcripts, so the directory must not be group/world readable.
    pub fn ensure_root(&self) -> Result<()> {
        create_private_dir(&self.root)?;
        create_private_dir(&self.turns_dir())?;
        ensure_workspace_root(&self.workspaces)?;
        create_private_dir(&self.visits_dir())?;
        create_private_dir(&self.memories_dir())?;
        Ok(())
    }
}

/// Where workspaces go when nothing overrides them: a private directory in the
/// OS temp area. Losing it costs nothing — `Workspace::scaffold` rewrites every
/// file it contains.
///
/// On macOS `$TMPDIR` is already per-user and mode 0700, and the name keeps
/// `$USER` so existing workspaces (and the notes a character keeps there) stay
/// where they are. Elsewhere the temp area is normally a shared, sticky `/tmp`,
/// so the name uses the numeric uid (`$USER` can be unset under cron or
/// systemd), and `ensure_workspace_root` checks who owns the directory before
/// anything goes into it: another account can create the name first.
///
/// The one thing it must not be is a descendant of `$HOME`.
fn default_workspace_root() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    Ok(base.join(format!("claude-daycare-{}", workspace_root_suffix())))
}

#[cfg(target_os = "macos")]
fn workspace_root_suffix() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    sanitize_segment(&user)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn workspace_root_suffix() -> String {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }.to_string()
}

#[cfg(not(unix))]
fn workspace_root_suffix() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
    sanitize_segment(&user)
}

/// Create the workspace root if it is missing, then refuse it unless it is a
/// real directory (not a symlink), owned by this user, with no group or other
/// permissions. `lstat` comes first and nothing is chmod-ed: a directory that
/// someone else created, or a symlink pointing into this user's files, must be
/// refused, not "repaired" through.
#[cfg(unix)]
pub fn ensure_workspace_root(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Recursive so an explicit DAYCARE_WORKSPACE_ROOT may be nested; an
            // existing directory is accepted here and judged below.
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let metadata = fs::symlink_metadata(path)?;
    let refuse = |why: &str| {
        Error::new(format!(
            "refusing the workspace root {}: {why}. Daycare workspaces must sit in a directory \
             only you can reach; remove it (or set DAYCARE_WORKSPACE_ROOT elsewhere) and retry",
            path.display()
        ))
    };
    if metadata.file_type().is_symlink() {
        return Err(refuse("it is a symlink"));
    }
    if !metadata.is_dir() {
        return Err(refuse("it is not a directory"));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        return Err(refuse(&format!(
            "it is owned by uid {}, not you (uid {euid})",
            metadata.uid()
        )));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(refuse(&format!(
            "its mode is {mode:03o}; it must be 700 (chmod 700 it if you made it)"
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn ensure_workspace_root(path: &Path) -> Result<()> {
    create_private_dir(path)
}

/// An actor id or command id becomes a directory/file name; keep it to
/// characters that cannot escape the layout.
pub fn sanitize_segment(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

pub fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    set_private_permissions(path, 0o700)?;
    Ok(())
}

/// Replace a file in one step so a crash mid-write cannot leave a half-parsed
/// config or session map behind.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    set_private_permissions(&temp, mode)?;
    fs::rename(&temp, path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_keeps_every_artifact_under_one_root() {
        let layout = Layout::at("/tmp/daycare-root");
        assert_eq!(
            layout.config_file(),
            PathBuf::from("/tmp/daycare-root/config.json")
        );
        assert_eq!(
            layout.workspace_dir("actor-1"),
            PathBuf::from("/tmp/daycare-root/workspaces/actor-1")
        );
        assert_eq!(
            layout.turn_file("cmd-9"),
            PathBuf::from("/tmp/daycare-root/turns/cmd-9.jsonl")
        );
        assert_eq!(
            layout.memory_file("actor-1"),
            PathBuf::from("/tmp/daycare-root/memories/actor-1.json")
        );
    }

    #[test]
    fn shell_quoted_paths_cannot_break_the_open_command() {
        let dir = crate::testdir::unique_path("daycare shell; 'quoted'");
        fs::create_dir_all(&dir).unwrap();
        let command = format!("cd {} && pwd -P", shell_quote_path(&dir));
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .output()
            .unwrap();
        assert!(output.status.success(), "{command}");
        assert_eq!(
            PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()),
            dir.canonicalize().unwrap()
        );
    }

    /// The whole point of the relocation: if this default ever slides back
    /// under `$HOME`, every turn starts loading the operator's global CLAUDE.md
    /// again, silently and with no other symptom.
    #[test]
    fn the_default_workspace_root_is_not_inside_the_users_home() {
        let root = default_workspace_root().unwrap();
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            assert!(
                !root.starts_with(&home),
                "workspaces default to {} which is inside {}; a turn would inherit \
                 {}/.claude/CLAUDE.md",
                root.display(),
                home.display(),
                home.display()
            );
        }
        assert!(root.is_absolute(), "{} must be absolute", root.display());
    }

    #[cfg(unix)]
    #[test]
    fn the_workspace_root_is_created_private_and_refused_when_not_ours_alone() {
        use std::os::unix::fs::PermissionsExt;
        let base = crate::testdir::unique_dir("daycare-wsroot");
        let root = base.join("claude-daycare-test");
        ensure_workspace_root(&root).unwrap();
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // Idempotent on a healthy root.
        ensure_workspace_root(&root).unwrap();

        // A loose mode is refused, not silently repaired.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let error = ensure_workspace_root(&root).unwrap_err();
        assert!(error.to_string().contains("755"), "{error}");

        // A symlink squatting the name is refused before anything follows it.
        let target = base.join("elsewhere");
        create_private_dir(&target).unwrap();
        let link = base.join("claude-daycare-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = ensure_workspace_root(&link).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");

        // So is a file.
        let file = base.join("claude-daycare-file");
        fs::write(&file, "").unwrap();
        assert!(ensure_workspace_root(&file).is_err());
        fs::remove_dir_all(&base).ok();
    }

    /// Someone else's directory (root's /, the only one a test can rely on
    /// existing and not being ours) is refused by owner.
    #[cfg(unix)]
    #[test]
    fn a_workspace_root_owned_by_another_account_is_refused() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let error = ensure_workspace_root(Path::new("/")).unwrap_err();
        assert!(error.to_string().contains("owned by uid 0"), "{error}");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_default_workspace_root_is_named_by_uid_not_user() {
        let root = default_workspace_root().unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(
            root.ends_with(format!("claude-daycare-{uid}")),
            "{}",
            root.display()
        );
    }

    #[test]
    fn identifiers_cannot_escape_the_layout() {
        let layout = Layout::at("/tmp/daycare-root");
        assert_eq!(sanitize_segment("../../.claude"), "_______claude");
        assert_eq!(sanitize_segment("../etc/passwd"), "___etc_passwd");
        let escaped = layout.workspace_dir("../../.claude");
        assert!(escaped.starts_with("/tmp/daycare-root/workspaces"));
        assert_eq!(sanitize_segment(""), "unnamed");
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_and_is_owner_only() {
        let dir = crate::testdir::unique_path("daycare-paths");
        create_private_dir(&dir).unwrap();
        let target = dir.join("config.json");
        write_atomic(&target, b"{\"a\":1}", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"a\":1}");
        let strays: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("tmp-"))
            .collect();
        assert!(strays.is_empty(), "temp file survived the write");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        fs::remove_dir_all(&dir).ok();
    }
}
