//! `daycare-runner challenge submit` (plan task N14): send `output/` as the
//! owner's entry, after they have seen exactly what goes.
//!
//! In order, and nothing is sent before the typed yes:
//! 1. refuse under `--json`, without a terminal, and inside a challenge
//!    session (the agent must not be the one that submits);
//! 2. find the challenge folder (`challenge submit <id>`, or run it inside the
//!    folder) and read its record (`folder::RECORD_FILE`);
//! 3. read the challenge's upload limits from the platform;
//! 4. walk `output/`: links and anything that is not a regular file abort, as
//!    do hidden files and names the platform would refuse; the count, size and
//!    extension limits apply. Each file is read once, and those bytes are the
//!    ones listed, scanned and sent;
//! 5. the local secret scan (`scan`): any finding stops here;
//! 6. print `path bytes sha256` for every file, ask the declaration (how much
//!    the owner directed the agent, which tools it used), and ask for `yes`;
//! 7. record the meter readings `challenge start` left in `meter/` in the
//!    folder's record, then deliver (`upload`): POST, one PUT per file, confirm.
//!
//! A resubmission is a new version; only a confirmed one counts.

use crate::approval::scan::Roots;
use crate::approval::{reconcile, Approval, Prompt};
use crate::challenge::folder::{validate_challenge_id, RECORD_FILE};
use crate::challenge::scan::{self as secret_scan, CredentialFiles, FindingKind, SecretSet};
use crate::challenge::upload::{self, Delivered, Delivery};
use crate::config::Config;
use crate::identity::{device_token_account, token_account, Identities};
use crate::keychain::TokenStore;
use crate::out::Out;
use crate::paths::{sanitize_segment, write_atomic, Layout};
use crate::platform::{PlatformClient, SignedUploader};
use crate::self_update::sha256_hex;
use crate::session::Active;
use crate::{Error, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The folder that is uploaded, and nothing else.
pub const OUTPUT_FOLDER: &str = "output";
/// Limits used when the platform publishes none. They match the platform's
/// own caps (`uploads.ts`), which it enforces again whatever the runner does.
pub const DEFAULT_MAX_FILES: usize = 100;
pub const DEFAULT_MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 45 * 1024 * 1024;
/// The platform's path rules (`artifactPathProblem` in `uploads.ts`).
pub const MAX_PATH_LENGTH: usize = 200;
pub const MAX_PATH_DEPTH: usize = 8;
/// The declaration's bounds (`zDeclaration` in `entries.ts`).
pub const MAX_DIRECTION_CHARS: usize = 500;
pub const MAX_TOOLS: usize = 30;
pub const MAX_TOOL_CHARS: usize = 100;
/// Files an operating system drops into folders. Left out, and said so, as
/// the web page does.
const SYSTEM_FILES: [&str; 3] = [".ds_store", "thumbs.db", "desktop.ini"];
/// The folder's record and a meter reading are small; anything larger is not ours.
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_METER_BYTES: u64 = 64 * 1024;

/// The upload limits `challenge show` publishes under `limits`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    /// Lowercase extensions without the dot. `None`: any extension. The
    /// platform publishes `allowed_extensions: null` until a challenge sets it.
    pub allowed_extensions: Option<BTreeSet<String>>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            allowed_extensions: None,
        }
    }
}

impl Limits {
    /// Read from the challenge record (`limits` at the top or inside
    /// `challenge`). A missing value keeps its default; a published one can
    /// only make the runner stricter than the defaults, never looser.
    pub fn from_challenge(value: &Value) -> Limits {
        let limits = value
            .get("limits")
            .or_else(|| value.get("challenge").and_then(|c| c.get("limits")))
            .cloned()
            .unwrap_or(Value::Null);
        let number = |key: &str| limits.get(key).and_then(Value::as_u64);
        let defaults = Limits::default();
        let allowed_extensions = limits
            .get("allowed_extensions")
            .or_else(|| limits.get("extensions"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|ext| ext.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|ext| !ext.is_empty())
                    .collect()
            });
        Limits {
            max_files: number("max_files")
                .map(|n| (n as usize).min(defaults.max_files))
                .unwrap_or(defaults.max_files),
            max_file_bytes: number("max_file_bytes")
                .map(|n| n.min(defaults.max_file_bytes))
                .unwrap_or(defaults.max_file_bytes),
            max_total_bytes: number("max_total_bytes")
                .map(|n| n.min(defaults.max_total_bytes))
                .unwrap_or(defaults.max_total_bytes),
            allowed_extensions,
        }
    }
}

/// One file of the entry: its path inside `output/`, and the bytes that were
/// listed, scanned and will be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// What `output/` holds, as it will be sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Sorted by path, byte order, as the platform sorts them.
    pub files: Vec<OutputFile>,
    /// System files that were left out.
    pub left_out: Vec<String>,
}

