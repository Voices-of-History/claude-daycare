//! The deterministic inventory behind `memory setup --scan` (plan task N4).
//!
//! It lists every piece of the owner's own memory that Claude Code or Codex
//! would load into a session started in a challenge folder: instruction files
//! (CLAUDE.md layers, rules, AGENTS.md layers), auto memory, user skills and
//! commands, plugins, and MCP servers. Each item is a path plus a sha256, so an
//! approval can be bound to exactly the bytes the owner reviewed.
//!
//! What it never does:
//! - read out file contents (only hashes and sizes leave this module);
//! - expose MCP settings values: an MCP item is its server name, the file that
//!   declares it, and a hash of its stanza, never the stanza;
//! - look inside `~/.claude-daycare`, which is Daycare's own state.
//!
//! The same machine scanned twice gives byte-identical output: items are
//! sorted by id and carry no timestamps.

use crate::self_update::sha256_hex;
use crate::skills;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The two clients a challenge can run in today. OpenCode is not supported
/// for challenges yet, so it has no inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    Claude,
    Codex,
}

impl Client {
    pub fn as_str(self) -> &'static str {
        match self {
            Client::Claude => "claude",
            Client::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The user-level CLAUDE.md in the Claude config directory.
    ClaudeMd,
    /// A user-level rules file, `<config>/rules/**/*.md`.
    Rules,
    /// A CLAUDE.md, CLAUDE.local.md or .claude/CLAUDE.md in a directory above
    /// the challenge folder. Claude loads every one of them.
    ProjectClaudeMd,
    /// A `.claude/rules/**/*.md` file in a directory above the challenge folder.
    ProjectRules,
    /// A file in Claude's auto memory, `<config>/projects/*/memory/**`.
    AutoMemory,
    /// A user skill directory. Its hash covers every file in it.
    Skill,
    /// A user slash command, `<config>/commands/**/*.md`.
    Command,
    /// An installed Claude Code plugin. Hashed by its install record.
    Plugin,
    /// One MCP server stanza. Hashed, never shown.
    McpServer,
    /// The user-level AGENTS.md or AGENTS.override.md in the Codex home.
    AgentsMd,
    /// An AGENTS.md or AGENTS.override.md in a directory above the folder.
    ProjectAgentsMd,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ClaudeMd => "claude_md",
            Kind::Rules => "rules",
            Kind::ProjectClaudeMd => "project_claude_md",
            Kind::ProjectRules => "project_rules",
            Kind::AutoMemory => "auto_memory",
            Kind::Skill => "skill",
            Kind::Command => "command",
            Kind::Plugin => "plugin",
            Kind::McpServer => "mcp_server",
            Kind::AgentsMd => "agents_md",
            Kind::ProjectAgentsMd => "project_agents_md",
        }
    }

    /// Instruction text the client reads into context. These are copied into
    /// the challenge folder when approved, and excluded at the source.
    pub fn is_instruction_file(self) -> bool {
        matches!(
            self,
            Kind::ClaudeMd
                | Kind::Rules
                | Kind::ProjectClaudeMd
                | Kind::ProjectRules
                | Kind::AutoMemory
                | Kind::AgentsMd
                | Kind::ProjectAgentsMd
                | Kind::Command
        )
    }
}

/// One candidate memory item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// `<client>:<kind>:<path>` plus `#<name>` for items that share a file.
    pub id: String,
    pub client: Client,
    pub kind: Kind,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub bytes: u64,
    pub sha256: String,
    /// `daycare` for Daycare's own bundled skills, `daycare-library` for a
    /// skill installed from the Daycare library. Absent for the owner's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl Item {
    fn new(
        client: Client,
        kind: Kind,
        path: PathBuf,
        name: Option<String>,
        bytes: u64,
        sha256: String,
    ) -> Item {
        Item {
            id: item_id(client, kind, &path, name.as_deref()),
            client,
            kind,
            path,
            name,
            bytes,
            sha256,
            origin: None,
        }
    }
}

pub fn item_id(client: Client, kind: Kind, path: &Path, name: Option<&str>) -> String {
    let mut id = format!("{}:{}:{}", client.as_str(), kind.as_str(), path.display());
    if let Some(name) = name {
        id.push('#');
        id.push_str(name);
    }
    id
}

