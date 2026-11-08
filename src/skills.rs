//! The skills this binary carries and installs, in one table.
//!
//! Each skill is embedded with `include_str!` so it can never be a version
//! behind the CLI it calls. `skill install` writes every entry into every
//! agent's user skill library; self-update runs plain `skill install --json`,
//! so a new entry here reaches existing machines on their next update with no
//! other change.
//!
//! Ownership is decided by the frontmatter `name:` line: an edited copy of a
//! Daycare skill is still Daycare's and may be upgraded (with a `.bak`), while
//! a different skill living at the same path is never overwritten unasked.
//!
//! The Daycare skills library (plan task N17) is the other kind:
//! `skill install name@version` fetches one published version from the
//! platform, checks every file against its sha256 before anything is written,
//! and installs it into the same three libraries with a `.daycare-skill.json`
//! marker. The marker is what makes a folder Daycare's to replace or remove:
//! `skill uninstall` removes marker-owned folders and nothing else, and an
//! install never replaces a folder without one.

use crate::paths::sanitize_segment;
use crate::self_update::sha256_hex;
use crate::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledSkill {
    /// The frontmatter name, which is also the directory name.
    pub name: &'static str,
    pub markdown: &'static str,
}

/// The visit skill. Its name and paths predate the table and must not move:
/// installed copies, the README and the self-update refresh all point at them.
pub const DAYCARE: BundledSkill = BundledSkill {
    name: "daycare",
    markdown: include_str!("../skill/SKILL.md"),
};

/// The memory review an owner runs before a challenge (plan task N3).
pub const MEMORY_REVIEW: BundledSkill = BundledSkill {
    name: "daycare-memory-review",
    markdown: include_str!("../skill/daycare-memory-review/SKILL.md"),
};

pub const BUNDLED: [BundledSkill; 2] = [DAYCARE, MEMORY_REVIEW];

pub fn find(name: &str) -> Option<BundledSkill> {
    BUNDLED.iter().copied().find(|skill| skill.name == name)
}

/// Native user skill paths for one skill, verified against each agent's docs
/// (see README). All receive the same embedded source, including before an
/// agent is installed.
pub fn skill_paths(home: &Path, xdg_config_home: &Path, name: &str) -> Vec<PathBuf> {
    vec![
        home.join(".claude/skills").join(name).join("SKILL.md"),
        home.join(".agents/skills").join(name).join("SKILL.md"),
        xdg_config_home
            .join("opencode/skills")
            .join(name)
            .join("SKILL.md"),
    ]
}

/// The three user skill libraries, for library skills: `<library>/<name>/`.
pub fn library_dirs(home: &Path, xdg_config_home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".claude/skills"),
        home.join(".agents/skills"),
        xdg_config_home.join("opencode/skills"),
    ]
}

/// The `name:` value in a skill file's frontmatter, if it has one.
pub fn frontmatter_name(contents: &str) -> Option<String> {
    frontmatter_value(contents, "name")
}