impl Manifest {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes.len() as u64).sum()
    }

    /// `path bytes sha256`, one file per line: the listing the owner says yes to.
    pub fn listing(&self) -> String {
        self.files
            .iter()
            .map(|file| format!("  {} {} {}\n", file.path, file.bytes.len(), file.sha256))
            .collect()
    }
}

fn mib(bytes: u64) -> String {
    format!("{} MB", (bytes as f64 / (1024.0 * 1024.0)).round() as u64)
}

/// Why a path inside `output/` would be refused; `None` when it is fine. The
/// same rules as the platform's, so the owner hears about them here.
pub fn path_problem(path: &str) -> Option<String> {
    if path.len() > MAX_PATH_LENGTH {
        return Some(format!(
            "its path is longer than {MAX_PATH_LENGTH} characters"
        ));
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() > MAX_PATH_DEPTH {
        return Some(format!("it is more than {MAX_PATH_DEPTH} folders deep"));
    }
    for segment in segments {
        let mut chars = segment.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if !first_ok
            || !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Some(
                "names may use only letters, digits, dots, dashes and underscores, and start \
                 with a letter, digit or underscore"
                    .into(),
            );
        }
    }
    None
}

/// Walk `output/`, refusing anything the upload must not carry, and read
/// every file once.
pub fn collect_output(output_dir: &Path, limits: &Limits) -> Result<Manifest> {
    let metadata = fs::symlink_metadata(output_dir).map_err(|_| {
        Error::new(format!(
            "{} is missing; put the files you want to submit there",
            output_dir.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::new(format!(
            "{} must be a real folder, not a link; nothing was sent",
            output_dir.display()
        )));
    }

    let mut found: Vec<(String, PathBuf, u64)> = Vec::new();
    let mut left_out = Vec::new();
    let mut stack = vec![(output_dir.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let mut entries: Vec<_> = fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                Error::new(format!(
                    "{} has a name that is not valid text; rename it. Nothing was sent",
                    entry.path().display()
                ))
            })?;
            let relative = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let shown = format!("{OUTPUT_FOLDER}/{relative}");
            let metadata = fs::symlink_metadata(entry.path())?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(Error::new(format!(
                    "{shown} is a link. Links are never uploaded; replace it with the real file \
                     or remove it. Nothing was sent"
                )));
            }
            if file_type.is_file() && SYSTEM_FILES.contains(&name.to_ascii_lowercase().as_str()) {
                left_out.push(relative);
                continue;
            }
            if name.starts_with('.') {
                return Err(Error::new(format!(
                    "{shown} is hidden. Hidden files and folders are not uploaded; remove it. \
                     Nothing was sent"
                )));
            }
            if file_type.is_dir() {
                stack.push((entry.path(), relative));
                continue;
            }
            if !file_type.is_file() {
                return Err(Error::new(format!(
                    "{shown} is not a regular file (a pipe, socket or device); remove it. \
                     Nothing was sent"
                )));
            }
            if let Some(problem) = path_problem(&relative) {
                return Err(Error::new(format!(
                    "{shown} cannot be uploaded: {problem}. Rename it. Nothing was sent"
                )));
            }
            if let Some(allowed) = &limits.allowed_extensions {
                let extension = name
                    .rsplit_once('.')
                    .map(|(_, ext)| ext.to_ascii_lowercase())
                    .unwrap_or_default();
                if !allowed.contains(&extension) {
                    let list = allowed.iter().cloned().collect::<Vec<_>>().join(", ");
                    return Err(Error::new(format!(
                        "{shown} is not a file type this challenge takes ({list}). Nothing was \
                         sent"
                    )));
                }
            }
            if metadata.len() > limits.max_file_bytes {
                return Err(Error::new(format!(
                    "{shown} is over the {} limit for one file. Nothing was sent",
                    mib(limits.max_file_bytes)
                )));
            }
            found.push((relative, entry.path(), metadata.len()));
        }
    }

    if found.is_empty() {
        return Err(Error::new(format!(
            "{} is empty. Put the files you want to submit there first",
            output_dir.display()
        )));
    }
    if found.len() > limits.max_files {
        return Err(Error::new(format!(
            "{OUTPUT_FOLDER}/ has {} files; an entry can have at most {}. Nothing was sent",
            found.len(),
            limits.max_files
        )));
    }
    let declared: u64 = found.iter().map(|(_, _, bytes)| bytes).sum();
    if declared > limits.max_total_bytes {
        return Err(Error::new(format!(
            "the files in {OUTPUT_FOLDER}/ add up to more than the {} limit for an entry. \
             Nothing was sent",
            mib(limits.max_total_bytes)
        )));
    }
    // Two names that differ only in case are one file on a Mac, and the
    // platform refuses them.
    let mut lower = BTreeSet::new();
    let mut folders = BTreeSet::new();
    for (relative, _, _) in &found {
        let key = relative.to_ascii_lowercase();
        if !lower.insert(key.clone()) {
            return Err(Error::new(format!(
                "{OUTPUT_FOLDER}/{relative} differs from another file only in case; rename one. \
                 Nothing was sent"
            )));
        }
        let parts: Vec<&str> = key.split('/').collect();
        for depth in 1..parts.len() {
            folders.insert(parts[..depth].join("/"));
        }
    }
    if let Some((relative, _, _)) = found
        .iter()
        .find(|(relative, _, _)| folders.contains(&relative.to_ascii_lowercase()))
    {
        return Err(Error::new(format!(
            "{OUTPUT_FOLDER}/{relative} has the same name as a folder; rename one. Nothing was sent"
        )));
    }

    let mut files = Vec::with_capacity(found.len());
    let mut total = 0u64;
    for (relative, path, _) in found {
        let bytes = read_capped(&path, limits.max_file_bytes)?.ok_or_else(|| {
            Error::new(format!(
                "{OUTPUT_FOLDER}/{relative} grew past the {} limit while it was being read. \
                 Nothing was sent",
                mib(limits.max_file_bytes)
            ))
        })?;
        total += bytes.len() as u64;
        if total > limits.max_total_bytes {
            return Err(Error::new(format!(
                "the files in {OUTPUT_FOLDER}/ grew past the {} limit while they were being \
                 read. Nothing was sent",
                mib(limits.max_total_bytes)
            )));
        }
        let sha256 = sha256_hex(&bytes);
        files.push(OutputFile {
            path: relative,
            bytes,
            sha256,
        });
    }
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    left_out.sort();
    Ok(Manifest { files, left_out })
}