/// Where each client keeps the owner's memory on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Roots {
    pub home: PathBuf,
    /// `CLAUDE_CONFIG_DIR`, or `~/.claude`.
    pub claude_config_dir: PathBuf,
    /// `~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`.
    pub claude_json: PathBuf,
    /// `CODEX_HOME`, or `~/.codex`.
    pub codex_home: PathBuf,
    /// The shared `~/.agents` folder Codex reads skills from.
    pub agents_home: PathBuf,
    /// The folder challenge folders are made in. Its ancestors are where
    /// project-level CLAUDE.md and AGENTS.md layers come from.
    pub challenge_root: PathBuf,
    /// Daycare's own state (`~/.claude-daycare` or `DAYCARE_HOME`). Never
    /// inventoried.
    pub daycare_root: PathBuf,
}

impl Roots {
    /// The real machine's roots, honouring the clients' own overrides.
    pub fn discover(daycare_root: &Path, challenge_root: &Path) -> Result<Roots> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::new("HOME is not set; cannot find your agents' memory"))?;
        let claude_config = absolute_env("CLAUDE_CONFIG_DIR");
        let (claude_config_dir, claude_json) = match claude_config {
            Some(dir) => (dir.clone(), dir.join(".claude.json")),
            None => (home.join(".claude"), home.join(".claude.json")),
        };
        let codex_home = absolute_env("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
        Ok(Roots {
            agents_home: home.join(".agents"),
            home,
            claude_config_dir,
            claude_json,
            codex_home,
            challenge_root: challenge_root.to_path_buf(),
            daycare_root: daycare_root.to_path_buf(),
        })
    }
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The scan result: items in id order, plus anything that could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub items: Vec<Item>,
    pub warnings: Vec<String>,
}

impl Inventory {
    pub fn get(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn for_client(&self, client: Client) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(move |item| item.client == client)
    }
}

/// Scan both clients.
pub fn scan(roots: &Roots) -> Inventory {
    let mut scanner = Scanner {
        roots,
        items: BTreeMap::new(),
        seen_files: std::collections::BTreeSet::new(),
        warnings: Vec::new(),
    };
    scanner.claude();
    scanner.codex();
    let mut warnings = scanner.warnings;
    warnings.sort();
    warnings.dedup();
    Inventory {
        items: scanner.items.into_values().collect(),
        warnings,
    }
}

struct Scanner<'a> {
    roots: &'a Roots,
    items: BTreeMap<String, Item>,
    seen_files: std::collections::BTreeSet<(Client, PathBuf)>,
    warnings: Vec<String>,
}