/// One `key: value` line from the leading `---` block. Quotes around the value
/// are dropped; nothing else is interpreted.
pub fn frontmatter_value(contents: &str, key: &str) -> Option<String> {
    let rest = contents.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != key {
            return None;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Ours, by the frontmatter name the skill file carries. An edited copy is
/// still ours; a different skill that happens to live at the same path is
/// not, and is never overwritten unasked.
pub fn is_bundled(contents: &str, skill: &BundledSkill) -> bool {
    let Some(rest) = contents.strip_prefix("---") else {
        return false;
    };
    let Some(end) = rest.find("\n---") else {
        return false;
    };
    let wanted = format!("name: {}", skill.name);
    rest[..end].lines().any(|line| line.trim() == wanted)
}

/// Whether a skill file is one of Daycare's own, under any bundled name.
pub fn is_any_bundled(contents: &str) -> bool {
    BUNDLED.iter().any(|skill| is_bundled(contents, skill))
}

/// The marker a library skill's folder carries. Its presence is what makes
/// the folder Daycare's to replace or remove.
pub const LIBRARY_MARKER: &str = ".daycare-skill.json";
/// Caps on one library version. The platform's own caps are stricter or
/// equal; these keep a bad answer from filling the disk.
pub const MAX_LIBRARY_FILES: usize = 200;
pub const MAX_LIBRARY_FILE_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_LIBRARY_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MARKER_BYTES: u64 = 256 * 1024;

/// `name@version`, each half already a safe path segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRef {
    pub name: String,
    pub version: String,
}

impl SkillRef {
    /// Each half must equal its own `sanitize_segment`: letters, digits, `-`
    /// and `_` only, so neither can climb out of a folder or a URL path. A
    /// bundled skill's name is never a library skill's.
    pub fn parse(text: &str) -> Result<SkillRef> {
        let refuse = |why: &str| {
            Error::new(format!(
                "{text:?} is not a skill to install: {why}. Use name@version, as the skill's \
                 page shows it"
            ))
        };
        let (name, version) = text
            .split_once('@')
            .ok_or_else(|| refuse("it has no @version"))?;
        for (part, what) in [(name, "name"), (version, "version")] {
            if part.is_empty() || part.len() > 100 || sanitize_segment(part) != part {
                return Err(refuse(&format!(
                    "the {what} may use only letters, digits, - and _"
                )));
            }
        }
        if find(name).is_some() {
            return Err(refuse(
                "that name is a bundled Daycare skill; plain `skill install` installs it",
            ));
        }
        Ok(SkillRef {
            name: name.to_string(),
            version: version.to_string(),
        })
    }

    pub fn display(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// One file of a library version, checked against its sha256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// A published version, every file downloaded and verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryVersion {
    pub name: String,
    pub version: String,
    pub licence: Option<String>,
    pub files: Vec<LibraryFile>,
}

/// sha256 of the canonical file list: JSON of the sorted
/// `[{path, bytes, sha256}]`, keys in that order, no whitespace — the same
/// form the platform uses for a challenge entry's manifest. This is what
/// `--accept-sha256` names.
pub fn file_list_sha256<'a>(files: impl IntoIterator<Item = (&'a str, u64, &'a str)>) -> String {
    let mut files: Vec<(&str, u64, &str)> = files.into_iter().collect();
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let items: Vec<String> = files
        .iter()
        .map(|(path, bytes, sha256)| {
            format!(
                "{{\"path\":{},\"bytes\":{bytes},\"sha256\":{}}}",
                Value::String(path.to_string()),
                Value::String(sha256.to_string())
            )
        })
        .collect();
    sha256_hex(format!("[{}]", items.join(",")).as_bytes())
}

impl LibraryVersion {
    pub fn file_list_sha256(&self) -> String {
        file_list_sha256(self.files.iter().map(|file| {
            (
                file.path.as_str(),
                file.bytes.len() as u64,
                file.sha256.as_str(),
            )
        }))
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes.len() as u64).sum()
    }

    /// `path bytes sha256`, one file per line.
    pub fn listing(&self) -> String {
        self.files
            .iter()
            .map(|file| format!("  {} {} {}\n", file.path, file.bytes.len(), file.sha256))
            .collect()
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The same path rule as a challenge upload: relative, forward slashes, no
/// hidden files, a short safe character set.
fn library_path_ok(path: &str) -> bool {
    path.len() <= 200
        && path.split('/').count() <= 8
        && path.split('/').all(|segment| {
            segment
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

/// Check the platform's record for `want`, download every file with `fetch`
/// (the signed-URL download in the CLI), and verify each against its size
/// and sha256. Nothing is written here; any mismatch refuses the whole
/// version.
pub fn verify_version(
    record: &Value,
    want: &SkillRef,
    fetch: &dyn Fn(&str, u64) -> Result<Vec<u8>>,
) -> Result<LibraryVersion> {
    let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
    if text("name").as_deref() != Some(want.name.as_str())
        || text("version").as_deref() != Some(want.version.as_str())
    {
        return Err(Error::new(format!(
            "the platform answered with a different skill than {}; nothing was installed",
            want.display()
        )));
    }
    let files = record
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new("the skill's record has no file list"))?;
    if files.is_empty() || files.len() > MAX_LIBRARY_FILES {
        return Err(Error::new(format!(
            "{} has {} files; a library skill has 1 to {MAX_LIBRARY_FILES}. Nothing was installed",
            want.display(),
            files.len()
        )));
    }
    let mut seen = BTreeSet::new();
    let mut declared = Vec::with_capacity(files.len());
    let mut total = 0u64;
    for file in files {
        let path = file.get("path").and_then(Value::as_str).unwrap_or_default();
        let bytes = file.get("bytes").and_then(Value::as_u64);
        let sha256 = file
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let url = file.get("url").and_then(Value::as_str);
        let refuse = |why: &str| {
            Error::new(format!(
                "{}: the file {path:?} {why}; nothing was installed",
                want.display()
            ))
        };
        if !library_path_ok(path) || path.eq_ignore_ascii_case(LIBRARY_MARKER) {
            return Err(refuse("has a name this runner will not write"));
        }
        if !seen.insert(path.to_ascii_lowercase()) {
            return Err(refuse("is listed twice"));
        }
        let bytes = bytes.ok_or_else(|| refuse("has no size"))?;
        if bytes > MAX_LIBRARY_FILE_BYTES {
            return Err(refuse("is too large"));
        }
        total += bytes;
        if !is_sha256(sha256) {
            return Err(refuse("has no valid sha256"));
        }
        let url = url.ok_or_else(|| refuse("has no download link"))?;
        declared.push((path.to_string(), bytes, sha256.to_string(), url.to_string()));
    }
    if total > MAX_LIBRARY_TOTAL_BYTES {
        return Err(Error::new(format!(
            "{} is larger than {MAX_LIBRARY_TOTAL_BYTES} bytes; nothing was installed",
            want.display()
        )));
    }
    if !seen.contains("skill.md") {
        return Err(Error::new(format!(
            "{} has no SKILL.md; nothing was installed",
            want.display()
        )));
    }
    let mut verified = Vec::with_capacity(declared.len());
    for (path, bytes, sha256, url) in declared {
        let body = fetch(&url, bytes)?;
        if body.len() as u64 != bytes || sha256_hex(&body) != sha256 {
            return Err(Error::new(format!(
                "{}: {path} does not match its sha256; nothing was installed",
                want.display()
            )));
        }
        verified.push(LibraryFile {
            path,
            bytes: body,
            sha256,
        });
    }
    verified.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(LibraryVersion {
        name: want.name.clone(),
        version: want.version.clone(),
        licence: record
            .get("licence")
            .or_else(|| record.get("license"))
            .and_then(Value::as_str)
            .map(str::to_string),
        files: verified,
    })
}

/// The marker of a library skill folder, if `dir` is one: a real folder
/// holding a small regular `.daycare-skill.json` that names `name`.
fn library_marker(dir: &Path, name: &str) -> Option<Value> {
    let folder = fs::symlink_metadata(dir).ok()?;
    if folder.file_type().is_symlink() || !folder.is_dir() {
        return None;
    }
    let marker = dir.join(LIBRARY_MARKER);
    let metadata = fs::symlink_metadata(&marker).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_MARKER_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(&fs::read(&marker).ok()?).ok()?;
    (value.get("name").and_then(Value::as_str) == Some(name)).then_some(value)
}

fn temp_sibling(target: &Path, what: &str) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.daycare-{what}-{}", std::process::id()))
}

/// Write a verified version into every library, with its marker. Refuses
/// before writing anything if any library already holds a folder of that name
/// that is not a library skill. Each library's folder is built beside it and
/// swapped in whole, so a reader never sees half a skill.
pub fn install_library_skill(
    version: &LibraryVersion,
    libraries: &[PathBuf],
    source: &str,
    installed_at: &str,
) -> Result<Vec<PathBuf>> {
    let targets: Vec<PathBuf> = libraries
        .iter()
        .map(|lib| lib.join(&version.name))
        .collect();
    for target in &targets {
        if fs::symlink_metadata(target).is_ok() && library_marker(target, &version.name).is_none() {
            return Err(Error::new(format!(
                "{} holds a skill that is not from the Daycare library; leaving it alone. Move \
                 it, then install again. Nothing was installed",
                target.display()
            )));
        }
    }
    let marker = json!({
        "name": version.name,
        "version": version.version,
        "licence": version.licence,
        "source": source,
        "installed_at": installed_at,
        "file_list_sha256": version.file_list_sha256(),
        "files": version.files.iter().map(|file| json!({
            "path": file.path, "bytes": file.bytes.len(), "sha256": file.sha256,
        })).collect::<Vec<_>>(),
    });
    let mut marker_text = serde_json::to_string_pretty(&marker)?;
    marker_text.push('\n');

    // Stage every library first; a failure leaves no staged folder behind.
    let mut staged = Vec::new();
    let stage = |target: &Path| -> Result<PathBuf> {
        let staging = temp_sibling(target, "install");
        if fs::symlink_metadata(&staging).is_ok() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        for file in &version.files {
            let path = staging.join(&file.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            crate::paths::write_atomic(&path, &file.bytes, 0o644)?;
        }
        crate::paths::write_atomic(&staging.join(LIBRARY_MARKER), marker_text.as_bytes(), 0o644)?;
        Ok(staging)
    };
    for target in &targets {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        match stage(target) {
            Ok(staging) => staged.push(staging),
            Err(error) => {
                for staging in &staged {
                    let _ = fs::remove_dir_all(staging);
                }
                let _ = fs::remove_dir_all(temp_sibling(target, "install"));
                return Err(Error::new(format!(
                    "could not write the skill into {}: {error}. Nothing was installed",
                    target.display()
                )));
            }
        }
    }
    for (target, staging) in targets.iter().zip(&staged) {
        if fs::symlink_metadata(target).is_ok() {
            let old = temp_sibling(target, "old");
            fs::rename(target, &old)?;
            fs::rename(staging, target)?;
            fs::remove_dir_all(&old)?;
        } else {
            fs::rename(staging, target)?;
        }
    }
    Ok(targets)
}

/// What `skill uninstall` did in each library.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uninstalled {
    pub removed: Vec<PathBuf>,
    /// Folders of that name that are not library skills: left alone.
    pub left: Vec<PathBuf>,
}

/// Remove `<library>/<name>/` wherever it is a marker-owned library skill,
/// and nothing else.
pub fn uninstall_library_skill(name: &str, libraries: &[PathBuf]) -> Result<Uninstalled> {
    if name.is_empty() || sanitize_segment(name) != name {
        return Err(Error::new(format!(
            "{name:?} is not a skill name; names use only letters, digits, - and _"
        )));
    }
    let mut done = Uninstalled::default();
    for library in libraries {
        let target = library.join(name);
        if fs::symlink_metadata(&target).is_err() {
            continue;
        }
        if library_marker(&target, name).is_some() {
            fs::remove_dir_all(&target)?;
            done.removed.push(target);
        } else {
            done.left.push(target);
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_skill_names_itself_in_its_own_frontmatter() {
        for skill in BUNDLED {
            assert!(is_bundled(skill.markdown, &skill), "{}", skill.name);
            assert_eq!(
                frontmatter_name(skill.markdown).as_deref(),
                Some(skill.name)
            );
            // A skill must not claim another bundled skill's name, or an
            // install would treat one as an edited copy of the other.
            for other in BUNDLED.iter().filter(|other| other.name != skill.name) {
                assert!(!is_bundled(skill.markdown, other));
            }
        }
    }

    #[test]
    fn ownership_is_by_exact_name_not_prefix() {
        let review = "---\nname: daycare-memory-review\n---\n";
        assert!(is_bundled(review, &MEMORY_REVIEW));
        assert!(!is_bundled(review, &DAYCARE));
        assert!(!is_bundled("---\nname: daycare-extra\n---\n", &DAYCARE));
        assert!(!is_bundled("name: daycare\n", &DAYCARE));
        assert!(is_any_bundled("---\nname: daycare\n---\nedited"));
        assert!(!is_any_bundled("---\nname: someone-elses\n---\n"));
    }

    #[test]
    fn frontmatter_values_are_read_without_quotes() {
        let text = "---\nname: \"my-skill\"\ndescription: Does a thing\n---\nbody";
        assert_eq!(frontmatter_name(text).as_deref(), Some("my-skill"));
        assert_eq!(
            frontmatter_value(text, "description").as_deref(),
            Some("Does a thing")
        );
        assert_eq!(frontmatter_name("no frontmatter"), None);
    }

    #[test]
    fn the_visit_skill_keeps_its_original_paths() {
        let paths = skill_paths(Path::new("/h"), Path::new("/h/.config"), DAYCARE.name);
        assert_eq!(
            paths,
            [
                PathBuf::from("/h/.claude/skills/daycare/SKILL.md"),
                PathBuf::from("/h/.agents/skills/daycare/SKILL.md"),
                PathBuf::from("/h/.config/opencode/skills/daycare/SKILL.md"),
            ]
        );
    }

    #[test]
    fn a_skill_reference_is_two_safe_segments() {
        assert_eq!(
            SkillRef::parse("tidy-csv@3").unwrap(),
            SkillRef {
                name: "tidy-csv".into(),
                version: "3".into()
            }
        );
        for bad in [
            "../x@1",
            "x@../1",
            "x",
            "@1",
            "x@",
            "x@1.0",
            "a/b@1",
            "x@1@2",
            "daycare@1",
        ] {
            assert!(SkillRef::parse(bad).is_err(), "{bad}");
        }
    }

    fn record(files: &[(&str, &str)]) -> Value {
        json!({
            "name": "tidy-csv", "version": "3", "licence": "CC-BY-4.0",
            "files": files.iter().map(|(path, body)| json!({
                "path": path, "bytes": body.len(), "sha256": sha256_hex(body.as_bytes()),
                "url": format!("http://127.0.0.1/{path}"),
            })).collect::<Vec<_>>(),
        })
    }

    fn serve(
        files: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str, u64) -> Result<Vec<u8>> {
        move |url: &str, _max: u64| {
            let path = url.trim_start_matches("http://127.0.0.1/");
            Ok(files
                .iter()
                .find(|(name, _)| *name == path)
                .map(|(_, body)| body.as_bytes().to_vec())
                .unwrap_or_default())
        }
    }

    const FILES: &[(&str, &str)] = &[
        ("SKILL.md", "---\nname: tidy-csv\n---\nTidy.\n"),
        ("ref/a.md", "a"),
    ];

    #[test]
    fn every_file_is_verified_before_anything_is_written() {
        let want = SkillRef::parse("tidy-csv@3").unwrap();
        let version = verify_version(&record(FILES), &want, &serve(FILES)).unwrap();
        assert_eq!(version.files.len(), 2);
        assert_eq!(version.licence.as_deref(), Some("CC-BY-4.0"));

        static TAMPERED: &[(&str, &str)] = &[
            ("SKILL.md", "---\nname: tidy-csv\n---\nTidy.\n"),
            ("ref/a.md", "b"),
        ];
        let error = verify_version(&record(FILES), &want, &serve(TAMPERED)).unwrap_err();
        assert!(
            error
                .message()
                .contains("ref/a.md does not match its sha256"),
            "{error}"
        );

        for bad in [
            record(&[("SKILL.md", "x"), ("../escape.md", "x")]),
            record(&[("SKILL.md", "x"), (".daycare-skill.json", "{}")]),
            record(&[("SKILL.md", "x"), ("skill.MD", "y")]),
            record(&[("notes.md", "x")]),
            json!({"name": "other", "version": "3", "files": []}),
        ] {
            assert!(verify_version(&bad, &want, &serve(FILES)).is_err(), "{bad}");
        }
    }

    #[test]
    fn install_writes_a_marker_and_uninstall_removes_only_marker_owned_folders() {
        let root = crate::testdir::unique_dir("library-skill");
        let libraries = vec![root.join("a/skills"), root.join("b/skills")];
        let want = SkillRef::parse("tidy-csv@3").unwrap();
        let version = verify_version(&record(FILES), &want, &serve(FILES)).unwrap();
        let paths = install_library_skill(&version, &libraries, "http://127.0.0.1", "now").unwrap();
        for path in &paths {
            assert_eq!(fs::read_to_string(path.join("ref/a.md")).unwrap(), "a");
            let marker = library_marker(path, "tidy-csv").unwrap();
            assert_eq!(marker["version"], "3");
            assert_eq!(marker["file_list_sha256"], version.file_list_sha256());
        }
        // Upgrading replaces the whole folder.
        fs::write(paths[0].join("stray.md"), "x").unwrap();
        install_library_skill(&version, &libraries, "http://127.0.0.1", "later").unwrap();
        assert!(!paths[0].join("stray.md").exists());

        // A folder of that name without a marker is never replaced or removed.
        let own = root.join("c/skills/tidy-csv");
        fs::create_dir_all(&own).unwrap();
        fs::write(own.join("SKILL.md"), "mine").unwrap();
        let mut with_own = libraries.clone();
        with_own.push(root.join("c/skills"));
        assert!(install_library_skill(&version, &with_own, "x", "y").is_err());
        let done = uninstall_library_skill("tidy-csv", &with_own).unwrap();
        assert_eq!(done.removed, paths);
        assert_eq!(done.left, vec![own.clone()]);
        assert_eq!(fs::read_to_string(own.join("SKILL.md")).unwrap(), "mine");
        assert!(uninstall_library_skill("../x", &with_own).is_err());
    }

    #[test]
    fn the_file_list_hash_is_order_independent() {
        let a = file_list_sha256([("b.md", 1, "b"), ("a.md", 2, "a")]);
        let b = file_list_sha256([("a.md", 2, "a"), ("b.md", 1, "b")]);
        assert_eq!(a, b);
        assert_eq!(
            a,
            sha256_hex(br#"[{"path":"a.md","bytes":2,"sha256":"a"},{"path":"b.md","bytes":1,"sha256":"b"}]"#)
        );
    }
}