/// Read at most `max` bytes, opened without following a link. `None` when the
/// file is larger.
fn read_capped(path: &Path, max: u64) -> Result<Option<Vec<u8>>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| Error::new(format!("could not read {}: {error}", path.display())))?;
    if !file.metadata()?.is_file() {
        return Err(Error::new(format!(
            "{} is not a regular file; nothing was sent",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= max).then_some(bytes))
}

/// The challenge folder `challenge start` made, and its record (`RECORD_FILE`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChallengeFolder {
    pub folder: PathBuf,
    pub challenge_id: String,
    pub entry: Value,
}

fn read_entry(folder: &Path) -> Result<Value> {
    let path = folder.join(RECORD_FILE);
    let metadata = fs::symlink_metadata(&path).map_err(|_| {
        Error::new(format!(
            "{} has no {RECORD_FILE}, so it is not a challenge folder. Run `daycare-runner \
             challenge start <id>` first",
            folder.display()
        ))
    })?;
    if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES {
        return Err(Error::new(format!(
            "{} is not a regular file; refusing it",
            path.display()
        )));
    }
    let value: Value = serde_json::from_slice(&fs::read(&path)?)
        .map_err(|_| Error::new(format!("{} is not valid JSON", path.display())))?;
    if !value.is_object() {
        return Err(Error::new(format!("{} is not a record", path.display())));
    }
    Ok(value)
}

fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

