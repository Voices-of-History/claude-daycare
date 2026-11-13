//! The local secret scan `challenge submit` runs before anything is sent
//! (plan task N15).
//!
//! Three passes over every file in `output/`, each file's name included:
//! 1. exact matches against the secrets this machine actually holds: every
//!    Daycare identity token and the device token, every value in Daycare's
//!    own `tokens.json`, and the values in the Codex, OpenCode and Claude Code
//!    login files;
//!    Also the provider keys this machine holds for OpenCode: each
//!    provider's `options.apiKey` and header values in the OpenCode config
//!    files, and the value of every model provider's API-key variable set in
//!    the environment;
//! 2. patterns for well-known credential shapes (`sk-ant-`, `sk-`, Google
//!    `AIza`, Groq `gsk_`, xAI `xai-`, GitHub tokens, AWS access key ids, PEM
//!    blocks) and the owner's home path, which names them and their machine;
//! 3. text from the owner's own memory that their approval does not allow in
//!    public work: items marked private, and every item not approved at all
//!    ("don't use"). Lines of instruction files, skill files and MCP settings
//!    values are matched verbatim, and so are OpenCode plugin options and the
//!    text an OpenCode stanza pulls in with `{file:…}` or `{env:…}`. A skill's code is not: its fenced code
//!    examples and its script files are mostly the skill author's boilerplate
//!    (a viewport `<meta>`, a labelled email field), which an entry writes on
//!    its own, so they would refuse honest work. A skill's prose still counts,
//!    and instruction files and auto memory are matched whole, code included.
//!
//! A finding names the file and the kind of finding, never the matched text,
//! so the refusal itself cannot leak what it found. Any finding blocks the
//! upload; there is no override, because the fix is to edit `output/`.
//!
//! What it cannot see: text inside compressed files (`.xlsx`, `.zip`, `.docx`)
//! is matched only as stored bytes, and a secret that was re-encoded (base64,
//! split across lines, HTML-escaped) does not match. The team's own scan runs
//! again on the platform before any entry is shown. `looks_like_a_secret` in
//! `workspace.rs` is a guard for one value, not this scanner.

use crate::approval::scan::{codex_mcp_stanzas, Client, Inventory, Item, Kind};
use crate::approval::{Reconciled, Scope};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

/// The shortest exact secret worth matching. Shorter values (an auth mode, a
/// provider name) would match ordinary words.
pub const MIN_EXACT_BYTES: usize = 12;
/// The shortest memory line worth matching. A line this long that appears
/// verbatim in the output was almost certainly copied.
pub const MIN_MEMORY_LINE_BYTES: usize = 24;
/// The largest memory file read for lines. Larger ones are skipped (and a
/// skill directory stops after this many bytes in total).
const MAX_MEMORY_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// The largest login or token file read for values.
const MAX_CREDENTIAL_FILE_BYTES: u64 = 1024 * 1024;

/// What a finding is, in words an owner can act on. Never the matched text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A Daycare identity token.
    IdentityToken,
    /// This machine's Daycare device token.
    DeviceToken,
    /// A value from Daycare's own `tokens.json`.
    DaycareTokenFile,
    /// A value from Codex's `auth.json`.
    CodexLogin,
    /// A value from OpenCode's `auth.json`.
    OpencodeLogin,
    /// A value from Claude Code's `.credentials.json`.
    ClaudeLogin,
    /// `sk-ant-…`.
    AnthropicKey,
    /// `sk-…` (OpenAI and others).
    ApiKey,
    /// `AIza…`.
    GoogleApiKey,
    /// `gsk_…`.
    GroqKey,
    /// `xai-…`.
    XaiKey,
    /// A provider's `apiKey` or header value in an OpenCode config file.
    OpencodeProviderKey,
    /// The value of a model provider's API-key variable in the environment.
    ProviderKeyEnv,
    /// `ghp_…`, `github_pat_…` and the other GitHub token prefixes.
    GithubToken,
    /// `AKIA…` or `ASIA…`.
    AwsAccessKey,
    /// `-----BEGIN …-----`.
    PemBlock,
    /// The owner's home folder path.
    HomePath,
    /// Text from memory approved for private use only.
    PrivateMemory,
    /// Text from memory that was never approved (don't use).
    DontUseMemory,
}

impl FindingKind {
    pub fn describe(self) -> &'static str {
        match self {
            FindingKind::IdentityToken => "a Daycare identity token",
            FindingKind::DeviceToken => "this machine's Daycare device token",
            FindingKind::DaycareTokenFile => "a credential from Daycare's tokens.json",
            FindingKind::CodexLogin => "a value from your Codex login (auth.json)",
            FindingKind::OpencodeLogin => "a value from your OpenCode login (auth.json)",
            FindingKind::ClaudeLogin => "a value from your Claude Code login (.credentials.json)",
            FindingKind::AnthropicKey => "something shaped like an Anthropic API key (sk-ant-)",
            FindingKind::ApiKey => "something shaped like an API key (sk-)",
            FindingKind::GoogleApiKey => "something shaped like a Google API key (AIza)",
            FindingKind::GroqKey => "something shaped like a Groq API key (gsk_)",
            FindingKind::XaiKey => "something shaped like an xAI API key (xai-)",
            FindingKind::OpencodeProviderKey => "a provider API key from your OpenCode settings",
            FindingKind::ProviderKeyEnv => "a model provider API key from your environment",
            FindingKind::GithubToken => "something shaped like a GitHub token",
            FindingKind::AwsAccessKey => "something shaped like an AWS access key id",
            FindingKind::PemBlock => "a PEM block (a private key or certificate)",
            FindingKind::HomePath => "your home folder's path, which names you and this machine",
            FindingKind::PrivateMemory => "text from memory you approved for private use only",
            FindingKind::DontUseMemory => "text from memory you did not approve (don't use)",
        }
    }
}

/// One finding: which file, and what kind. Deliberately nothing else.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Finding {
    pub path: String,
    pub kind: FindingKind,
}