impl Scanner<'_> {
    fn push(&mut self, item: Item) {
        if item.path.starts_with(&self.roots.daycare_root) {
            return;
        }
        // One file is one item per client, however it was reached: `$HOME/.claude/CLAUDE.md`
        // is also an ancestor's `.claude/CLAUDE.md`, and a folder reached
        // through a symlink (macOS `/var` is `/private/var`) is walked twice.
        // The first spelling wins; user-level layers are scanned first.
        // MCP servers and plugins share a file and are keyed by name instead.
        if !matches!(item.kind, Kind::McpServer | Kind::Plugin) {
            let physical = fs::canonicalize(&item.path).unwrap_or_else(|_| item.path.clone());
            if !self.seen_files.insert((item.client, physical)) {
                return;
            }
        }
        self.items.entry(item.id.clone()).or_insert(item);
    }

    /// A file that exists is hashed; a missing one is skipped; an unreadable
    /// one becomes a warning (the client cannot read it either).
    fn file(&mut self, client: Client, kind: Kind, path: PathBuf) {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.warnings
                    .push(format!("could not inspect {}: {error}", path.display()));
                return;
            }
        }
        match fs::read(&path) {
            Ok(bytes) => {
                let item = Item::new(
                    client,
                    kind,
                    path,
                    None,
                    bytes.len() as u64,
                    sha256_hex(&bytes),
                );
                self.push(item);
            }
            Err(error) => self
                .warnings
                .push(format!("could not read {}: {error}", path.display())),
        }
    }

    /// Every `.md` file under `dir`, recursively, in path order.
    fn markdown_tree(&mut self, client: Client, kind: Kind, dir: &Path) {
        for path in walk_files(dir, &mut self.warnings) {
            if path.extension().is_some_and(|ext| ext == "md") {
                self.file(client, kind, path);
            }
        }
    }

    fn claude(&mut self) {
        let config = self.roots.claude_config_dir.clone();
        self.file(Client::Claude, Kind::ClaudeMd, config.join("CLAUDE.md"));
        self.markdown_tree(Client::Claude, Kind::Rules, &config.join("rules"));

        for dir in ancestors(&self.roots.challenge_root) {
            for path in [
                dir.join("CLAUDE.md"),
                dir.join("CLAUDE.local.md"),
                dir.join(".claude").join("CLAUDE.md"),
            ] {
                if !self.is_user_level_claude(&path) {
                    self.file(Client::Claude, Kind::ProjectClaudeMd, path);
                }
            }
            let rules = dir.join(".claude").join("rules");
            if rules != config.join("rules") {
                self.markdown_tree(Client::Claude, Kind::ProjectRules, &rules);
            }
        }

        // Auto memory: every project's memory folder. A challenge session
        // turns auto memory off, so an approved file reaches it only as a copy.
        for project in sorted_dirs(&config.join("projects"), &mut self.warnings) {
            self.markdown_tree(Client::Claude, Kind::AutoMemory, &project.join("memory"));
        }

        for dir in sorted_dirs(&config.join("skills"), &mut self.warnings) {
            self.skill(Client::Claude, &dir);
        }

        let commands = config.join("commands");
        for path in walk_files(&commands, &mut self.warnings) {
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            let name = command_name(&commands, &path);
            match fs::read(&path) {
                Ok(bytes) => {
                    let item = Item::new(
                        Client::Claude,
                        Kind::Command,
                        path,
                        Some(name),
                        bytes.len() as u64,
                        sha256_hex(&bytes),
                    );
                    self.push(item);
                }
                Err(error) => self
                    .warnings
                    .push(format!("could not read {}: {error}", path.display())),
            }
        }

        self.claude_plugins(&config.join("plugins").join("installed_plugins.json"));
        let claude_json = self.roots.claude_json.clone();
        self.claude_mcp(&claude_json);
    }

    fn is_user_level_claude(&self, path: &Path) -> bool {
        path == self.roots.claude_config_dir.join("CLAUDE.md")
    }

    fn skill(&mut self, client: Client, dir: &Path) {
        let skill_file = dir.join("SKILL.md");
        let Ok(text) = fs::read_to_string(&skill_file) else {
            return;
        };
        let dir_name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = skills::frontmatter_name(&text).unwrap_or(dir_name);
        let (bytes, sha256) = match tree_digest(dir) {
            Ok(digest) => digest,
            Err(error) => {
                self.warnings.push(format!(
                    "could not read the skill at {}: {error}",
                    dir.display()
                ));
                return;
            }
        };
        let mut item = Item::new(
            client,
            Kind::Skill,
            dir.to_path_buf(),
            Some(name),
            bytes,
            sha256,
        );
        if dir.join(".daycare-skill.json").exists() {
            item.origin = Some("daycare-library".into());
        } else if skills::is_any_bundled(&text) {
            item.origin = Some("daycare".into());
        }
        self.push(item);
    }

    /// Plugins are listed from Claude Code's install record. A plugin folder
    /// can be large, so the hash covers the install record (id, path, version,
    /// commit) rather than every file: an update changes it, which is what
    /// should trigger a fresh review.
    fn claude_plugins(&mut self, record: &Path) {
        let Some(value) = self.read_json(record) else {
            return;
        };
        let Some(plugins) = value.get("plugins").and_then(serde_json::Value::as_object) else {
            return;
        };
        for (id, entries) in plugins {
            let entries: Vec<&serde_json::Value> = match entries {
                serde_json::Value::Array(list) => list.iter().collect(),
                other => vec![other],
            };
            let identity: Vec<serde_json::Value> = entries
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "installPath": entry.get("installPath"),
                        "version": entry.get("version"),
                        "gitCommitSha": entry.get("gitCommitSha"),
                    })
                })
                .collect();
            let install_path = entries
                .iter()
                .find_map(|entry| entry.get("installPath")?.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| record.to_path_buf());
            let canonical = serde_json::json!({ "id": id, "installs": identity }).to_string();
            self.push(Item::new(
                Client::Claude,
                Kind::Plugin,
                install_path,
                Some(id.clone()),
                canonical.len() as u64,
                sha256_hex(canonical.as_bytes()),
            ));
        }
    }

    /// User-scope MCP servers from `~/.claude.json`. The stanza is hashed in
    /// a canonical form (sorted keys) and never stored or printed.
    fn claude_mcp(&mut self, claude_json: &Path) {
        let Some(value) = self.read_json(claude_json) else {
            return;
        };
        let Some(servers) = value
            .get("mcpServers")
            .and_then(serde_json::Value::as_object)
        else {
            return;
        };
        for (name, stanza) in servers {
            let canonical = stanza.to_string();
            self.push(Item::new(
                Client::Claude,
                Kind::McpServer,
                claude_json.to_path_buf(),
                Some(name.clone()),
                canonical.len() as u64,
                sha256_hex(canonical.as_bytes()),
            ));
        }
    }

    fn read_json(&mut self, path: &Path) -> Option<serde_json::Value> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                self.warnings
                    .push(format!("could not read {}: {error}", path.display()));
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(value) => Some(value),
            // Never include the parse error: it can quote the file.
            Err(_) => {
                self.warnings
                    .push(format!("{} is not valid JSON; skipped", path.display()));
                None
            }
        }
    }

    fn codex(&mut self) {
        let codex_home = self.roots.codex_home.clone();
        for name in ["AGENTS.md", "AGENTS.override.md"] {
            self.file(Client::Codex, Kind::AgentsMd, codex_home.join(name));
        }
        for dir in ancestors(&self.roots.challenge_root) {
            for name in ["AGENTS.md", "AGENTS.override.md"] {
                let path = dir.join(name);
                if path.parent() != Some(codex_home.as_path()) {
                    self.file(Client::Codex, Kind::ProjectAgentsMd, path);
                }
            }
        }
        for root in [
            codex_home.join("skills"),
            self.roots.agents_home.join("skills"),
        ] {
            for dir in sorted_dirs(&root, &mut self.warnings) {
                self.skill(Client::Codex, &dir);
            }
        }
        let config = codex_home.join("config.toml");
        match fs::read_to_string(&config) {
            Ok(text) => {
                for (name, stanza) in codex_mcp_stanzas(&text) {
                    self.push(Item::new(
                        Client::Codex,
                        Kind::McpServer,
                        config.clone(),
                        Some(name),
                        stanza.len() as u64,
                        sha256_hex(stanza.as_bytes()),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => self
                .warnings
                .push(format!("could not read {}: {error}", config.display())),
        }
    }
}

/// The folder itself and every directory above it, lexically and, where it
/// differs, physically (Claude resolves symlinks before walking).
fn ancestors(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = dir.ancestors().map(Path::to_path_buf).collect();
    let mut existing = dir;
    while fs::symlink_metadata(existing).is_err() {
        match existing.parent() {
            Some(parent) => existing = parent,
            None => break,
        }
    }
    if let Ok(physical) = fs::canonicalize(existing) {
        out.extend(physical.ancestors().map(Path::to_path_buf));
    }
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|dir| seen.insert(dir.clone()));
    out
}

/// Subdirectories of `dir`, sorted, following a symlinked entry (skills are
/// often symlinked in from a dotfiles repo).
fn sorted_dirs(dir: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warnings.push(format!("could not list {}: {error}", dir.display()));
            return Vec::new();
        }
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Regular files under `dir`, recursively, sorted. Symlinked files are
/// followed (the client follows them); symlinked directories are not entered.
fn walk_files(dir: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => continue,
            Err(error) => {
                warnings.push(format!("could not list {}: {error}", current.display()));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(link) = fs::symlink_metadata(&path) else {
                continue;
            };
            if link.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// A skill or command name as Claude Code shows it: the path below
/// `commands/` without `.md`, with folders joined by `:`.
fn command_name(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path).with_extension("");
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

/// A digest of a whole directory: one line per entry, sorted by relative
/// path, then hashed. Files contribute their own sha256, symlinks their target
/// (they are never followed, and never copied into a challenge folder).
/// Returns the total file bytes and the digest.
pub fn tree_digest(dir: &Path) -> Result<(u64, String)> {
    let mut lines = Vec::new();
    let mut bytes = 0u64;
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let current = dir.join(&relative);
        let mut entries: Vec<_> = fs::read_dir(&current)?
            .filter_map(|entry| entry.ok())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let rel = relative.join(entry.file_name());
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            let shown = rel.to_string_lossy().into_owned();
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path)?;
                lines.push(format!("{shown}\0link\0{}", target.display()));
            } else if metadata.is_dir() {
                lines.push(format!("{shown}\0dir"));
                stack.push(rel);
            } else if metadata.is_file() {
                let contents = fs::read(&path)?;
                bytes += contents.len() as u64;
                lines.push(format!("{shown}\0file\0{}", sha256_hex(&contents)));
            }
        }
    }
    lines.sort();
    Ok((bytes, sha256_hex(lines.join("\n").as_bytes())))
}