impl ChallengeFolder {
    /// `challenge submit <id>`: the folder named for the id, or the one whose
    /// record has that id or slug. Without an id: the challenge folder
    /// the current directory is in.
    pub fn locate(challenge: Option<&str>, root: &Path, cwd: &Path) -> Result<ChallengeFolder> {
        let folder = match challenge {
            Some(id) => {
                validate_challenge_id(id)?;
                let direct = root.join(sanitize_segment(id));
                if real_dir(&direct) {
                    direct
                } else {
                    Self::find_by_record(root, id)?
                }
            }
            None => Self::enclosing(root, cwd)?,
        };
        let metadata = fs::symlink_metadata(&folder)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::new(format!(
                "{} is not a real folder; refusing it",
                folder.display()
            )));
        }
        let entry = read_entry(&folder)?;
        let challenge_id = entry
            .get("challenge_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::new(format!("{RECORD_FILE} has no challenge_id")))?
            .to_string();
        validate_challenge_id(&challenge_id)?;
        Ok(ChallengeFolder {
            folder,
            challenge_id,
            entry,
        })
    }

    fn find_by_record(root: &Path, id: &str) -> Result<PathBuf> {
        let not_found = || {
            Error::new(format!(
                "no challenge folder for {id:?} under {}. Run `daycare-runner challenge start \
                 {id}` first",
                root.display()
            ))
        };
        let entries = fs::read_dir(root).map_err(|_| not_found())?;
        let mut matches = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !real_dir(&path) {
                continue;
            }
            let Ok(record) = read_entry(&path) else {
                continue;
            };
            let named = |key: &str| record.get(key).and_then(Value::as_str) == Some(id);
            if named("challenge_id") || named("challenge_slug") {
                matches.push(path);
            }
        }
        matches.sort();
        matches.into_iter().next().ok_or_else(not_found)
    }

    fn enclosing(root: &Path, cwd: &Path) -> Result<PathBuf> {
        let outside = || {
            Error::new(format!(
                "this is not a challenge folder. Run it inside one (under {}), or name the \
                 challenge: `daycare-runner challenge submit <id>`",
                root.display()
            ))
        };
        let root = fs::canonicalize(root).map_err(|_| outside())?;
        let cwd = fs::canonicalize(cwd).map_err(|_| outside())?;
        let relative = cwd.strip_prefix(&root).map_err(|_| outside())?;
        let first = relative.components().next().ok_or_else(outside)?;
        Ok(root.join(first.as_os_str()))
    }

    pub fn output_dir(&self) -> PathBuf {
        self.folder.join(OUTPUT_FOLDER)
    }

    fn field(&self, key: &str) -> Option<&str> {
        self.entry
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    }

    /// The identity that started this folder.
    pub fn actor_id(&self) -> Option<&str> {
        self.field("actor_id")
    }

    pub fn entry_id(&self) -> Option<&str> {
        self.field("entry_id")
    }

    pub fn title(&self) -> &str {
        self.field("title").unwrap_or(&self.challenge_id)
    }

    /// `meter/start.json` and `meter/end.json`, when `challenge start` wrote
    /// them. A file that is there but unreadable is recorded as such, not
    /// left out.
    pub fn meter_readings(&self) -> Option<Value> {
        let dir = self.folder.join("meter");
        if !real_dir(&dir) {
            return None;
        }
        let mut readings = serde_json::Map::new();
        for name in ["start", "end"] {
            let path = dir.join(format!("{name}.json"));
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            let reading = if !metadata.is_file() || metadata.len() > MAX_METER_BYTES {
                json!({ "unreadable": "not a small regular file" })
            } else {
                fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .unwrap_or_else(|| json!({ "unreadable": "not valid JSON" }))
            };
            readings.insert(name.to_string(), reading);
        }
        (!readings.is_empty()).then_some(Value::Object(readings))
    }

    /// Rewrite the record with `edit` applied, never through a link.
    pub fn update_entry(&mut self, edit: impl FnOnce(&mut Value)) -> Result<()> {
        let path = self.folder.join(RECORD_FILE);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
            return Err(Error::new(format!(
                "{} is not a regular file; refusing to write it",
                path.display()
            )));
        }
        edit(&mut self.entry);
        let mut text = serde_json::to_string_pretty(&self.entry)?;
        text.push('\n');
        write_atomic(&path, text.as_bytes(), 0o600)
    }
}

/// The owner's account of how the work was done. Self-reported, and said so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// `none`, `light` or `heavy`, the web page's three answers.
    pub direction: String,
    pub tools: Vec<String>,
}

/// The web page's answers, in the same words.
pub const DIRECTIONS: [(&str, &str); 3] = [
    ("none", "Not at all. It worked from the brief on its own."),
    (
        "light",
        "A little. A few nudges or answers to its questions.",
    ),
    (
        "heavy",
        "A lot. You steered it closely or did parts yourself.",
    ),
];