/// Fixed-length-anchored needles, matched in one pass with a rolling hash so
/// thousands of memory lines cost one read of each output file.
#[derive(Debug, Default, Clone)]
struct Needles {
    anchor: usize,
    by_hash: HashMap<u64, Vec<usize>>,
    needles: Vec<(Vec<u8>, FindingKind)>,
    seen: BTreeSet<Vec<u8>>,
}

const BASE: u64 = 0x0100_0000_01b3;

fn window_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |hash, &byte| {
        hash.wrapping_mul(BASE).wrapping_add(byte as u64)
    })
}

impl Needles {
    fn new(anchor: usize) -> Needles {
        Needles {
            anchor,
            ..Needles::default()
        }
    }

    fn add(&mut self, needle: &[u8], kind: FindingKind) {
        if needle.len() < self.anchor || !self.seen.insert(needle.to_vec()) {
            return;
        }
        let index = self.needles.len();
        self.by_hash
            .entry(window_hash(&needle[..self.anchor]))
            .or_default()
            .push(index);
        self.needles.push((needle.to_vec(), kind));
    }

    fn len(&self) -> usize {
        self.needles.len()
    }

    fn find(&self, haystack: &[u8], found: &mut BTreeSet<FindingKind>) {
        let k = self.anchor;
        if self.needles.is_empty() || k == 0 || haystack.len() < k {
            return;
        }
        let top = (1..k).fold(1u64, |power, _| power.wrapping_mul(BASE));
        let mut hash = window_hash(&haystack[..k]);
        let mut start = 0;
        loop {
            if let Some(indices) = self.by_hash.get(&hash) {
                for &index in indices {
                    let (needle, kind) = &self.needles[index];
                    if haystack[start..].starts_with(needle) {
                        found.insert(*kind);
                    }
                }
            }
            if start + k >= haystack.len() {
                break;
            }
            hash = hash
                .wrapping_sub((haystack[start] as u64).wrapping_mul(top))
                .wrapping_mul(BASE)
                .wrapping_add(haystack[start + k] as u64);
            start += 1;
        }
    }
}

/// Everything the scan looks for, gathered before any file is read.
#[derive(Debug, Clone)]
pub struct SecretSet {
    exact: Needles,
    memory: Needles,
    home: Option<Vec<u8>>,
}

impl Default for SecretSet {
    fn default() -> Self {
        SecretSet::new()
    }
}

impl SecretSet {
    pub fn new() -> SecretSet {
        SecretSet {
            exact: Needles::new(MIN_EXACT_BYTES),
            memory: Needles::new(MIN_MEMORY_LINE_BYTES),
            home: None,
        }
    }

    /// A secret this machine holds. Values shorter than `MIN_EXACT_BYTES` are
    /// ignored; surrounding whitespace is not part of a secret.
    pub fn add_exact(&mut self, value: &str, kind: FindingKind) {
        self.exact.add(value.trim().as_bytes(), kind);
    }

    /// The owner's home folder. `/` and other one-part paths are ignored.
    pub fn set_home(&mut self, home: &Path) {
        let text = home.to_string_lossy().trim_end_matches('/').to_string();
        if text.starts_with('/') && text.len() > 2 && text[1..].contains(|c| c != '/') {
            self.home = Some(text.into_bytes());
        }
    }

    /// Lines of memory text that must not appear in public work.
    pub fn add_memory_text(&mut self, text: &str, kind: FindingKind) {
        for line in text.lines() {
            let line = normalize_memory_line(line);
            self.memory.add(line.as_bytes(), kind);
        }
    }

    /// One memory value matched whole (an MCP header or argument).
    pub fn add_memory_value(&mut self, value: &str, kind: FindingKind) {
        let value = value.trim();
        if value.len() >= MIN_EXACT_BYTES {
            // Short values are anchored on the exact table's shorter window.
            self.exact.add(value.as_bytes(), kind);
        }
    }

    /// How many exact secrets and memory lines are loaded, for tests and the
    /// command's summary line.
    pub fn counts(&self) -> (usize, usize) {
        (self.exact.len(), self.memory.len())
    }

    /// Scan one file, its name included. Findings are sorted and unique.
    pub fn scan_file(&self, path: &str, bytes: &[u8]) -> Vec<Finding> {
        let mut kinds = BTreeSet::new();
        for haystack in [path.as_bytes(), bytes] {
            self.exact.find(haystack, &mut kinds);
            self.memory.find(haystack, &mut kinds);
            patterns(haystack, &mut kinds);
            if let Some(home) = &self.home {
                if contains_home(haystack, home) {
                    kinds.insert(FindingKind::HomePath);
                }
            }
        }
        kinds
            .into_iter()
            .map(|kind| Finding {
                path: path.to_string(),
                kind,
            })
            .collect()
    }

    /// Scan every file, in the order given.
    pub fn scan<'a>(&self, files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Vec<Finding> {
        files
            .into_iter()
            .flat_map(|(path, bytes)| self.scan_file(path, bytes))
            .collect()
    }
}

/// Markdown decoration is not part of what was written: `# Rule`, `- rule` and
/// `> rule` all match `rule`.
fn normalize_memory_line(line: &str) -> String {
    line.trim()
        .trim_start_matches(|c: char| matches!(c, '#' | '-' | '*' | '>' | '+') || c.is_whitespace())
        .trim()
        .to_string()
}

fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Bytes from `start` that satisfy `accept`, up to the end or the first that
/// does not.
fn run_length(haystack: &[u8], start: usize, accept: impl Fn(u8) -> bool) -> usize {
    haystack[start.min(haystack.len())..]
        .iter()
        .take_while(|&&byte| accept(byte))
        .count()
}

fn starts_a_word(haystack: &[u8], at: usize) -> bool {
    at == 0 || !haystack[at - 1].is_ascii_alphanumeric()
}

fn occurrences<'a>(haystack: &'a [u8], prefix: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    haystack
        .windows(prefix.len())
        .enumerate()
        .filter(move |(_, window)| *window == prefix)
        .map(|(at, _)| at)
}

