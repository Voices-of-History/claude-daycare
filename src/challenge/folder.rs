//! The challenge folder (plan task N7): `~/daycare/challenges/<id>/`, 0700.
//!
//! ```text
//! BRIEF.md            the task, written from the platform's brief
//! inputs/             the challenge's files, each checked against its sha256
//! approved/private/   copies of approved memory, private use only
//! approved/public/    copies of approved memory, public use allowed
//! CLAUDE.md|AGENTS.md a pointer Daycare writes for the chosen client
//! output/             empty; the only folder that is ever uploaded
//! .daycare-record.json  which entry this folder belongs to, and what it holds
//! meter/              start.json and end.json, the weekly-allowance readings
//!                     taken around an interactive session (`challenge::meter`)
//! ```
//!
//! Nothing is written until every input file has been downloaded and has
//! matched its declared size and hash, so a bad seed leaves no folder. No
//! write follows a symlink inside the folder, and copies of approved memory
//! are checked against the approved hash as they are made: a file edited
//! after approval is refused here as well as by `challenge start`.
//!
//! The record is not called `ENTRY.json`: that is the name of the declaration
//! every brief asks the agent to write in `output/`, and a real session read
//! the record while writing its own declaration. As a dot-file it is also left
//! out of the file listings both clients use by default.
//!
//! The folder lives under `$HOME` by default, so `$HOME` is an ancestor and
//! Claude would load `~/CLAUDE.md` and `~/.claude/CLAUDE.md` from it. That is
//! deliberate: those layers are excluded by the generated settings (see
//! `exclusion`), not avoided by location. `DAYCARE_CHALLENGE_ROOT` moves the
//! folders elsewhere.

use crate::approval::scan::{tree_digest, Client, Item, Kind};
use crate::approval::Scope;
use crate::paths::{sanitize_segment, write_atomic};
use crate::self_update::sha256_hex;
use crate::{Error, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const CHALLENGE_ROOT_ENV: &str = "DAYCARE_CHALLENGE_ROOT";

/// The runner's record of a challenge folder, at the folder's top.
pub const RECORD_FILE: &str = ".daycare-record.json";

/// The largest single input file a challenge may declare.
pub const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;
/// The largest approved instruction file copied into a folder.
const MAX_COPY_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// The largest approved skill copied into a folder.
const MAX_COPY_SKILL_BYTES: u64 = 32 * 1024 * 1024;

/// `DAYCARE_CHALLENGE_ROOT`, or `~/daycare/challenges`.
pub fn challenge_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os(CHALLENGE_ROOT_ENV).filter(|v| !v.is_empty()) {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(Error::new(format!(
                "{CHALLENGE_ROOT_ENV} must be an absolute path"
            )));
        }
        return Ok(root);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::new("HOME is not set; cannot place the challenge folder"))?;
    Ok(home.join("daycare").join("challenges"))
}

/// One input file as the platform declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub url: String,
}

/// A downloaded input that matched its declaration, held until the folder
/// is written.
#[derive(Debug, Clone)]
pub struct StagedInput {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// The same rule as the platform's `daycare_challenge_inputs_path_safe`
/// check, applied again here: relative, no `..`, no backslash, no empty or
/// dot segments.
pub fn validate_input_path(path: &str) -> Result<PathBuf> {
    let refuse = || Error::new(format!("refusing an input file named {path:?}"));
    if path.is_empty()
        || path.len() > 240
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.contains("//")
        || path.contains("..")
    {
        return Err(refuse());
    }
    let relative = PathBuf::from(path);
    for component in relative.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => {}
            _ => return Err(refuse()),
        }
    }
    if path.split('/').any(|part| part.is_empty() || part == ".") {
        return Err(refuse());
    }
    Ok(relative)
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Download one input and check it against its declaration. `fetch` is the
/// signed-URL download in the CLI and a closure in tests.
pub fn stage_input(
    input: &InputFile,
    fetch: &dyn Fn(&str, u64) -> Result<Vec<u8>>,
) -> Result<StagedInput> {
    let path = validate_input_path(&input.path)?;
    if !is_sha256(&input.sha256) {
        return Err(Error::new(format!(
            "the input file {} has no valid sha256; refusing it",
            input.path
        )));
    }
    if input.bytes > MAX_INPUT_BYTES {
        return Err(Error::new(format!(
            "the input file {} is larger than {MAX_INPUT_BYTES} bytes; refusing it",
            input.path
        )));
    }
    let bytes = fetch(&input.url, input.bytes)?;
    if bytes.len() as u64 != input.bytes {
        return Err(Error::new(format!(
            "the input file {} is {} bytes, not the {} the challenge declared; nothing was set up",
            input.path,
            bytes.len(),
            input.bytes
        )));
    }
    let sha256 = sha256_hex(&bytes);
    if sha256 != input.sha256 {
        return Err(Error::new(format!(
            "the input file {} does not match its sha256; nothing was set up",
            input.path
        )));
    }
    Ok(StagedInput {
        path,
        bytes,
        sha256,
    })
}