/// "Claude Code, a browser,  " → ["Claude Code", "a browser"], as the web
/// page parses it.
pub fn parse_tools(text: &str) -> Result<Vec<String>> {
    let mut tools: Vec<String> = Vec::new();
    for tool in text
        .split([',', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        if tool.chars().count() > MAX_TOOL_CHARS {
            return Err(Error::new(format!(
                "each tool can be at most {MAX_TOOL_CHARS} characters"
            )));
        }
        if !tools.iter().any(|known| known == tool) {
            tools.push(tool.to_string());
        }
    }
    if tools.len() > MAX_TOOLS {
        return Err(Error::new(format!("name at most {MAX_TOOLS} tools")));
    }
    Ok(tools)
}

/// Three tries per question; leaving (end of input) stops at once.
const TRIES: usize = 3;

fn left(answer: Option<String>) -> Result<String> {
    answer.ok_or_else(|| Error::new("you left before answering. Nothing was sent"))
}

/// Ask the two declaration questions.
pub fn ask_declaration(prompt: &mut dyn Prompt, screen: &mut dyn Write) -> Result<Declaration> {
    writeln!(screen)?;
    writeln!(
        screen,
        "How much did you direct your agent? This is self-reported and shown with your entry."
    )?;
    for (value, hint) in DIRECTIONS {
        writeln!(screen, "  {value:<6} {hint}")?;
    }
    screen.flush()?;
    let mut direction = None;
    for _ in 0..TRIES {
        let answer = left(prompt.ask("Type none, light or heavy: "))?;
        let answer = answer.trim().to_ascii_lowercase();
        if DIRECTIONS.iter().any(|(value, _)| *value == answer) {
            direction = Some(answer);
            break;
        }
        writeln!(screen, "Answer none, light or heavy.")?;
        screen.flush()?;
    }
    let direction = direction.ok_or_else(|| {
        Error::new("no answer to how much you directed your agent. Nothing was sent")
    })?;
    debug_assert!(direction.len() <= MAX_DIRECTION_CHARS);

    for _ in 0..TRIES {
        let answer =
            left(prompt.ask(
                "Which tools did it use? Separate them with commas, or press Enter for none: ",
            ))?;
        match parse_tools(&answer) {
            Ok(tools) => return Ok(Declaration { direction, tools }),
            Err(error) => {
                writeln!(screen, "{error}.")?;
                screen.flush()?;
            }
        }
    }
    Err(Error::new("no usable answer about tools. Nothing was sent"))
}

/// Where `gather_secrets` looks. The CLI discovers these on the real machine;
/// tests point every one at a scratch folder.
pub struct SecretSources<'a> {
    pub layout: &'a Layout,
    pub store: &'a dyn TokenStore,
    /// The identity token this submit sends.
    pub identity_token: &'a str,
    pub roots: &'a Roots,
    pub credentials: &'a CredentialFiles,
}

/// Every secret this machine holds that the scan matches exactly, plus the
/// home path and the memory the approval keeps out of public work. Fails
/// closed: a credential store that cannot be read stops the submit, because
/// the scan could not check for what it holds.
pub fn gather_secrets(sources: SecretSources<'_>) -> Result<SecretSet> {
    let SecretSources {
        layout,
        store,
        identity_token,
        roots,
        credentials,
    } = sources;
    let mut set = SecretSet::new();
    let unreadable = |what: &str, error: Error| {
        Error::new(format!(
            "could not read {what} to check output/ for it ({error}). Nothing was sent"
        ))
    };
    if let Ok(config) = Config::load(layout) {
        if let Some(token) = store
            .read(&device_token_account(&config.device_id))
            .map_err(|error| unreadable("this machine's device token", error))?
        {
            set.add_exact(&token, FindingKind::DeviceToken);
        }
    }
    for identity in Identities::load(layout)?.all() {
        if let Some(token) = store
            .read(&token_account(&identity.identity_id))
            .map_err(|error| unreadable("an identity token", error))?
        {
            set.add_exact(&token, FindingKind::IdentityToken);
        }
    }
    // Last: a device token that is also the acting credential (a first
    // pairing) is reported as the device token.
    set.add_exact(identity_token, FindingKind::IdentityToken);

    set.set_home(&roots.home);
    credentials.add_to(&mut set);
    let inventory = crate::approval::scan::scan(roots);
    let approval = Approval::load(layout)?;
    let reconciled = reconcile(approval.as_ref(), &inventory);
    secret_scan::add_memory(
        &mut set,
        &inventory,
        &reconciled,
        &roots.claude_json,
        &roots.home,
    );
    secret_scan::add_opencode_provider_keys(&mut set, roots);
    secret_scan::add_provider_env_keys(
        &mut set,
        std::env::vars_os().filter_map(|(name, value)| {
            Some((name.into_string().ok()?, value.into_string().ok()?))
        }),
    );
    Ok(set)
}

fn is_yes(answer: &Option<String>) -> bool {
    answer
        .as_deref()
        .is_some_and(|text| text.trim().eq_ignore_ascii_case("yes"))
}

/// Everything one submit needs. The CLI builds it from the terminal; tests
/// build it with a scripted prompt and a mock platform.
pub struct SubmitRequest<'a> {
    pub platform: &'a PlatformClient,
    pub uploader: &'a SignedUploader,
    pub token: &'a str,
    pub folder: ChallengeFolder,
    pub secrets: &'a SecretSet,
    pub submitted_at: &'a str,
}

/// What a delivered submit returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    pub delivered: Delivered,
    pub files: usize,
}