/// The well-known credential shapes, matched on bytes so binary files are
/// covered too.
fn patterns(haystack: &[u8], found: &mut BTreeSet<FindingKind>) {
    for at in occurrences(haystack, b"sk-") {
        if !starts_a_word(haystack, at) {
            continue;
        }
        let body = &haystack[at + 3..];
        if body.starts_with(b"ant-") {
            if run_length(haystack, at + 7, is_key_byte) >= 16 {
                found.insert(FindingKind::AnthropicKey);
            }
            continue;
        }
        let length = run_length(haystack, at + 3, is_key_byte);
        let key = &haystack[at + 3..at + 3 + length];
        if length >= 20
            && key.iter().any(u8::is_ascii_digit)
            && key.iter().any(u8::is_ascii_alphabetic)
        {
            found.insert(FindingKind::ApiKey);
        }
    }
    // Provider key prefixes, each followed by a long run of key characters:
    // Google `AIza` + 35, Groq `gsk_` + 52, xAI `xai-` + 80.
    for (prefix, min, kind) in [
        (&b"AIza"[..], 30, FindingKind::GoogleApiKey),
        (b"gsk_", 40, FindingKind::GroqKey),
        (b"xai-", 40, FindingKind::XaiKey),
    ] {
        for at in occurrences(haystack, prefix) {
            if starts_a_word(haystack, at)
                && run_length(haystack, at + prefix.len(), is_key_byte) >= min
            {
                found.insert(kind);
            }
        }
    }
    for prefix in [&b"ghp_"[..], b"gho_", b"ghu_", b"ghs_", b"ghr_"] {
        for at in occurrences(haystack, prefix) {
            if starts_a_word(haystack, at)
                && run_length(haystack, at + 4, |b| b.is_ascii_alphanumeric()) >= 30
            {
                found.insert(FindingKind::GithubToken);
            }
        }
    }
    for at in occurrences(haystack, b"github_pat_") {
        if starts_a_word(haystack, at)
            && run_length(haystack, at + 11, |b| {
                b.is_ascii_alphanumeric() || b == b'_'
            }) >= 30
        {
            found.insert(FindingKind::GithubToken);
        }
    }
    for prefix in [&b"AKIA"[..], b"ASIA"] {
        for at in occurrences(haystack, prefix) {
            let length = run_length(haystack, at + 4, |b| {
                b.is_ascii_uppercase() || b.is_ascii_digit()
            });
            let after = haystack.get(at + 4 + length).copied();
            if starts_a_word(haystack, at)
                && length == 16
                && !after.is_some_and(|b| b.is_ascii_alphanumeric())
            {
                found.insert(FindingKind::AwsAccessKey);
            }
        }
    }
    for at in occurrences(haystack, b"-----BEGIN ") {
        let label = run_length(haystack, at + 11, |b| {
            b.is_ascii_uppercase() || b.is_ascii_digit() || b == b' '
        });
        if label > 0 && haystack[at + 11 + label..].starts_with(b"-----") {
            found.insert(FindingKind::PemBlock);
        }
    }
}

/// The home path, not as the start of a longer name: `/Users/sam/x` and
/// `/Users/sam` match, `/Users/samantha` does not.
fn contains_home(haystack: &[u8], home: &[u8]) -> bool {
    occurrences(haystack, home).any(|at| {
        haystack.get(at + home.len()).is_none_or(|&next| {
            !(next.is_ascii_alphanumeric() || matches!(next, b'_' | b'-' | b'.'))
        })
    })
}

/// Every string in a JSON value, at any depth.
fn json_strings(value: &Value, into: &mut Vec<String>) {
    match value {
        Value::String(text) => into.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| json_strings(item, into)),
        Value::Object(map) => map.values().for_each(|item| json_strings(item, into)),
        _ => {}
    }
}

/// A login file's values worth matching: long, no spaces, and not a
/// timestamp (an expiry would match ordinary dates).
fn credential_values(value: &Value) -> Vec<String> {
    let mut strings = Vec::new();
    json_strings(value, &mut strings);
    strings
        .into_iter()
        .filter(|text| text.len() >= MIN_EXACT_BYTES && !text.contains(char::is_whitespace))
        .filter(|text| {
            let bytes = text.as_bytes();
            !(bytes.len() >= 5 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-')
        })
        .collect()
}

/// Read a small regular file, refusing links and anything large. Absent and
/// unreadable files are simply not there for the scan.
fn read_small(path: &Path, max: u64) -> Option<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > max {
        return None;
    }
    fs::read(path).ok()
}

/// Add every value of a JSON login or token file. Returns whether it was read.
pub fn add_credential_file(set: &mut SecretSet, path: &Path, kind: FindingKind) -> bool {
    let Some(bytes) = read_small(path, MAX_CREDENTIAL_FILE_BYTES) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    for secret in credential_values(&value) {
        set.add_exact(&secret, kind);
    }
    true
}

/// Where the login files of the other clients live on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialFiles {
    /// Daycare's own 0600 token file, and `DAYCARE_TOKEN_FILE` when it is set.
    pub daycare_token_files: Vec<PathBuf>,
    /// `$CODEX_HOME/auth.json` or `~/.codex/auth.json`.
    pub codex_auth: PathBuf,
    /// `$XDG_DATA_HOME/opencode/auth.json` or `~/.local/share/opencode/auth.json`.
    pub opencode_auth: PathBuf,
    /// `$CLAUDE_CONFIG_DIR/.credentials.json` or `~/.claude/.credentials.json`.
    pub claude_credentials: PathBuf,
}

impl CredentialFiles {
    /// The real machine's paths, honouring each client's own override.
    pub fn discover(home: &Path, daycare_fallback_token_file: &Path) -> CredentialFiles {
        let absolute = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let mut daycare_token_files = vec![daycare_fallback_token_file.to_path_buf()];
        if let Some(path) = absolute(crate::keychain::TOKEN_FILE_ENV) {
            if !daycare_token_files.contains(&path) {
                daycare_token_files.push(path);
            }
        }
        CredentialFiles {
            daycare_token_files,
            codex_auth: absolute("CODEX_HOME")
                .unwrap_or_else(|| home.join(".codex"))
                .join("auth.json"),
            opencode_auth: absolute("XDG_DATA_HOME")
                .unwrap_or_else(|| home.join(".local/share"))
                .join("opencode/auth.json"),
            claude_credentials: absolute("CLAUDE_CONFIG_DIR")
                .unwrap_or_else(|| home.join(".claude"))
                .join(".credentials.json"),
        }
    }