/// Create `path` as a 0700 directory, or accept an existing one only if it is
/// a real directory (not a symlink), owned by this user, with no group or
/// other permissions. Nothing is chmod-ed into shape.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
            // `recursive` applies the mode to every directory it made, but the
            // umask may have narrowed it; set the leaf exactly.
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let metadata = fs::symlink_metadata(path)?;
    let refuse = |why: String| {
        Error::new(format!(
            "refusing the challenge folder {}: {why}",
            path.display()
        ))
    };
    if metadata.file_type().is_symlink() {
        return Err(refuse("it is a symlink".into()));
    }
    if !metadata.is_dir() {
        return Err(refuse("it is not a directory".into()));
    }
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid {
        return Err(refuse(format!("it is owned by uid {}", metadata.uid())));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(refuse(format!(
            "its mode is {mode:03o}; it must be 700 (chmod 700 it if you made it)"
        )));
    }
    Ok(())
}

/// The folder that holds challenge folders may be a normal directory (say,
/// 755), but never a symlink, someone else's, or writable by others.
pub fn ensure_root(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if fs::symlink_metadata(path).is_err() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != euid
        || metadata.mode() & 0o022 != 0
    {
        return Err(Error::new(format!(
            "refusing {} as the place for challenge folders: it must be your own directory, \
             not a symlink, and not writable by others. Set {CHALLENGE_ROOT_ENV} to another \
             folder if you need to",
            path.display()
        )));
    }
    Ok(())
}