/// Steps 3–7. The caller has already refused `--json`, a missing terminal and
/// the session marker.
pub fn run(
    request: SubmitRequest<'_>,
    prompt: &mut dyn Prompt,
    screen: &mut dyn Write,
) -> Result<Submitted> {
    let SubmitRequest {
        platform,
        uploader,
        token,
        mut folder,
        secrets,
        submitted_at,
    } = request;

    let challenge = platform.get_challenge(token, &folder.challenge_id)?;
    let record = challenge
        .get("challenge")
        .filter(|inner| inner.is_object())
        .unwrap_or(&challenge);
    if let Some(state) = record
        .get("state")
        .or_else(|| record.get("phase"))
        .and_then(Value::as_str)
    {
        if state != "open" {
            return Err(Error::new(format!(
                "Not delivered: {} is {state}, not taking entries. Nothing was sent",
                folder.title()
            )));
        }
    }
    let limits = Limits::from_challenge(&challenge);

    let manifest = collect_output(&folder.output_dir(), &limits)?;
    let findings = secrets.scan(
        manifest
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.bytes.as_slice())),
    );
    if !findings.is_empty() {
        return Err(Error::new(secret_scan::refusal(&findings)));
    }

    writeln!(
        screen,
        "Submitting {} from {}.",
        folder.title(),
        folder.output_dir().display()
    )?;
    writeln!(
        screen,
        "These {} file(s), {} bytes in all, will be sent. Each line is path, bytes, sha256:",
        manifest.files.len(),
        manifest.total_bytes()
    )?;
    write!(screen, "{}", manifest.listing())?;
    for path in &manifest.left_out {
        writeln!(
            screen,
            "Left out: {OUTPUT_FOLDER}/{path} (a file your system made, not your work)."
        )?;
    }
    writeln!(screen, "The local secret scan found nothing.")?;
    screen.flush()?;

    let declaration = ask_declaration(prompt, screen)?;

    // Said before the yes, because they go with the entry: the share of the
    // weekly allowance used before and after the session, nothing else.
    let readings = folder.meter_readings();
    let sent_meter = readings.as_ref().and_then(super::meter::for_platform);
    if let Some(meter) = &sent_meter {
        writeln!(
            screen,
            "Your usage readings go with your entry, self-reported: {}.",
            super::meter::describe(meter)
        )?;
    }

    writeln!(screen)?;
    screen.flush()?;
    let answer = prompt.ask(&format!(
        "Type yes to send these {} file(s) as your entry: ",
        manifest.files.len()
    ));
    if !is_yes(&answer) {
        return Err(Error::new(
            "you did not type yes, so nothing was sent. Run `daycare-runner challenge submit` \
             again when you are ready",
        ));
    }

    // The readings go into the record before anything leaves the machine, so
    // they are on record whatever the network does.
    if let Some(meter) = readings {
        folder.update_entry(|entry| entry["meter"] = meter)?;
        writeln!(
            screen,
            "Recorded the meter readings from meter/ in {RECORD_FILE}."
        )?;
    }

    let challenge_id = folder.challenge_id.clone();
    let expected_entry_id = folder.entry_id().map(str::to_string);
    let mut progress = |line: &str| {
        let _ = writeln!(screen, "{line}");
        let _ = screen.flush();
    };
    let delivered = upload::deliver(
        Delivery {
            platform,
            uploader,
            token,
            challenge_id: &challenge_id,
            expected_entry_id: expected_entry_id.as_deref(),
            files: &manifest.files,
            declaration: &declaration,
            meter: sent_meter.as_ref(),
        },
        &mut progress,
    )?;

    folder.update_entry(|entry| {
        let record = json!({
            "submission_id": delivered.submission_id,
            "version": delivered.version,
            "files": manifest.files.len(),
            "manifest_sha256": delivered.manifest_sha256,
            "direction": declaration.direction,
            "tools": declaration.tools,
            "confirmed_at": submitted_at,
        });
        match entry.get_mut("submissions").and_then(Value::as_array_mut) {
            Some(list) => list.push(record),
            None => entry["submissions"] = Value::Array(vec![record]),
        }
    })?;
    writeln!(
        screen,
        "Delivered: version {} of your entry is confirmed ({} file(s), manifest sha256 {}).",
        delivered.version,
        manifest.files.len(),
        delivered.manifest_sha256
    )?;
    writeln!(
        screen,
        "You can submit again until entries close; each time makes a new version, and the \
         newest confirmed one counts."
    )?;
    screen.flush()?;
    Ok(Submitted {
        files: manifest.files.len(),
        delivered,
    })
}