    pub fn add_to(&self, set: &mut SecretSet) {
        for path in &self.daycare_token_files {
            add_credential_file(set, path, FindingKind::DaycareTokenFile);
        }
        add_credential_file(set, &self.codex_auth, FindingKind::CodexLogin);
        add_credential_file(set, &self.opencode_auth, FindingKind::OpencodeLogin);
        add_credential_file(set, &self.claude_credentials, FindingKind::ClaudeLogin);
    }
}

/// Add the text of every memory item the approval keeps out of public work:
/// approved for private use, changed since approval, gone, or never approved.
/// Daycare's own bundled skills are public by nature and left out.
pub fn add_memory(
    set: &mut SecretSet,
    inventory: &Inventory,
    reconciled: &Reconciled,
    claude_json: &Path,
    home: &Path,
) {
    let public: BTreeSet<&str> = reconciled
        .approved
        .iter()
        .filter(|(_, scope)| *scope == Scope::Public)
        .map(|(item, _)| item.id.as_str())
        .collect();
    let private: BTreeSet<&str> = reconciled
        .approved
        .iter()
        .filter(|(_, scope)| *scope == Scope::Private)
        .map(|(item, _)| item.id.as_str())
        .collect();
    let claude_mcp = read_small(claude_json, MAX_MEMORY_FILE_BYTES)
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    for item in &inventory.items {
        if public.contains(item.id.as_str()) || item.origin.as_deref() == Some("daycare") {
            continue;
        }
        let kind = if private.contains(item.id.as_str()) {
            FindingKind::PrivateMemory
        } else {
            FindingKind::DontUseMemory
        };
        add_item(set, item, kind, claude_mcp.as_ref(), home);
    }
}

fn add_item(
    set: &mut SecretSet,
    item: &Item,
    kind: FindingKind,
    claude_json: Option<&Value>,
    home: &Path,
) {
    match item.kind {
        Kind::McpServer => {
            let name = item.name.as_deref().unwrap_or_default();
            match item.client {
                Client::Claude => {
                    if let Some(stanza) =
                        claude_json.and_then(|value| value["mcpServers"].get(name))
                    {
                        let mut strings = Vec::new();
                        json_strings(stanza, &mut strings);
                        for value in strings {
                            set.add_memory_value(&value, kind);
                        }
                    }
                }
                Client::Codex => {
                    let Some(bytes) = read_small(&item.path, MAX_MEMORY_FILE_BYTES) else {
                        return;
                    };
                    let text = String::from_utf8_lossy(&bytes);
                    for (server, stanza) in codex_mcp_stanzas(&text) {
                        if server == name {
                            for value in quoted_values(&stanza) {
                                set.add_memory_value(&value, kind);
                            }
                        }
                    }
                }
                Client::Opencode => add_opencode_stanza(set, item, &["mcp"], kind, home),
            }
        }
        // An OpenCode agent from a config file: its stanza's values (the
        // key its id names, `agent` or `mode`). One from a markdown file is
        // read whole, as instruction files are.
        Kind::Agent if is_json_config(&item.path) => {
            let name = item.name.as_deref().unwrap_or_default();
            let keys: &[&str] = if item.id.ends_with(&format!("#mode.{name}")) {
                &["mode"]
            } else if item.id.ends_with(&format!("#agent.{name}")) {
                &["agent"]
            } else {
                &["agent", "mode"]
            };
            add_opencode_stanza(set, item, keys, kind, home)
        }
        // A plugin listed in an OpenCode config: the options the owner gave
        // it (they can hold a token). Its code is the plugin author's.
        Kind::Plugin if item.client == Client::Opencode && is_json_config(&item.path) => {
            add_opencode_plugin_options(set, item, kind, home)
        }
        // A plugin or custom tool is code, used by reference; a plugin's files
        // are the plugin author's, not the owner's writing.
        Kind::Plugin | Kind::Tool => {}
        Kind::Skill => {
            let mut budget = MAX_MEMORY_FILE_BYTES;
            for file in text_files_under(&item.path) {
                if is_skill_script(&file) {
                    continue;
                }
                let Some(bytes) = read_small(&file, budget) else {
                    continue;
                };
                budget = budget.saturating_sub(bytes.len() as u64);
                set.add_memory_text(&without_fenced_code(&String::from_utf8_lossy(&bytes)), kind);
            }
        }
        _ => {
            if let Some(bytes) = read_small(&item.path, MAX_MEMORY_FILE_BYTES) {
                set.add_memory_text(&String::from_utf8_lossy(&bytes), kind);
            }
        }
    }
}

fn is_json_config(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "json" || ext == "jsonc")
}

fn read_opencode_config(path: &Path) -> Option<Value> {
    let bytes = read_small(path, MAX_MEMORY_FILE_BYTES)?;
    crate::approval::scan::parse_jsonc(&String::from_utf8_lossy(&bytes))
}

/// Every string in an OpenCode stanza, plus what OpenCode substitutes into
/// it: the text of each `{file:…}` it names and the value of each `{env:…}`.
fn add_opencode_values(
    set: &mut SecretSet,
    stanza: &Value,
    config: &Path,
    home: &Path,
    kind: FindingKind,
) {
    use crate::approval::scan::{
        opencode_env_references, opencode_file_references, resolve_opencode_file,
    };
    let mut strings = Vec::new();
    json_strings(stanza, &mut strings);
    for value in strings {
        set.add_memory_value(&value, kind);
    }
    for reference in opencode_file_references(stanza) {
        let file = resolve_opencode_file(&reference, config, home);
        if let Some(bytes) = read_small(&file, MAX_MEMORY_FILE_BYTES) {
            let text = String::from_utf8_lossy(&bytes);
            set.add_memory_value(&text, kind);
            set.add_memory_text(&text, kind);
        }
    }
    for name in opencode_env_references(stanza) {
        if let Some(value) = std::env::var_os(&name) {
            set.add_memory_value(&value.to_string_lossy(), kind);
        }
    }
}

