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
        create_private_dir(&self.workspaces)?;
        create_private_dir(&self.visits_dir())?;
        create_private_dir(&self.memories_dir())?;
        Ok(())
    }
}

/// Where workspaces go when nothing overrides them: a private directory in the
/// OS temp area, which on macOS is already per-user and mode 0700, so no
/// ancestor of a workspace is writable by another account. Losing it costs
/// nothing — `Workspace::scaffold` rewrites every file it contains.
///
/// The one thing it must not be is a descendant of `$HOME`.
fn default_workspace_root() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    Ok(base.join(format!("claude-daycare-{}", sanitize_segment(&user))))
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