/// The CLI's `challenge submit`, after `main` has chosen the identity.
pub fn submit(
    layout: &Layout,
    store: &dyn TokenStore,
    active: &Active,
    folder: ChallengeFolder,
    challenge_root: &Path,
    submitted_at: &str,
    out: Out,
) -> Result<()> {
    crate::approval::require_terminal(out.json, "challenge submit")?;
    let roots = Roots::discover(layout.root(), challenge_root)?;
    let credentials = CredentialFiles::discover(&roots.home, &layout.fallback_token_file());
    let secrets = gather_secrets(SecretSources {
        layout,
        store,
        identity_token: active.token(),
        roots: &roots,
        credentials: &credentials,
    })?;
    let platform = PlatformClient::new(&active.platform_url);
    let uploader = SignedUploader::new();
    let mut stdout = std::io::stdout();
    run(
        SubmitRequest {
            platform: &platform,
            uploader: &uploader,
            token: active.token(),
            folder,
            secrets: &secrets,
            submitted_at,
        },
        &mut crate::approval::TerminalPrompt,
        &mut stdout,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::unique_dir;

    fn output_with(files: &[(&str, &str)]) -> PathBuf {
        let dir = unique_dir("submit-output");
        let output = dir.join("output");
        fs::create_dir_all(&output).unwrap();
        for (path, body) in files {
            let target = output.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, body).unwrap();
        }
        output
    }

    struct Scripted(Vec<Option<&'static str>>);

    impl Prompt for Scripted {
        fn ask(&mut self, _question: &str) -> Option<String> {
            if self.0.is_empty() {
                return None;
            }
            self.0.remove(0).map(str::to_string)
        }
    }

    #[test]
    fn the_listing_is_sorted_and_carries_size_and_sha256() {
        let output = output_with(&[("z.md", "zz"), ("a/b.csv", "x,y\n"), ("entry.html", "<p>")]);
        let manifest = collect_output(&output, &Limits::default()).unwrap();
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a/b.csv", "entry.html", "z.md"]);
        assert_eq!(
            manifest.listing().lines().next().unwrap(),
            format!("  a/b.csv 4 {}", sha256_hex(b"x,y\n"))
        );
        assert_eq!(manifest.total_bytes(), 9);
    }

    #[test]
    fn a_symlink_anywhere_in_output_aborts() {
        let output = output_with(&[("entry.html", "<p>")]);
        std::os::unix::fs::symlink("/etc/hosts", output.join("hosts")).unwrap();
        let error = collect_output(&output, &Limits::default()).unwrap_err();
        assert!(error.message().contains("is a link"), "{error}");

        let nested = output_with(&[("dir/entry.html", "<p>")]);
        std::os::unix::fs::symlink("/etc", nested.join("dir/etc")).unwrap();
        assert!(collect_output(&nested, &Limits::default()).is_err());

        // output/ itself as a link.
        let dir = unique_dir("submit-linked-output");
        fs::create_dir_all(&dir).unwrap();
        let real = output_with(&[("a.md", "a")]);
        std::os::unix::fs::symlink(&real, dir.join("output")).unwrap();
        assert!(collect_output(&dir.join("output"), &Limits::default()).is_err());
    }

    #[test]
    fn a_pipe_is_not_a_regular_file() {
        let output = output_with(&[("a.md", "a")]);
        let fifo = output.join("pipe");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let error = collect_output(&output, &Limits::default()).unwrap_err();
        assert!(error.message().contains("not a regular file"), "{error}");
    }

    #[test]
    fn hidden_files_are_refused_and_system_files_left_out() {
        let hidden = output_with(&[("entry.html", "<p>"), (".env", "KEY=1")]);
        let error = collect_output(&hidden, &Limits::default()).unwrap_err();
        assert!(error.message().contains("hidden"), "{error}");
        let hidden_dir = output_with(&[("entry.html", "<p>"), (".git/config", "x")]);
        assert!(collect_output(&hidden_dir, &Limits::default()).is_err());

        let system = output_with(&[("entry.html", "<p>"), (".DS_Store", "bud1")]);
        let manifest = collect_output(&system, &Limits::default()).unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.left_out, [".DS_Store"]);
    }

    #[test]
    fn names_follow_the_platforms_rules() {
        assert_eq!(path_problem("entry.html"), None);
        assert_eq!(path_problem("data/out_1.csv"), None);
        for bad in [
            "my file.html",
            "-flag.md",
            "naïve.md",
            "a/b/c/d/e/f/g/h/i.md",
        ] {
            assert!(path_problem(bad).is_some(), "{bad}");
        }
        assert!(path_problem(&"a".repeat(201)).is_some());
        let output = output_with(&[("my file.html", "<p>")]);
        assert!(collect_output(&output, &Limits::default()).is_err());
    }

    #[test]
    fn count_size_and_extension_limits_apply() {
        let output = output_with(&[("a.md", "aaaa"), ("b.md", "bb"), ("c.csv", "c")]);
        let mut limits = Limits {
            max_files: 2,
            ..Limits::default()
        };
        assert!(collect_output(&output, &limits)
            .unwrap_err()
            .message()
            .contains("at most 2"));
        limits = Limits {
            max_file_bytes: 3,
            ..Limits::default()
        };
        assert!(collect_output(&output, &limits)
            .unwrap_err()
            .message()
            .contains("limit for one file"));
        limits = Limits {
            max_total_bytes: 6,
            ..Limits::default()
        };
        assert!(collect_output(&output, &limits)
            .unwrap_err()
            .message()
            .contains("add up to"));
        limits = Limits {
            allowed_extensions: Some(["md".to_string()].into()),
            ..Limits::default()
        };
        assert!(collect_output(&output, &limits)
            .unwrap_err()
            .message()
            .contains("c.csv"));
    }

    #[test]
    fn published_limits_are_read_and_can_only_tighten() {
        let limits = Limits::from_challenge(&json!({
            "limits": {"max_files": 5, "max_file_bytes": 10, "max_total_bytes": 999_999_999_999u64,
                       "allowed_extensions": [".HTML", "md"]}
        }));
        assert_eq!(limits.max_files, 5);
        assert_eq!(limits.max_file_bytes, 10);
        assert_eq!(limits.max_total_bytes, DEFAULT_MAX_TOTAL_BYTES);
        assert_eq!(
            limits.allowed_extensions,
            Some(["html".to_string(), "md".to_string()].into())
        );
        assert_eq!(
            Limits::from_challenge(&json!({"limits": {"allowed_extensions": null}})),
            Limits::default()
        );
    }

    #[test]
    fn an_empty_output_folder_is_refused() {
        let output = output_with(&[]);
        assert!(collect_output(&output, &Limits::default())
            .unwrap_err()
            .message()
            .contains("empty"));
    }

    #[test]
    fn the_declaration_takes_the_web_pages_answers() {
        let mut screen = Vec::new();
        let declaration = ask_declaration(
            &mut Scripted(vec![
                Some("maybe\n"),
                Some(" Light \n"),
                Some("Claude Code, a browser,, Claude Code\n"),
            ]),
            &mut screen,
        )
        .unwrap();
        assert_eq!(
            declaration,
            Declaration {
                direction: "light".into(),
                tools: vec!["Claude Code".into(), "a browser".into()],
            }
        );
        let none =
            ask_declaration(&mut Scripted(vec![Some("none"), Some("\n")]), &mut screen).unwrap();
        assert!(none.tools.is_empty());
        assert!(ask_declaration(&mut Scripted(vec![None]), &mut screen).is_err());
        assert!(ask_declaration(
            &mut Scripted(vec![Some("x"), Some("y"), Some("z")]),
            &mut screen
        )
        .is_err());
    }

    #[test]
    fn the_folder_is_found_by_id_by_slug_or_from_inside_it() {
        let root = unique_dir("submit-root");
        let folder = root.join("c-1");
        fs::create_dir_all(folder.join("output/sub")).unwrap();
        fs::write(
            folder.join(RECORD_FILE),
            r#"{"challenge_id":"c-1","challenge_slug":"signup-rescue","entry_id":"entry-1","actor_id":"actor-1"}"#,
        )
        .unwrap();
        let by_id = ChallengeFolder::locate(Some("c-1"), &root, Path::new("/")).unwrap();
        assert_eq!(by_id.entry_id(), Some("entry-1"));
        assert_eq!(by_id.actor_id(), Some("actor-1"));
        let by_slug =
            ChallengeFolder::locate(Some("signup-rescue"), &root, Path::new("/")).unwrap();
        assert_eq!(by_slug.challenge_id, "c-1");
        let inside = ChallengeFolder::locate(None, &root, &folder.join("output/sub")).unwrap();
        assert_eq!(inside.challenge_id, "c-1");
        assert!(ChallengeFolder::locate(None, &root, Path::new("/tmp")).is_err());
        assert!(ChallengeFolder::locate(Some("c-2"), &root, Path::new("/")).is_err());
        assert!(ChallengeFolder::locate(Some("../c-1"), &root, Path::new("/")).is_err());
    }

    #[test]
    fn meter_readings_are_read_when_present_and_recorded_in_entry_json() {
        let root = unique_dir("submit-meter");
        let folder = root.join("c-1");
        fs::create_dir_all(folder.join("meter")).unwrap();
        fs::write(folder.join(RECORD_FILE), r#"{"challenge_id":"c-1"}"#).unwrap();
        let mut located = ChallengeFolder::locate(Some("c-1"), &root, Path::new("/")).unwrap();
        assert_eq!(located.meter_readings(), None);
        fs::write(folder.join("meter/start.json"), r#"{"used_percentage":12}"#).unwrap();
        fs::write(folder.join("meter/end.json"), "not json").unwrap();
        let meter = located.meter_readings().unwrap();
        assert_eq!(meter["start"]["used_percentage"], 12);
        assert_eq!(meter["end"]["unreadable"], "not valid JSON");
        located
            .update_entry(|entry| entry["meter"] = meter)
            .unwrap();
        let written: Value =
            serde_json::from_slice(&fs::read(folder.join(RECORD_FILE)).unwrap()).unwrap();
        assert_eq!(written["meter"]["start"]["used_percentage"], 12);
        assert_eq!(written["challenge_id"], "c-1");
    }
}