/// The values of one named entry under `keys` in an OpenCode config file
/// (JSON with comments).
fn add_opencode_stanza(
    set: &mut SecretSet,
    item: &Item,
    keys: &[&str],
    kind: FindingKind,
    home: &Path,
) {
    let Some(config) = read_opencode_config(&item.path) else {
        return;
    };
    let name = item.name.as_deref().unwrap_or_default();
    for key in keys {
        if let Some(stanza) = config.get(*key).and_then(|entries| entries.get(name)) {
            add_opencode_values(set, stanza, &item.path, home, kind);
        }
    }
}

/// The options of every `plugin` entry for one spec (`[spec, options]`).
fn add_opencode_plugin_options(set: &mut SecretSet, item: &Item, kind: FindingKind, home: &Path) {
    let Some(config) = read_opencode_config(&item.path) else {
        return;
    };
    let spec = item.name.as_deref().unwrap_or_default();
    for entry in config
        .get("plugin")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Value::Array(pair) = entry {
            if pair.first().and_then(Value::as_str) == Some(spec) {
                for options in &pair[1..] {
                    add_opencode_values(set, options, &item.path, home, kind);
                }
            }
        }
    }
}

/// Substitute `{env:…}` and `{file:…}` in one OpenCode config value as
/// OpenCode does. A file that cannot be read gives nothing.
fn resolve_opencode_value(value: &str, config: &Path, home: &Path) -> String {
    let mut text = value.to_string();
    let as_value = Value::String(value.to_string());
    for name in crate::approval::scan::opencode_env_references(&as_value) {
        let resolved = std::env::var(&name).unwrap_or_default();
        text = text.replace(&format!("{{env:{name}}}"), &resolved);
    }
    for reference in crate::approval::scan::opencode_file_references(&as_value) {
        let file = crate::approval::scan::resolve_opencode_file(&reference, config, home);
        let resolved = read_small(&file, MAX_CREDENTIAL_FILE_BYTES)
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
            .unwrap_or_default();
        text = text.replace(&format!("{{file:{reference}}}"), &resolved);
    }
    text
}

/// The provider keys in the owner's OpenCode config files: every
/// `provider.<id>.options.apiKey` and header value, resolved. Each is a
/// secret whatever the approval says, so it is matched exactly, and so is
/// each long word of it (the token of `Bearer <token>`).
pub fn add_opencode_provider_keys(set: &mut SecretSet, roots: &crate::approval::scan::Roots) {
    for path in crate::approval::scan::opencode_config_files(roots) {
        let Some(config) = read_opencode_config(&path) else {
            continue;
        };
        let Some(providers) = config.get("provider").and_then(Value::as_object) else {
            continue;
        };
        for provider in providers.values() {
            let options = provider.get("options");
            let mut values = Vec::new();
            if let Some(key) = options
                .and_then(|o| o.get("apiKey"))
                .and_then(Value::as_str)
            {
                values.push(key.to_string());
            }
            if let Some(headers) = options.and_then(|o| o.get("headers")) {
                json_strings(headers, &mut values);
            }
            for value in values {
                let resolved = resolve_opencode_value(&value, &path, &roots.home);
                set.add_exact(&resolved, FindingKind::OpencodeProviderKey);
                for word in resolved.split_whitespace() {
                    set.add_exact(word, FindingKind::OpencodeProviderKey);
                }
            }
        }
    }
}

/// The value of every model provider's API-key variable that is set
/// (`challenge::opencode::PROVIDER_KEY_ENV`), the chosen provider's
/// included: the runner does not know at submit which one a session used.
pub fn add_provider_env_keys(
    set: &mut SecretSet,
    vars: impl IntoIterator<Item = (String, String)>,
) {
    let names: BTreeSet<&str> = crate::challenge::opencode::PROVIDER_KEY_ENV
        .iter()
        .flat_map(|(_, keys)| keys.iter().copied())
        .collect();
    for (name, value) in vars {
        if names.contains(name.as_str()) {
            set.add_exact(&value, FindingKind::ProviderKeyEnv);
        }
    }
}

/// A script in a skill folder: code, not the owner's writing (see the module
/// notes). Settings files (JSON, YAML, TOML) are still read.
fn is_skill_script(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "sh" | "py" | "js" | "ts"))
}

/// Markdown without its fenced code blocks (``` or ~~~), fences included. An
/// unclosed fence runs to the end, as Markdown renders it.
fn without_fenced_code(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker));
        match (fence, marker) {
            (None, Some(marker)) => fence = Some(marker),
            (Some(open), Some(marker)) if open == marker => fence = None,
            (None, None) => {
                kept.push_str(line);
                kept.push('\n');
            }
            _ => {}
        }
    }
    kept
}

/// The quoted string values in a TOML stanza, without a TOML parser: enough
/// for `KEY = "value"` and `args = ["a", "b"]`.
fn quoted_values(stanza: &str) -> Vec<String> {
    let mut values = Vec::new();
    for line in stanza.lines() {
        let mut rest = line;
        while let Some(open) = rest.find('"') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('"') else { break };
            values.push(after[..close].to_string());
            rest = &after[close + 1..];
        }
    }
    values
}