/// `[mcp_servers.<name>]` stanzas from a Codex `config.toml`, as raw text per
/// server (header and nested tables included). The text is only ever hashed.
///
/// There is no TOML parser in this crate, so this reads just enough: table
/// headers (bare or quoted keys), inline entries under a bare
/// `[mcp_servers]`, and root-level dotted `mcp_servers.<name>.*` keys. A `[`
/// line inside a multi-line string or array is not mistaken for a header.
pub fn codex_mcp_stanzas(text: &str) -> Vec<(String, String)> {
    let mut stanzas: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // None: root table. Some(path): the current table header's key path.
    let mut table: Option<Vec<String>> = None;
    let mut in_multiline = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_multiline && trimmed.starts_with('[') {
            if let Some(path) = parse_header(trimmed) {
                table = Some(path);
                if let Some(server) = server_of(table.as_deref()) {
                    stanzas.entry(server).or_default().push(trimmed.to_string());
                }
                continue;
            }
        }
        let toggles = line.matches("\"\"\"").count() + line.matches("'''").count();
        let was_multiline = in_multiline;
        if toggles % 2 == 1 {
            in_multiline = !in_multiline;
        }
        if let Some(server) = server_of(table.as_deref()) {
            stanzas
                .entry(server)
                .or_default()
                .push(line.trim_end().to_string());
            continue;
        }
        if was_multiline || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, _)) = trimmed.split_once('=') else {
            continue;
        };
        let Some(key) = parse_key(key.trim()) else {
            continue;
        };
        let server = match table.as_deref() {
            // `[mcp_servers]` then `name = { ... }`
            Some([only]) if only == "mcp_servers" => key.first().cloned(),
            // root-level `mcp_servers.name.command = "..."`
            None if key.len() >= 2 && key[0] == "mcp_servers" => Some(key[1].clone()),
            _ => None,
        };
        if let Some(server) = server {
            stanzas.entry(server).or_default().push(trimmed.to_string());
        }
    }
    stanzas
        .into_iter()
        .map(|(name, lines)| {
            let mut text = lines.join("\n");
            while text.ends_with('\n') {
                text.pop();
            }
            (name, text.trim_end().to_string())
        })
        .collect()
}