/// Make every directory from `folder` down to `relative`'s parent, refusing a
/// symlink at any step and at the target itself. Returns the target path.
fn safe_target(folder: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = folder.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            return Err(Error::new(format!(
                "refusing to write {} inside the challenge folder",
                relative.display()
            )));
        };
        current.push(part);
        let last = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::new(format!(
                    "{} is a symlink; refusing to write through it",
                    current.display()
                )))
            }
            Ok(metadata) if !last && !metadata.is_dir() => {
                return Err(Error::new(format!(
                    "{} is in the way; it should be a folder",
                    current.display()
                )))
            }
            Ok(metadata) if last && metadata.is_dir() => {
                return Err(Error::new(format!(
                    "{} is a folder; expected a file",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !last {
                    ensure_private_dir(&current)?;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(current)
}

fn write_in(folder: &Path, relative: &str, bytes: &[u8]) -> Result<PathBuf> {
    let target = safe_target(folder, Path::new(relative))?;
    write_atomic(&target, bytes, 0o600)?;
    Ok(target)
}

/// Write one file Daycare owns inside an existing challenge folder (the meter
/// readings), 0600, creating private folders on the way and never following a
/// symlink.
pub fn write_file_in(folder: &Path, relative: &str, bytes: &[u8]) -> Result<PathBuf> {
    ensure_private_dir(folder)?;
    write_in(folder, relative, bytes)
}

/// Remove one file Daycare owns inside the folder, if it is there. A symlink
/// on the way is refused, as for writes; the link itself is never followed.
pub fn remove_file_in(folder: &Path, relative: &str) -> Result<()> {
    let target = safe_target(folder, Path::new(relative))?;
    match fs::remove_file(&target) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// A directory inside the folder, created private, never a symlink.
fn dir_in(folder: &Path, relative: &str) -> Result<PathBuf> {
    let path = folder.join(relative);
    ensure_private_dir(&path)?;
    Ok(path)
}

/// Where one approved item went. `copy` is relative to the folder; `None`
/// means the item is used by reference (a plugin or an MCP server), never
/// copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedCopy {
    pub item: Item,
    pub scope: Scope,
    pub copy: Option<PathBuf>,
}

/// Copy the approved items of one client into `approved/<scope>/`.
pub fn copy_approved(
    folder: &Path,
    approved: &[(Item, Scope)],
    client: Client,
) -> Result<Vec<ApprovedCopy>> {
    let approved_dir = folder.join("approved");
    // A previous start's copies are replaced wholesale, so an item no longer
    // approved does not linger.
    match fs::symlink_metadata(&approved_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(Error::new(format!(
                "{} is not a folder Daycare made; remove it and retry",
                approved_dir.display()
            )))
        }
        Ok(_) => fs::remove_dir_all(&approved_dir)?,
        Err(_) => {}
    }
    dir_in(folder, "approved")?;
    let mut copies = Vec::new();
    for (index, (item, scope)) in approved
        .iter()
        .filter(|(item, _)| item.client == client)
        .enumerate()
    {
        let copy = match item.kind {
            Kind::Plugin | Kind::McpServer => None,
            Kind::Skill => {
                let name = sanitize_segment(item.name.as_deref().unwrap_or("skill"));
                let relative = format!("approved/{}/skills/{name}", scope.as_str());
                copy_skill(folder, &relative, item)?;
                Some(PathBuf::from(relative))
            }
            _ => {
                let base = item
                    .path
                    .file_name()
                    .map(|name| sanitize_file_name(&name.to_string_lossy()))
                    .unwrap_or_else(|| "memory.md".into());
                let relative = format!(
                    "approved/{}/{:02}-{}-{base}",
                    scope.as_str(),
                    index + 1,
                    item.kind.as_str()
                );
                copy_file(folder, &relative, item)?;
                Some(PathBuf::from(relative))
            }
        };
        copies.push(ApprovedCopy {
            item: item.clone(),
            scope: *scope,
            copy,
        });
    }
    Ok(copies)
}

fn sanitize_file_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_start_matches('.')
        .to_string()
}

fn changed_since_approval(item: &Item) -> Error {
    Error::new(format!(
        "{} changed after you approved it; review it again before starting",
        item.path.display()
    ))
}

fn copy_file(folder: &Path, relative: &str, item: &Item) -> Result<()> {
    let bytes = fs::read(&item.path)
        .map_err(|error| Error::new(format!("could not copy {}: {error}", item.path.display())))?;
    if bytes.len() as u64 > MAX_COPY_FILE_BYTES {
        return Err(Error::new(format!(
            "{} is too large to bring into a challenge",
            item.path.display()
        )));
    }
    if sha256_hex(&bytes) != item.sha256 {
        return Err(changed_since_approval(item));
    }
    write_in(folder, relative, &bytes)?;
    Ok(())
}

/// Regular files only; symlinks inside a skill are not followed or copied.
fn copy_skill(folder: &Path, relative: &str, item: &Item) -> Result<()> {
    let (bytes, digest) = tree_digest(&item.path)?;
    if digest != item.sha256 {
        return Err(changed_since_approval(item));
    }
    if bytes > MAX_COPY_SKILL_BYTES {
        return Err(Error::new(format!(
            "the skill at {} is too large to bring into a challenge",
            item.path.display()
        )));
    }
    let destination = safe_target(folder, Path::new(relative))?;
    ensure_private_dir(&destination)?;
    let mut stack = vec![PathBuf::new()];
    while let Some(sub) = stack.pop() {
        for entry in fs::read_dir(item.path.join(&sub))?.flatten() {
            let rel = sub.join(entry.file_name());
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                ensure_private_dir(&destination.join(&rel))?;
                stack.push(rel);
            } else if metadata.is_file() {
                let contents = fs::read(entry.path())?;
                write_atomic(&destination.join(&rel), &contents, 0o600)?;
            }
        }
    }
    Ok(())
}

/// The parts of the platform's challenge record the folder needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeDetail {
    pub id: String,
    pub slug: Option<String>,
    pub title: String,
    pub state: Option<String>,
    pub brief_md: String,
    pub closes_at: Option<String>,
    /// Vote, goal, third component points.
    pub weights: Option<(i64, i64, i64)>,
    pub third_label: Option<String>,
    pub inputs: Vec<InputFile>,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.is_empty())
}

/// A challenge id becomes a URL segment and a folder name; it must already be
/// a safe one (a uuid or a slug).
pub fn validate_challenge_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 || sanitize_segment(id) != id {
        return Err(Error::new(format!(
            "{id:?} is not a challenge id; use the id or slug from `challenge list`"
        )));
    }
    Ok(())
}