/// Text-like files in a skill directory, sorted, never through a link.
fn text_files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| {
                        matches!(
                            ext.to_ascii_lowercase().as_str(),
                            "md" | "txt"
                                | "json"
                                | "yaml"
                                | "yml"
                                | "toml"
                                | "sh"
                                | "py"
                                | "js"
                                | "ts"
                        )
                    })
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// The refusal, as the owner reads it: one line per file and kind.
pub fn refusal(findings: &[Finding]) -> String {
    let mut text = String::from(
        "the local secret scan found something in output/ that must not be sent, so nothing was \
         sent:\n",
    );
    for finding in findings {
        text.push_str(&format!(
            "  output/{}: {}\n",
            finding.path,
            finding.kind.describe()
        ));
    }
    text.push_str(
        "Take it out of those files (or remove the files), then run `daycare-runner challenge \
         submit` again",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::scan::item_id;
    use crate::testdir::unique_dir;

    const DEVICE: &str = "dck_device_secret_value_0123456789";

    fn kinds(set: &SecretSet, text: &str) -> Vec<FindingKind> {
        set.scan_file("out.txt", text.as_bytes())
            .into_iter()
            .map(|finding| finding.kind)
            .collect()
    }

    #[test]
    fn a_planted_device_token_is_found_by_exact_match_and_never_echoed() {
        let mut set = SecretSet::new();
        set.add_exact(DEVICE, FindingKind::DeviceToken);
        let findings = set.scan_file("notes/run.md", format!("token={DEVICE}\n").as_bytes());
        assert_eq!(
            findings,
            vec![Finding {
                path: "notes/run.md".into(),
                kind: FindingKind::DeviceToken
            }]
        );
        let message = refusal(&findings);
        assert!(message.contains("output/notes/run.md"), "{message}");
        assert!(!message.contains(DEVICE), "{message}");
        // In a file name too.
        assert_eq!(
            set.scan_file(&format!("{DEVICE}.txt"), b"clean")[0].kind,
            FindingKind::DeviceToken
        );
    }

    #[test]
    fn a_clean_file_passes() {
        let mut set = SecretSet::new();
        set.add_exact(DEVICE, FindingKind::DeviceToken);
        set.set_home(Path::new("/Users/sam"));
        set.add_memory_text(
            "# PRIVATE-GLOBAL-RULES-do-not-copy\n",
            FindingKind::PrivateMemory,
        );
        let html = "<!doctype html><form><label for=e>Email</label><input id=e></form>\n\
                    task-runner-configuration-file risk-assessment-2024-final\n\
                    /Users/samantha/report.csv AKIAshort\n";
        assert!(kinds(&set, html).is_empty(), "{:?}", kinds(&set, html));
    }

    #[test]
    fn each_credential_shape_is_found() {
        let set = SecretSet::new();
        let cases = [
            (
                "key: sk-ant-api03-abcdefghij0123456789",
                FindingKind::AnthropicKey,
            ),
            (
                "OPENAI=sk-proj-abcdefghij0123456789xyz",
                FindingKind::ApiKey,
            ),
            (
                "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                FindingKind::GithubToken,
            ),
            (
                "github_pat_11ABCDEFG0abcdefghijklmnopqrstuvwxyz",
                FindingKind::GithubToken,
            ),
            ("aws AKIAIOSFODNN7EXAMPLE here", FindingKind::AwsAccessKey),
            (
                // Split so the source never holds a whole key-shaped literal.
                concat!("key=AIza", "SyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q"),
                FindingKind::GoogleApiKey,
            ),
            (
                concat!("GROQ=gsk_", "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOP"),
                FindingKind::GroqKey,
            ),
            (
                concat!("xai-", "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefgh"),
                FindingKind::XaiKey,
            ),
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----",
                FindingKind::PemBlock,
            ),
        ];
        for (text, kind) in cases {
            assert_eq!(kinds(&set, text), vec![kind], "{text}");
        }
    }

    #[test]
    fn near_misses_are_not_findings() {
        let set = SecretSet::new();
        for text in [
            "sk-ant-short",
            "task-abcdefghijklmnopqrstuvwxyz0123",
            "sk-abcdefghijklmnopqrstuvwxyz",
            "ghp_short",
            "AIzaShort-but-not-a-key",
            "gsk_short_value",
            "the xai-grok model",
            "AKIAIOSFODNN7EXAMPLEX",
            "-----BEGIN-----",
        ] {
            assert!(kinds(&set, text).is_empty(), "{text}");
        }
    }

    #[test]
    fn the_home_path_matches_only_as_itself() {
        let mut set = SecretSet::new();
        set.set_home(Path::new("/Users/sam/"));
        assert_eq!(
            kinds(&set, "see /Users/sam/daycare"),
            vec![FindingKind::HomePath]
        );
        assert_eq!(kinds(&set, "\"/Users/sam\""), vec![FindingKind::HomePath]);
        assert!(kinds(&set, "/Users/samantha").is_empty());
        let mut root = SecretSet::new();
        root.set_home(Path::new("/"));
        assert!(kinds(&root, "/etc/hosts").is_empty());
    }

    #[test]
    fn memory_lines_match_without_their_markdown() {
        let mut set = SecretSet::new();
        set.add_memory_text(
            "# PRIVATE-GLOBAL-RULES-do-not-copy\n- short line\n",
            FindingKind::PrivateMemory,
        );
        assert_eq!(
            kinds(&set, "<p>PRIVATE-GLOBAL-RULES-do-not-copy</p>"),
            vec![FindingKind::PrivateMemory]
        );
        assert!(kinds(&set, "short line").is_empty());
    }

    #[test]
    fn the_scan_is_deterministic_and_reports_each_kind_once() {
        let mut set = SecretSet::new();
        set.add_exact(DEVICE, FindingKind::DeviceToken);
        let text = format!("{DEVICE} {DEVICE} sk-ant-api03-abcdefghij0123456789");
        let first = set.scan_file("a.txt", text.as_bytes());
        assert_eq!(first, set.scan_file("a.txt", text.as_bytes()));
        assert_eq!(
            first.iter().map(|f| f.kind).collect::<Vec<_>>(),
            vec![FindingKind::DeviceToken, FindingKind::AnthropicKey]
        );
    }

    #[test]
    fn login_files_contribute_their_long_values_only() {
        let dir = unique_dir("scan-logins");
        std::fs::create_dir_all(&dir).unwrap();
        let auth = dir.join("auth.json");
        std::fs::write(
            &auth,
            r#"{"auth_mode":"chatgpt","last_refresh":"2026-11-07T10:00:00Z",
                "tokens":{"access_token":"codex-access-token-0123456789","refresh_token":"rt"}}"#,
        )
        .unwrap();
        let mut set = SecretSet::new();
        assert!(add_credential_file(
            &mut set,
            &auth,
            FindingKind::CodexLogin
        ));
        assert_eq!(set.counts().0, 1);
        assert_eq!(
            kinds(&set, "codex-access-token-0123456789"),
            vec![FindingKind::CodexLogin]
        );
        assert!(kinds(&set, "chatgpt 2026-11-07T10:00:00Z").is_empty());
        // A link is never followed.
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&auth, &link).unwrap();
        assert!(!add_credential_file(
            &mut SecretSet::new(),
            &link,
            FindingKind::CodexLogin
        ));
    }

    #[test]
    fn approval_decides_which_memory_is_scanned_for() {
        let dir = unique_dir("scan-memory");
        std::fs::create_dir_all(&dir).unwrap();
        let item = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            Item {
                id: item_id(Client::Claude, Kind::ClaudeMd, &path, None),
                client: Client::Claude,
                kind: Kind::ClaudeMd,
                path,
                name: None,
                bytes: text.len() as u64,
                sha256: crate::self_update::sha256_hex(text.as_bytes()),
                origin: None,
            }
        };
        let private = item("private.md", "private rule that is long enough to match\n");
        let public = item("public.md", "public rule that is long enough to match\n");
        let unreviewed = item("unreviewed.md", "unreviewed rule that is long enough too\n");
        let inventory = Inventory {
            items: vec![private.clone(), public.clone(), unreviewed.clone()],
            warnings: vec![],
            ..Inventory::default()
        };
        let reconciled = Reconciled {
            approved: vec![(private, Scope::Private), (public, Scope::Public)],
            unreviewed: vec![unreviewed],
            ..Reconciled::default()
        };
        let mut set = SecretSet::new();
        add_memory(
            &mut set,
            &inventory,
            &reconciled,
            &dir.join("absent.json"),
            &dir,
        );
        assert_eq!(
            kinds(&set, "private rule that is long enough to match"),
            vec![FindingKind::PrivateMemory]
        );
        assert_eq!(
            kinds(&set, "unreviewed rule that is long enough too"),
            vec![FindingKind::DontUseMemory]
        );
        assert!(kinds(&set, "public rule that is long enough to match").is_empty());
    }

    #[test]
    fn a_skills_code_examples_and_scripts_are_not_matched_but_its_prose_is() {
        let dir = unique_dir("scan-skill-code");
        let skill = dir.join("web-quality");
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "# Web quality\n\nAlways check the owner's private launch checklist first.\n\n```html\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n```\n",
        )
        .unwrap();
        std::fs::write(
            skill.join("references/PATTERNS.md"),
            "~~~\n<label for=\"email\">Email address</label>\n~~~\nUse the house form pattern from the private handbook.\n",
        )
        .unwrap();
        std::fs::write(
            skill.join("check.py"),
            "print('a script line that is long enough')\n",
        )
        .unwrap();
        let skill_item = Item {
            id: item_id(Client::Claude, Kind::Skill, &skill, Some("web-quality")),
            client: Client::Claude,
            kind: Kind::Skill,
            path: skill.clone(),
            name: Some("web-quality".into()),
            bytes: 0,
            sha256: String::new(),
            origin: None,
        };
        let claude_md = dir.join("CLAUDE.md");
        std::fs::write(
            &claude_md,
            "```\nssh deploy@build-box.internal.example\n```\n",
        )
        .unwrap();
        let claude_md_item = Item {
            id: item_id(Client::Claude, Kind::ClaudeMd, &claude_md, None),
            client: Client::Claude,
            kind: Kind::ClaudeMd,
            path: claude_md,
            name: None,
            bytes: 0,
            sha256: String::new(),
            origin: None,
        };
        let inventory = Inventory {
            items: vec![skill_item.clone(), claude_md_item.clone()],
            warnings: vec![],
            ..Inventory::default()
        };
        let reconciled = Reconciled {
            unreviewed: vec![skill_item, claude_md_item],
            ..Reconciled::default()
        };
        let mut set = SecretSet::new();
        add_memory(
            &mut set,
            &inventory,
            &reconciled,
            &dir.join("absent.json"),
            &dir,
        );

        let entry = "<!doctype html><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<label for=\"email\">Email address</label>\nprint('a script line that is long enough')";
        assert!(kinds(&set, entry).is_empty(), "boilerplate matched");
        for prose in [
            "Always check the owner's private launch checklist first.",
            "Use the house form pattern from the private handbook.",
            // An instruction file's code block is the owner's own: still matched.
            "ssh deploy@build-box.internal.example",
        ] {
            assert_eq!(
                kinds(&set, prose),
                vec![FindingKind::DontUseMemory],
                "{prose}"
            );
        }
    }

    #[test]
    fn fenced_code_is_dropped_with_its_fences_and_an_open_fence_runs_to_the_end() {
        assert_eq!(
            without_fenced_code("a\n```js\nb\n```\nc\n  ~~~\nd\n```\ne\n~~~\nf\n```\ng"),
            "a\nc\nf\n"
        );
    }

    #[test]
    fn mcp_settings_values_are_matched_whole() {
        let dir = unique_dir("scan-mcp");
        std::fs::create_dir_all(&dir).unwrap();
        let claude_json = dir.join(".claude.json");
        std::fs::write(
            &claude_json,
            r#"{"mcpServers":{"github":{"headers":{"Authorization":"Bearer planted-mcp-secret-31337"}}}}"#,
        )
        .unwrap();
        let item = Item {
            id: item_id(
                Client::Claude,
                Kind::McpServer,
                &claude_json,
                Some("github"),
            ),
            client: Client::Claude,
            kind: Kind::McpServer,
            path: claude_json.clone(),
            name: Some("github".into()),
            bytes: 1,
            sha256: "0".repeat(64),
            origin: None,
        };
        let inventory = Inventory {
            items: vec![item.clone()],
            warnings: vec![],
            ..Inventory::default()
        };
        let reconciled = Reconciled {
            unreviewed: vec![item],
            ..Reconciled::default()
        };
        let mut set = SecretSet::new();
        add_memory(&mut set, &inventory, &reconciled, &claude_json, &dir);
        assert_eq!(
            kinds(&set, "header: Bearer planted-mcp-secret-31337"),
            vec![FindingKind::DontUseMemory]
        );
    }

    #[test]
    fn opencode_config_entries_are_matched_by_their_values_and_markdown_agents_whole() {
        let dir = unique_dir("scan-opencode");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        let config = dir.join("opencode.jsonc");
        std::fs::write(
            &config,
            "{\n  // mine\n  \"mcp\": {\"search\": {\"environment\": {\"KEY\": \"planted-oc-secret-4444\"}},},\n  \"agent\": {\"reviewer\": {\"prompt\": \"Always cite the internal wiki page first\"}}\n}",
        )
        .unwrap();
        let agent_file = dir.join("agents/helper.md");
        std::fs::write(
            &agent_file,
            "Answer in the house voice of our team, always\n",
        )
        .unwrap();
        let tool_file = dir.join("lookup.ts");
        std::fs::write(
            &tool_file,
            "export const planted = 'tool-code-not-memory-5555'\n",
        )
        .unwrap();
        let item = |kind: Kind, path: &Path, name: Option<&str>| Item {
            id: item_id(Client::Opencode, kind, path, name),
            client: Client::Opencode,
            kind,
            path: path.to_path_buf(),
            name: name.map(str::to_string),
            bytes: 1,
            sha256: "0".repeat(64),
            origin: None,
        };
        let items = vec![
            item(Kind::McpServer, &config, Some("search")),
            item(Kind::Agent, &config, Some("reviewer")),
            item(Kind::Agent, &agent_file, Some("helper")),
            item(Kind::Tool, &tool_file, None),
        ];
        let inventory = Inventory {
            items: items.clone(),
            warnings: vec![],
            ..Inventory::default()
        };
        let reconciled = Reconciled {
            unreviewed: items,
            ..Reconciled::default()
        };
        let mut set = SecretSet::new();
        add_memory(
            &mut set,
            &inventory,
            &reconciled,
            &dir.join(".claude.json"),
            &dir,
        );
        for text in [
            "env: planted-oc-secret-4444",
            "Always cite the internal wiki page first",
            "Answer in the house voice of our team, always",
        ] {
            assert_eq!(
                kinds(&set, text),
                vec![FindingKind::DontUseMemory],
                "{text}"
            );
        }
        // A custom tool is code, used by reference, as a plugin is.
        assert!(kinds(&set, "tool-code-not-memory-5555").is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn opencode_plugin_options_provider_keys_and_substituted_values_are_found() {
        let dir = unique_dir("scan-opencode-keys");
        let home = dir.join("home");
        let roots = crate::approval::scan::Roots {
            claude_config_dir: home.join(".claude"),
            claude_json: home.join(".claude.json"),
            codex_home: home.join(".codex"),
            agents_home: home.join(".agents"),
            opencode_config_dir: home.join(".config/opencode"),
            opencode_dot_dir: home.join(".opencode"),
            challenge_root: home.join("daycare/challenges"),
            daycare_root: home.join(".claude-daycare"),
            home: home.clone(),
        };
        let write = |path: &Path, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            &roots.opencode_config_dir.join("opencode.jsonc"),
            r#"{
  // A token in a plugin's options, a header from a file, provider keys.
  "plugin": ["opencode-notify", ["opencode-notify", {"token": "plugin-option-token-1234"}]],
  "mcp": {"docs": {"type": "remote", "url": "https://x.test/mcp",
                   "headers": {"Authorization": "Bearer {file:~/.secrets/docs}"}}},
  "provider": {
    "openai": {"options": {"apiKey": "{file:~/.secrets/openai}"}},
    "groq": {"options": {"apiKey": "literal-provider-key-5678",
                         "headers": {"Authorization": "Bearer header-token-abcdef12"}}},
  },
}"#,
        );
        write(&home.join(".secrets/docs"), "docs-token-from-a-file-9999\n");
        write(
            &home.join(".secrets/openai"),
            "openai-key-from-a-file-7777\n",
        );
        let inventory = crate::approval::scan::scan(&roots);
        assert!(
            inventory.opencode_unreadable.is_empty(),
            "{:?}",
            inventory.warnings
        );
        let reconciled = Reconciled {
            unreviewed: inventory.items.clone(),
            ..Reconciled::default()
        };
        let mut set = SecretSet::new();
        add_memory(&mut set, &inventory, &reconciled, &roots.claude_json, &home);
        add_opencode_provider_keys(&mut set, &roots);
        add_provider_env_keys(
            &mut set,
            [
                (
                    "GROQ_API_KEY".to_string(),
                    "env-groq-value-123456".to_string(),
                ),
                (
                    "UNRELATED".to_string(),
                    "unrelated-value-123456".to_string(),
                ),
            ],
        );
        for (text, kind) in [
            (
                "token: plugin-option-token-1234",
                FindingKind::DontUseMemory,
            ),
            ("docs-token-from-a-file-9999", FindingKind::DontUseMemory),
            (
                "k=literal-provider-key-5678",
                FindingKind::OpencodeProviderKey,
            ),
            (
                "openai-key-from-a-file-7777",
                FindingKind::OpencodeProviderKey,
            ),
            ("header-token-abcdef12", FindingKind::OpencodeProviderKey),
            ("env-groq-value-123456", FindingKind::ProviderKeyEnv),
        ] {
            assert_eq!(kinds(&set, text), vec![kind], "{text}");
        }
        assert!(kinds(&set, "unrelated-value-123456").is_empty());
        // Approved for public work, a plugin's options are not matched as
        // memory; a provider key still is, whatever the approval says.
        let public = Reconciled {
            approved: inventory
                .items
                .iter()
                .map(|item| (item.clone(), Scope::Public))
                .collect(),
            ..Reconciled::default()
        };
        let mut set = SecretSet::new();
        add_memory(&mut set, &inventory, &public, &roots.claude_json, &home);
        add_opencode_provider_keys(&mut set, &roots);
        assert!(kinds(&set, "plugin-option-token-1234").is_empty());
        assert_eq!(
            kinds(&set, "literal-provider-key-5678"),
            vec![FindingKind::OpencodeProviderKey]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn codex_stanza_values_are_read_from_quotes() {
        assert_eq!(
            quoted_values(
                "[mcp_servers.docs]\ncommand = \"docs-mcp\"\nargs = [\"--token\", \"abc\"]"
            ),
            vec!["docs-mcp", "--token", "abc"]
        );
    }
}