fn server_of(table: Option<&[String]>) -> Option<String> {
    match table {
        Some([first, server, ..]) if first == "mcp_servers" => Some(server.clone()),
        _ => None,
    }
}

/// `[a.b."c d"]` or `[[a.b]]`, with an optional trailing comment.
fn parse_header(line: &str) -> Option<Vec<String>> {
    let (open, close) = if line.starts_with("[[") {
        ("[[", "]]")
    } else {
        ("[", "]")
    };
    let inner = line.strip_prefix(open)?;
    let end = find_unquoted(inner, close)?;
    let rest = inner[end + close.len()..].trim();
    if !(rest.is_empty() || rest.starts_with('#')) {
        return None;
    }
    parse_key(inner[..end].trim())
}

fn find_unquoted(text: &str, needle: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (index, c) in text.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if text[index..].starts_with(needle) => return Some(index),
            None => {}
        }
    }
    None
}

/// A dotted TOML key: bare parts `[A-Za-z0-9_-]+` or quoted strings.
fn parse_key(key: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = key.trim();
    loop {
        let (part, after) = if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"')?;
            (quoted[..end].to_string(), &quoted[end + 1..])
        } else if let Some(quoted) = rest.strip_prefix('\'') {
            let end = quoted.find('\'')?;
            (quoted[..end].to_string(), &quoted[end + 1..])
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            (rest[..end].to_string(), &rest[end..])
        };
        parts.push(part);
        let after = after.trim_start();
        if after.is_empty() {
            return Some(parts);
        }
        rest = after.strip_prefix('.')?.trim_start();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::unique_dir;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn roots(base: &Path) -> Roots {
        let home = base.join("home");
        Roots {
            claude_config_dir: home.join(".claude"),
            claude_json: home.join(".claude.json"),
            codex_home: home.join(".codex"),
            agents_home: home.join(".agents"),
            challenge_root: home.join("daycare/challenges"),
            daycare_root: home.join(".claude-daycare"),
            home,
        }
    }

    /// A home with one of everything, including a secret in an MCP stanza.
    fn populated() -> (PathBuf, Roots) {
        let base = unique_dir("daycare-scan");
        let roots = roots(&base);
        let h = &roots.home;
        write(&h.join(".claude/CLAUDE.md"), "# My private global rules\n");
        write(&h.join(".claude/rules/style.md"), "Use tabs.\n");
        write(&h.join("CLAUDE.md"), "home-level project memory\n");
        write(
            &h.join(".claude/projects/-Users-sam-app/memory/MEMORY.md"),
            "remembered thing\n",
        );
        write(
            &h.join(".claude/skills/review/SKILL.md"),
            "---\nname: review\n---\nbody\n",
        );
        write(&h.join(".claude/skills/review/notes.txt"), "extra\n");
        write(
            &h.join(".claude/skills/daycare/SKILL.md"),
            skills::DAYCARE.markdown,
        );
        write(&h.join(".claude/commands/git/commit.md"), "commit it\n");
        write(
            &h.join(".claude/plugins/installed_plugins.json"),
            r#"{"version":2,"plugins":{"fmt@tools":[{"scope":"user","installPath":"/p/fmt","version":"1.0.0","installedAt":"x"}]}}"#,
        );
        write(
            &h.join(".claude.json"),
            r#"{"mcpServers":{"github":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer sk-planted-secret-123"}}},"oauthAccount":{"emailAddress":"sam@example.test"}}"#,
        );
        write(&h.join(".codex/AGENTS.md"), "codex global\n");
        write(
            &h.join(".codex/config.toml"),
            "model = \"gpt\"\n\n[mcp_servers.docs]\ncommand = \"npx\"\nenv = { TOKEN = \"codex-planted-secret-456\" }\n\n[mcp_servers.docs.env]\nOTHER = \"x\"\n\n[profiles.fast]\nmodel = \"y\"\n",
        );
        write(
            &h.join(".agents/skills/notes/SKILL.md"),
            "---\nname: notes\n---\n",
        );
        write(&h.join(".claude-daycare/approval.json"), "{}");
        (base, roots)
    }

    #[test]
    fn the_scan_lists_every_layer_with_hashes_and_never_contents_or_secrets() {
        let (base, roots) = populated();
        let inventory = scan(&roots);
        let json = serde_json::to_string(&inventory).unwrap();
        for secret in [
            "sk-planted-secret-123",
            "codex-planted-secret-456",
            "Bearer",
            "My private global rules",
            "remembered thing",
            "sam@example.test",
            "example.test/mcp",
        ] {
            assert!(!json.contains(secret), "scan leaked {secret:?}: {json}");
        }
        let kinds: Vec<(Client, Kind, Option<&str>)> = inventory
            .items
            .iter()
            .map(|item| (item.client, item.kind, item.name.as_deref()))
            .collect();
        for expected in [
            (Client::Claude, Kind::ClaudeMd, None),
            (Client::Claude, Kind::Rules, None),
            (Client::Claude, Kind::ProjectClaudeMd, None),
            (Client::Claude, Kind::AutoMemory, None),
            (Client::Claude, Kind::Skill, Some("review")),
            (Client::Claude, Kind::Skill, Some("daycare")),
            (Client::Claude, Kind::Command, Some("git:commit")),
            (Client::Claude, Kind::Plugin, Some("fmt@tools")),
            (Client::Claude, Kind::McpServer, Some("github")),
            (Client::Codex, Kind::AgentsMd, None),
            (Client::Codex, Kind::Skill, Some("notes")),
            (Client::Codex, Kind::McpServer, Some("docs")),
        ] {
            assert!(
                kinds.contains(&expected),
                "missing {expected:?} in {kinds:?}"
            );
        }
        let claude_md = inventory
            .items
            .iter()
            .find(|item| item.kind == Kind::ClaudeMd)
            .unwrap();
        assert_eq!(claude_md.sha256, sha256_hex(b"# My private global rules\n"));
        assert_eq!(claude_md.bytes, 26);
        // The user CLAUDE.md is listed once, as user memory, even though
        // $HOME/.claude/CLAUDE.md is also an ancestor's `.claude/CLAUDE.md`.
        assert_eq!(
            inventory
                .items
                .iter()
                .filter(|item| item.path == roots.home.join(".claude/CLAUDE.md"))
                .count(),
            1
        );
        let daycare = inventory
            .items
            .iter()
            .find(|item| item.name.as_deref() == Some("daycare"))
            .unwrap();
        assert_eq!(daycare.origin.as_deref(), Some("daycare"));
        // Daycare's own state is never part of the owner's memory.
        assert!(inventory
            .items
            .iter()
            .all(|item| !item.path.starts_with(&roots.daycare_root)));
        assert!(inventory.warnings.is_empty(), "{:?}", inventory.warnings);
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn two_scans_are_byte_identical_and_sorted() {
        let (base, roots) = populated();
        let first = serde_json::to_string(&scan(&roots)).unwrap();
        let second = serde_json::to_string(&scan(&roots)).unwrap();
        assert_eq!(first, second);
        let inventory = scan(&roots);
        let ids: Vec<&str> = inventory
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn an_edit_anywhere_in_a_skill_changes_its_hash() {
        let (base, roots) = populated();
        let before = scan(&roots);
        fs::write(
            roots.home.join(".claude/skills/review/notes.txt"),
            "edited\n",
        )
        .unwrap();
        let after = scan(&roots);
        let hash = |inventory: &Inventory| {
            inventory
                .items
                .iter()
                .find(|item| item.name.as_deref() == Some("review"))
                .unwrap()
                .sha256
                .clone()
        };
        assert_ne!(hash(&before), hash(&after));
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn an_mcp_value_change_changes_the_hash_but_not_the_listing() {
        let (base, roots) = populated();
        let before = scan(&roots);
        write(
            &roots.home.join(".claude.json"),
            r#"{"mcpServers":{"github":{"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer rotated"}}}}"#,
        );
        let after = scan(&roots);
        let github = |inventory: &Inventory| {
            inventory
                .items
                .iter()
                .find(|item| item.name.as_deref() == Some("github"))
                .unwrap()
                .clone()
        };
        assert_eq!(github(&before).id, github(&after).id);
        assert_ne!(github(&before).sha256, github(&after).sha256);
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_broken_json_file_is_a_warning_that_quotes_nothing() {
        let (base, roots) = populated();
        write(
            &roots.home.join(".claude.json"),
            "{\"secret\": \"oops-value\"",
        );
        let inventory = scan(&roots);
        let text = inventory.warnings.join("\n");
        assert!(text.contains("not valid JSON"), "{text}");
        assert!(!text.contains("oops-value"), "{text}");
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn codex_mcp_stanzas_cover_headers_inline_tables_and_dotted_keys() {
        let text = r#"
model = "gpt"
mcp_servers.rootdotted.command = "a"

[mcp_servers]
inline = { command = "b", args = ["x"] }

[mcp_servers."quoted name"]
command = "c"
args = [
  ["not", "a header"],
]
note = """
[mcp_servers.fake]
"""

[mcp_servers.plain] # comment
command = "d"

[mcp_servers.plain.env]
KEY = "v"

[[profiles.list]]
x = 1
"#;
        let stanzas = codex_mcp_stanzas(text);
        let names: Vec<&str> = stanzas.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["inline", "plain", "quoted name", "rootdotted"]);
        let plain = &stanzas.iter().find(|(name, _)| name == "plain").unwrap().1;
        assert!(plain.contains("KEY = \"v\""), "{plain}");
        assert!(!plain.contains("profiles"), "{plain}");
        let quoted = &stanzas
            .iter()
            .find(|(name, _)| name == "quoted name")
            .unwrap()
            .1;
        assert!(quoted.contains("[mcp_servers.fake]"), "{quoted}");
    }

    #[test]
    fn keys_parse_bare_and_quoted_parts() {
        assert_eq!(
            parse_key(r#"a.b."c d".'e'"#).unwrap(),
            ["a", "b", "c d", "e"]
        );
        assert_eq!(parse_key("a . b").unwrap(), ["a", "b"]);
        assert!(parse_key(r#""a", "b""#).is_none());
        assert!(parse_header(r#"["not", "a header"],"#).is_none());
    }
}