impl ChallengeDetail {
    /// Read leniently: `{challenge: {...}, inputs: [...]}` or a flat record,
    /// weights as `{weights: {vote, goal, third}}` or `w_vote`/`w_goal`/`w_third`.
    pub fn parse(value: &Value) -> Result<ChallengeDetail> {
        let record = value
            .get("challenge")
            .filter(|inner| inner.is_object())
            .unwrap_or(value);
        let id =
            text(record, "id").ok_or_else(|| Error::new("the challenge response has no id"))?;
        validate_challenge_id(&id)?;
        let weights = match record.get("weights") {
            Some(weights) => Some((
                weights.get("vote").and_then(Value::as_i64).unwrap_or(0),
                weights.get("goal").and_then(Value::as_i64).unwrap_or(0),
                weights.get("third").and_then(Value::as_i64).unwrap_or(0),
            )),
            None => match (
                record.get("w_vote").and_then(Value::as_i64),
                record.get("w_goal").and_then(Value::as_i64),
                record.get("w_third").and_then(Value::as_i64),
            ) {
                (Some(v), Some(g), Some(t)) => Some((v, g, t)),
                _ => None,
            },
        };
        let inputs_value = value
            .get("inputs")
            .or_else(|| record.get("inputs"))
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        let inputs = inputs_value
            .as_array()
            .ok_or_else(|| Error::new("the challenge's inputs are not a list"))?
            .iter()
            .map(|input| {
                Ok(InputFile {
                    path: text(input, "path")
                        .ok_or_else(|| Error::new("an input file has no path"))?,
                    bytes: input
                        .get("bytes")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| Error::new("an input file has no size"))?,
                    sha256: text(input, "sha256").unwrap_or_default(),
                    url: text(input, "url")
                        .or_else(|| text(input, "signed_url"))
                        .ok_or_else(|| Error::new("an input file has no download link"))?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ChallengeDetail {
            title: text(record, "title").unwrap_or_else(|| id.clone()),
            slug: text(record, "slug"),
            state: text(record, "state").or_else(|| text(record, "phase")),
            brief_md: text(record, "brief_md")
                .or_else(|| text(record, "brief"))
                .unwrap_or_default(),
            closes_at: text(record, "closes_at"),
            third_label: text(record, "third_label"),
            weights,
            inputs,
            id,
        })
    }

    /// The challenge without download links, for printing.
    pub fn public_json(&self) -> Value {
        json!({
            "id": self.id,
            "slug": self.slug,
            "title": self.title,
            "state": self.state,
            "closes_at": self.closes_at,
            "weights": self.weights.map(|(vote, goal, third)| json!({
                "vote": vote, "goal": goal, "third": third,
            })),
            "third_label": self.third_label,
            "brief_md": self.brief_md,
            "inputs": self.inputs.iter().map(|input| json!({
                "path": input.path, "bytes": input.bytes, "sha256": input.sha256,
            })).collect::<Vec<_>>(),
        })
    }
}

pub fn brief_text(detail: &ChallengeDetail) -> String {
    let mut text = format!("# {}\n\n", detail.title);
    text.push_str(detail.brief_md.trim_end());
    text.push_str("\n");
    if let Some((vote, goal, third)) = detail.weights {
        text.push_str("\n## How it is scored\n\n");
        text.push_str(&format!("- Votes: up to {vote} points\n"));
        text.push_str(&format!("- Goal checks: up to {goal} points\n"));
        let label = detail.third_label.as_deref().unwrap_or("Third component");
        text.push_str(&format!("- {label}: up to {third} points\n"));
    }
    if !detail.inputs.is_empty() {
        text.push_str("\n## Input files\n\n");
        for input in &detail.inputs {
            text.push_str(&format!(
                "- `inputs/{}` ({} bytes)\n",
                input.path, input.bytes
            ));
        }
    }
    if let Some(closes_at) = &detail.closes_at {
        text.push_str(&format!("\nEntries close at {closes_at}.\n"));
    }
    text
}

/// The pointer file Daycare writes: `CLAUDE.md` for Claude Code (approved
/// instruction files are imported with `@`), `AGENTS.md` for Codex (they are
/// listed, since AGENTS.md has no imports).
pub fn pointer_text(client: Client, detail: &ChallengeDetail, copies: &[ApprovedCopy]) -> String {
    let mut text = format!(
        "# Daycare challenge: {}\n\n\
         Daycare wrote this file to set up this folder.\n\n\
         - Read `BRIEF.md` first. It is the task.\n\
         - The input files are in `inputs/`. Do not change them.\n\
         - Put everything you want to submit in `output/`. Only `output/` is uploaded, and \
           only after your owner types yes.\n\
         - Work in this folder. Leave `.daycare-record.json` and `meter/` alone: they are \
           Daycare's records.\n\n\
         ## Memory your owner approved\n\n\
         Private items help you do the work. Do not copy them, or anything that gives them \
         away, into `output/`. Public items may also appear in your entry. Your owner did \
         not approve anything else; do not look for their other memory.\n",
        detail.title
    );
    for scope in [Scope::Private, Scope::Public] {
        let chosen: Vec<&ApprovedCopy> = copies.iter().filter(|copy| copy.scope == scope).collect();
        text.push_str(match scope {
            Scope::Private => "\n### Private\n\n",
            Scope::Public => "\n### Public\n\n",
        });
        if chosen.is_empty() {
            text.push_str("None.\n");
            continue;
        }
        for copy in chosen {
            let name = copy.item.name.as_deref().unwrap_or("");
            let line = match (copy.item.kind, &copy.copy, client) {
                (Kind::McpServer, _, _) => format!("- MCP server `{name}`\n"),
                (Kind::Plugin, _, _) => format!("- Plugin `{name}`\n"),
                (Kind::Skill, Some(path), Client::Claude) => {
                    format!(
                        "- Skill `{name}` (available as a skill; copy in `{}`)\n",
                        path.display()
                    )
                }
                (Kind::Skill, Some(path), Client::Codex) => format!(
                    "- Skill `{name}`: read `{}/SKILL.md` when the task fits it\n",
                    path.display()
                ),
                (Kind::Command, Some(path), _) => {
                    format!("- Command `/{name}` (copy in `{}`)\n", path.display())
                }
                (_, Some(path), Client::Claude) => format!("@{}\n", path.display()),
                (_, Some(path), Client::Codex) => {
                    format!("- Read `{}` before you start\n", path.display())
                }
                (_, None, _) => continue,
            };
            text.push_str(&line);
        }
    }
    text
}

/// What `challenge start` writes, gathered.
pub struct FolderSpec<'a> {
    pub folder: &'a Path,
    pub client: Client,
    pub detail: &'a ChallengeDetail,
    pub inputs: &'a [StagedInput],
    pub approved: &'a [(Item, Scope)],
    pub entry: Value,
}

/// Write the whole folder. Run only after every input has been staged.
pub fn write_folder(spec: &FolderSpec<'_>) -> Result<Vec<ApprovedCopy>> {
    ensure_private_dir(spec.folder)?;
    write_in(spec.folder, "BRIEF.md", brief_text(spec.detail).as_bytes())?;
    dir_in(spec.folder, "inputs")?;
    for input in spec.inputs {
        let relative = Path::new("inputs").join(&input.path);
        let target = safe_target(spec.folder, &relative)?;
        write_atomic(&target, &input.bytes, 0o600)?;
    }
    refuse_symlinks_under(&spec.folder.join("inputs"))?;
    let copies = copy_approved(spec.folder, spec.approved, spec.client)?;
    let pointer = match spec.client {
        Client::Claude => "CLAUDE.md",
        Client::Codex => "AGENTS.md",
    };
    write_in(
        spec.folder,
        pointer,
        pointer_text(spec.client, spec.detail, &copies).as_bytes(),
    )?;
    dir_in(spec.folder, "output")?;
    let mut entry = spec.entry.clone();
    entry["approved"] = Value::Array(
        copies
            .iter()
            .map(|copy| {
                json!({
                    "id": copy.item.id,
                    "scope": copy.scope,
                    "sha256": copy.item.sha256,
                    "copy": copy.copy,
                })
            })
            .collect(),
    );
    entry["inputs"] = Value::Array(
        spec.inputs
            .iter()
            .map(|input| {
                json!({
                    "path": input.path,
                    "bytes": input.bytes.len(),
                    "sha256": input.sha256,
                })
            })
            .collect(),
    );
    let mut entry_text = serde_json::to_string_pretty(&entry)?;
    entry_text.push('\n');
    write_in(spec.folder, RECORD_FILE, entry_text.as_bytes())?;
    Ok(copies)
}

fn refuse_symlinks_under(dir: &Path) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)?.flatten() {
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(Error::new(format!(
                    "{} is a symlink; input files must be real files",
                    entry.path().display()
                )));
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::scan::item_id;
    use crate::testdir::unique_dir;
    use std::os::unix::fs::PermissionsExt;

    fn detail(inputs: Vec<InputFile>) -> ChallengeDetail {
        ChallengeDetail {
            id: "c1".into(),
            slug: Some("signup-rescue".into()),
            title: "Signup Rescue".into(),
            state: Some("open".into()),
            brief_md: "Fix the form.".into(),
            closes_at: Some("2026-11-20T00:00:00Z".into()),
            weights: Some((40, 40, 20)),
            third_label: Some("Accessibility".into()),
            inputs,
        }
    }

    fn input(path: &str, bytes: &[u8]) -> InputFile {
        InputFile {
            path: path.into(),
            bytes: bytes.len() as u64,
            sha256: sha256_hex(bytes),
            url: format!("https://storage.test/{path}"),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn input_paths_follow_the_platforms_rule() {
        for good in ["a.csv", "data/sheet.csv", "x-y_z/1.txt"] {
            assert!(validate_input_path(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "a\\b",
            "./a",
            "a/./b",
            "a/",
            "..",
        ] {
            assert!(validate_input_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_wrong_sha256_or_size_aborts_before_anything_is_written() {
        let bytes = b"name,email\n".to_vec();
        let mut wrong = input("form.csv", &bytes);
        wrong.sha256 = sha256_hex(b"something else");
        let fetch = |_: &str, _: u64| Ok(bytes.clone());
        assert!(stage_input(&wrong, &fetch)
            .unwrap_err()
            .message()
            .contains("sha256"));
        let mut short = input("form.csv", &bytes);
        short.bytes += 1;
        assert!(stage_input(&short, &fetch).is_err());
        let staged = stage_input(&input("form.csv", &bytes), &fetch).unwrap();
        assert_eq!(staged.bytes, bytes);
    }

    #[test]
    fn the_folder_is_private_and_holds_every_part() {
        let base = unique_dir("daycare-folder");
        let folder = base.join("challenges/c1");
        let memory = base.join("CLAUDE.md");
        fs::write(&memory, "approved memory\n").unwrap();
        let item = Item {
            id: item_id(Client::Claude, Kind::ClaudeMd, &memory, None),
            client: Client::Claude,
            kind: Kind::ClaudeMd,
            path: memory.clone(),
            name: None,
            bytes: 16,
            sha256: sha256_hex(b"approved memory\n"),
            origin: None,
        };
        let csv = b"a,b\n".to_vec();
        let staged = vec![
            stage_input(&input("data/form.csv", &csv), &|_: &str, _: u64| {
                Ok(csv.clone())
            })
            .unwrap(),
        ];
        let detail = detail(vec![input("data/form.csv", &csv)]);
        let copies = write_folder(&FolderSpec {
            folder: &folder,
            client: Client::Claude,
            detail: &detail,
            inputs: &staged,
            approved: &[(item.clone(), Scope::Private)],
            entry: json!({"entry_id": "e1"}),
        })
        .unwrap();
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&folder.join("output")), 0o700);
        assert_eq!(fs::read_dir(folder.join("output")).unwrap().count(), 0);
        assert_eq!(fs::read(folder.join("inputs/data/form.csv")).unwrap(), csv);
        let brief = fs::read_to_string(folder.join("BRIEF.md")).unwrap();
        assert!(brief.contains("Fix the form."));
        assert!(brief.contains("Accessibility: up to 20 points"));
        let pointer = fs::read_to_string(folder.join("CLAUDE.md")).unwrap();
        let copy = copies[0].copy.clone().unwrap();
        assert!(copy.starts_with("approved/private"));
        assert!(
            pointer.contains(&format!("@{}", copy.display())),
            "{pointer}"
        );
        assert_eq!(
            fs::read_to_string(folder.join(&copy)).unwrap(),
            "approved memory\n"
        );
        let entry: Value =
            serde_json::from_slice(&fs::read(folder.join(RECORD_FILE)).unwrap()).unwrap();
        assert_eq!(entry["entry_id"], "e1");
        assert_eq!(entry["approved"][0]["scope"], "private");
        assert_eq!(entry["inputs"][0]["sha256"], sha256_hex(&csv));

        // Edited after approval: the copy is refused.
        fs::write(&memory, "edited\n").unwrap();
        let error = write_folder(&FolderSpec {
            folder: &folder,
            client: Client::Claude,
            detail: &detail,
            inputs: &staged,
            approved: &[(item, Scope::Private)],
            entry: json!({}),
        })
        .unwrap_err();
        assert!(error.message().contains("changed after you approved it"));
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_symlink_in_the_folder_is_never_written_through() {
        let base = unique_dir("daycare-folder-link");
        let folder = base.join("c1");
        ensure_private_dir(&folder).unwrap();
        let outside = base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("inputs")).unwrap();
        let csv = b"x".to_vec();
        let staged =
            vec![stage_input(&input("a.csv", &csv), &|_: &str, _: u64| Ok(csv.clone())).unwrap()];
        let result = write_folder(&FolderSpec {
            folder: &folder,
            client: Client::Codex,
            detail: &detail(vec![]),
            inputs: &staged,
            approved: &[],
            entry: json!({}),
        });
        assert!(result.is_err());
        assert!(!outside.join("a.csv").exists());
        // And a loose folder is refused, not repaired.
        let loose = base.join("loose");
        fs::create_dir_all(&loose).unwrap();
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&loose)
            .unwrap_err()
            .message()
            .contains("755"));
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn meter_files_are_written_private_and_never_through_a_symlink() {
        let base = unique_dir("daycare-folder-meter");
        let folder = base.join("c1");
        ensure_private_dir(&folder).unwrap();
        let written = write_file_in(&folder, "meter/start.json", b"{}").unwrap();
        assert_eq!(written, folder.join("meter/start.json"));
        assert_eq!(mode(&folder.join("meter")), 0o700);
        assert_eq!(mode(&written), 0o600);
        remove_file_in(&folder, "meter/start.json").unwrap();
        remove_file_in(&folder, "meter/start.json").unwrap();
        assert!(!written.exists());
        // A meter/ link to somewhere else is refused, for writes and removals.
        fs::remove_dir(folder.join("meter")).unwrap();
        let outside = base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("end.json"), "theirs").unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("meter")).unwrap();
        assert!(write_file_in(&folder, "meter/start.json", b"{}").is_err());
        assert!(remove_file_in(&folder, "meter/end.json").is_err());
        assert!(!outside.join("start.json").exists());
        assert!(outside.join("end.json").exists());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_codex_pointer_lists_copies_instead_of_importing() {
        let item = Item {
            id: "codex:skill:/h/.agents/skills/notes#notes".into(),
            client: Client::Codex,
            kind: Kind::Skill,
            path: PathBuf::from("/h/.agents/skills/notes"),
            name: Some("notes".into()),
            bytes: 1,
            sha256: "a".repeat(64),
            origin: None,
        };
        let copies = vec![ApprovedCopy {
            item,
            scope: Scope::Public,
            copy: Some(PathBuf::from("approved/public/skills/notes")),
        }];
        let text = pointer_text(Client::Codex, &detail(vec![]), &copies);
        assert!(
            text.contains("read `approved/public/skills/notes/SKILL.md`"),
            "{text}"
        );
        assert!(!text.contains('@'), "{text}");
        assert!(text.contains("### Private\n\nNone."), "{text}");
    }

    #[test]
    fn the_challenge_record_is_read_from_either_shape() {
        let flat = json!({
            "id": "c1", "slug": "s", "title": "T", "state": "open", "brief_md": "B",
            "w_vote": 20, "w_goal": 60, "w_third": 20, "third_label": "Auditability",
            "inputs": [{"path": "a.csv", "bytes": 3, "sha256": "a".repeat(64), "url": "https://x/a"}],
        });
        let wrapped = json!({
            "challenge": {"id": "c1", "title": "T", "brief": "B", "weights": {"vote": 20, "goal": 60, "third": 20}},
            "inputs": [{"path": "a.csv", "bytes": 3, "sha256": "a".repeat(64), "signed_url": "https://x/a"}],
        });
        for value in [flat, wrapped] {
            let detail = ChallengeDetail::parse(&value).unwrap();
            assert_eq!(detail.weights, Some((20, 60, 20)));
            assert_eq!(detail.brief_md, "B");
            assert_eq!(detail.inputs[0].url, "https://x/a");
            assert!(!detail.public_json().to_string().contains("https://x/a"));
        }
        assert!(ChallengeDetail::parse(&json!({"id": "../etc"})).is_err());
    }
}
