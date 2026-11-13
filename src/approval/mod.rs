//! The owner's memory approval for challenges (plan tasks N4 and N5).
//!
//! `memory setup` is about the owner's OWN agent memory — CLAUDE.md layers,
//! AGENTS.md, rules, auto memory, skills, agents, plugins, MCP servers, for
//! Claude Code, Codex and OpenCode — and which of
//! it their agent may bring into a challenge. It is unrelated to `memory list`,
//! which reads the memories an agent kept from Daycare visits.
//!
//! The review conversation happens in the owner's own agent session (the
//! `daycare-memory-review` skill). That agent has full tools, so it must not be
//! the one that approves: it writes a draft, and only this runner writes the
//! approval file, after a typed `yes` on a real terminal. Under `--json`, with
//! no terminal, or with any answer but yes, nothing is written — so an
//! abandoned review approves nothing.
//!
//! The approval file lives at `~/.claude-daycare/approval.json`, outside every
//! challenge folder, mode 0600. Each item is bound to the sha256 the owner
//! reviewed; when the file on disk no longer matches, the approval no longer
//! covers it and `challenge start` refuses until it is reviewed again.
//! An item that is absent from the file is "don't use".

pub mod scan;

use crate::out::Out;
use crate::paths::{create_private_dir, write_atomic, Layout};
use crate::{Error, Result};
use scan::{Client, Inventory, Item, Kind, Roots};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub const APPROVAL_FILE: &str = "approval.json";
pub const APPROVAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The agent may use it to do the work, not copy it into public work.
    Private,
    /// The agent may also use it in work other people see.
    Public,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Private => "private",
            Scope::Public => "public",
        }
    }
}

/// One approved item, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedItem {
    pub id: String,
    pub client: Client,
    pub kind: Kind,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub sha256: String,
    pub scope: Scope,
}

impl ApprovedItem {
    fn from_item(item: &Item, scope: Scope) -> ApprovedItem {
        ApprovedItem {
            id: item.id.clone(),
            client: item.client,
            kind: item.kind,
            path: item.path.clone(),
            name: item.name.clone(),
            sha256: item.sha256.clone(),
            scope,
        }
    }
}

/// The approval file's schema, version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub version: u32,
    pub written_at: String,
    pub items: Vec<ApprovedItem>,
}

impl Approval {
    pub fn path(layout: &Layout) -> PathBuf {
        layout.root().join(APPROVAL_FILE)
    }

    /// `None` when there is no approval: nothing may be used.
    pub fn load(layout: &Layout) -> Result<Option<Approval>> {
        let path = Approval::path(layout);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(metadata) if !metadata.is_file() => {
                return Err(Error::new(format!(
                    "{} is not a regular file; refusing to read it as your memory approval",
                    path.display()
                )))
            }
            Ok(_) => {}
        }
        let bytes = std::fs::read(&path)?;
        let approval: Approval = serde_json::from_slice(&bytes).map_err(|_| {
            Error::new(format!(
                "{} is not a valid memory approval; run `daycare-runner memory setup` again",
                path.display()
            ))
        })?;
        if approval.version != APPROVAL_VERSION {
            return Err(Error::new(format!(
                "{} has version {}, which this runner does not understand; run `daycare-runner memory setup` again",
                path.display(),
                approval.version
            )));
        }
        Ok(Some(approval))
    }

    fn save(&self, layout: &Layout) -> Result<PathBuf> {
        create_private_dir(layout.root())?;
        let path = Approval::path(layout);
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        write_atomic(&path, text.as_bytes(), 0o600)?;
        Ok(path)
    }
}

/// An approved item whose file no longer hashes to what was approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Changed {
    pub id: String,
    pub path: PathBuf,
    pub approved_sha256: String,
    pub current_sha256: String,
}

/// The approval laid against what is on disk now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Reconciled {
    /// Approved and unchanged: these, and only these, may be used.
    pub approved: Vec<(Item, Scope)>,
    /// Approved, but edited since: not usable until reviewed again.
    pub changed: Vec<Changed>,
    /// Approved, but gone from disk.
    pub missing: Vec<ApprovedItem>,
    /// On disk, never reviewed: "don't use".
    pub unreviewed: Vec<Item>,
}

impl Reconciled {
    pub fn scope_of(&self, id: &str) -> Option<Scope> {
        self.approved
            .iter()
            .find(|(item, _)| item.id == id)
            .map(|(_, scope)| *scope)
    }

    pub fn needs_review(&self) -> bool {
        !self.changed.is_empty()
    }

    /// The refusal `challenge start` gives when memory changed after review.
    pub fn require_reviewed(&self) -> Result<()> {
        if self.changed.is_empty() {
            return Ok(());
        }
        let list = self
            .changed
            .iter()
            .map(|changed| format!("  {}", changed.path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        Err(Error::new(format!(
            "memory you approved has changed since you reviewed it, so the approval no longer \
             covers it:\n{list}\nReview it again with the daycare-memory-review skill, then run \
             `daycare-runner memory setup --write <draft>` in your own terminal"
        )))
    }
}

pub fn reconcile(approval: Option<&Approval>, inventory: &Inventory) -> Reconciled {
    let mut out = Reconciled::default();
    let mut reviewed = BTreeSet::new();
    for approved in approval.map(|a| a.items.as_slice()).unwrap_or_default() {
        reviewed.insert(approved.id.clone());
        match inventory.get(&approved.id) {
            Some(item) if item.sha256 == approved.sha256 => {
                out.approved.push((item.clone(), approved.scope))
            }
            Some(item) => out.changed.push(Changed {
                id: approved.id.clone(),
                path: approved.path.clone(),
                approved_sha256: approved.sha256.clone(),
                current_sha256: item.sha256.clone(),
            }),
            None => out.missing.push(approved.clone()),
        }
    }
    out.unreviewed = inventory
        .items
        .iter()
        .filter(|item| !reviewed.contains(&item.id))
        .cloned()
        .collect();
    out
}

/// What the review skill writes: one answer per item it asked about.
#[derive(Debug, Clone, Deserialize)]
pub struct Draft {
    #[serde(default)]
    pub version: Option<u32>,
    pub items: Vec<DraftItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DraftItem {
    pub id: String,
    /// The hash the owner reviewed. When given, it must still match.
    #[serde(default)]
    pub sha256: Option<String>,
    pub scope: DraftScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftScope {
    None,
    Private,
    Public,
}

/// Turn a draft into the items to approve, against a fresh scan. Refuses an
/// unknown id, a repeated id, or an item edited since the draft was written.
pub fn plan_from_draft(draft: &Draft, inventory: &Inventory) -> Result<Vec<ApprovedItem>> {
    if let Some(version) = draft.version {
        if version != APPROVAL_VERSION {
            return Err(Error::new(format!(
                "the draft has version {version}; this runner writes version {APPROVAL_VERSION}"
            )));
        }
    }
    let mut seen = BTreeSet::new();
    let mut items = Vec::new();
    for answer in &draft.items {
        if !seen.insert(answer.id.as_str()) {
            return Err(Error::new(format!(
                "the draft answers {} twice; nothing was saved",
                answer.id
            )));
        }
        let item = inventory.get(&answer.id).ok_or_else(|| {
            Error::new(format!(
                "the draft names {}, which the scan does not find; run `memory setup --scan` \
                 and review again. Nothing was saved",
                answer.id
            ))
        })?;
        if let Some(sha256) = &answer.sha256 {
            if sha256 != &item.sha256 {
                return Err(Error::new(format!(
                    "{} changed after it was reviewed; review it again. Nothing was saved",
                    item.path.display()
                )));
            }
        }
        let scope = match answer.scope {
            DraftScope::None => continue,
            DraftScope::Private => Scope::Private,
            DraftScope::Public => Scope::Public,
        };
        items.push(ApprovedItem::from_item(item, scope));
    }
    items.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(items)
}

/// "Accept all": every item except Daycare's own skills, at one scope.
pub fn plan_accept_all(inventory: &Inventory, scope: Scope) -> Vec<ApprovedItem> {
    inventory
        .items
        .iter()
        .filter(|item| item.origin.as_deref() != Some("daycare"))
        .map(|item| ApprovedItem::from_item(item, scope))
        .collect()
}

/// Where a typed answer comes from. The terminal in the CLI; a script in tests.
pub trait Prompt {
    /// `None` when the person left (end of input).
    fn ask(&mut self, question: &str) -> Option<String>;
}

/// Reads answers from this process's terminal.
pub struct TerminalPrompt;

impl Prompt for TerminalPrompt {
    fn ask(&mut self, question: &str) -> Option<String> {
        print!("{question}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

fn is_yes(answer: &Option<String>) -> bool {
    answer
        .as_deref()
        .is_some_and(|text| text.trim().eq_ignore_ascii_case("yes"))
}

fn is_no(answer: &Option<String>) -> bool {
    answer
        .as_deref()
        .is_some_and(|text| text.trim().eq_ignore_ascii_case("no"))
}

/// Steps that need a person's typed yes refuse `--json` and refuse to run
/// without a terminal on both stdin and stdout. A guard rail, not proof: a
/// program can fake a terminal (the usage meter does, for its own reasons).
pub fn require_terminal(json: bool, what: &str) -> Result<()> {
    if json {
        return Err(Error::new(format!(
            "{what} needs your typed yes, so it does not run under --json. Run it in your own \
             terminal without --json. Nothing was saved"
        )));
    }
    // SAFETY: isatty has no preconditions.
    let terminal = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    if !terminal {
        return Err(Error::new(format!(
            "{what} needs your typed yes on a real terminal, and this is not one. Run it \
             yourself in a terminal window. Nothing was saved"
        )));
    }
    Ok(())
}

/// Show what will be approved, ask for `yes`, re-check the hashes, write.
/// Anything but a typed yes leaves no file behind.
pub fn confirm_and_write(
    layout: &Layout,
    roots: &Roots,
    items: Vec<ApprovedItem>,
    written_at: &str,
    prompt: &mut dyn Prompt,
    out: Out,
) -> Result<PathBuf> {
    print_plan(&items, out);
    let answer = prompt.ask("Type yes to save this approval: ");
    if !is_yes(&answer) {
        return Err(not_saved(&answer));
    }
    write_checked(layout, roots, items, written_at)
}

/// "Accept all" asks twice: private use first, then, as its own question,
/// public use. Any answer to the second question but yes or no saves nothing.
pub fn accept_all(
    layout: &Layout,
    roots: &Roots,
    written_at: &str,
    prompt: &mut dyn Prompt,
    out: Out,
) -> Result<(PathBuf, Scope)> {
    let inventory = scan::scan(roots);
    let items = plan_accept_all(&inventory, Scope::Private);
    out.say(format!(
        "Accept all covers these {} items for private use: your agent may use them to do the \
         work, but not copy them into anything other people see.",
        items.len()
    ));
    for item in &items {
        out.say(format!(
            "  {}",
            describe_parts(item.client, item.kind, item.name.as_deref(), &item.path)
        ));
    }
    let skipped = inventory.items.len() - items.len();
    if skipped > 0 {
        out.say(format!(
            "Daycare's own skills ({skipped}) are left out; they do nothing inside a challenge."
        ));
    }
    let private = prompt.ask("Type yes to allow private use of all of them: ");
    if !is_yes(&private) {
        return Err(not_saved(&private));
    }
    let public = prompt.ask(
        "Should all of them also be allowed in public work, such as entries other people can \
         read? Type yes or no: ",
    );
    let scope = if is_yes(&public) {
        Scope::Public
    } else if is_no(&public) {
        Scope::Private
    } else {
        return Err(not_saved(&public));
    };
    let items = plan_accept_all(&inventory, scope);
    Ok((write_checked(layout, roots, items, written_at)?, scope))
}

fn not_saved(answer: &Option<String>) -> Error {
    match answer {
        None => Error::new("the review was left before it was confirmed. Nothing was saved"),
        Some(_) => Error::new("not confirmed. Nothing was saved"),
    }
}

/// The scan is repeated after the yes, so a file edited while the question
/// was on screen is not approved at its new content.
fn write_checked(
    layout: &Layout,
    roots: &Roots,
    items: Vec<ApprovedItem>,
    written_at: &str,
) -> Result<PathBuf> {
    let now = scan::scan(roots);
    for item in &items {
        match now.get(&item.id) {
            Some(current) if current.sha256 == item.sha256 => {}
            _ => {
                return Err(Error::new(format!(
                    "{} changed while you were confirming; review it again. Nothing was saved",
                    item.path.display()
                )))
            }
        }
    }
    let approval = Approval {
        version: APPROVAL_VERSION,
        written_at: written_at.to_string(),
        items,
    };
    approval.save(layout)
}

fn describe(item: &Item) -> String {
    describe_parts(item.client, item.kind, item.name.as_deref(), &item.path)
}

fn describe_parts(client: Client, kind: Kind, name: Option<&str>, path: &Path) -> String {
    let what = kind.as_str().replace('_', " ");
    match name {
        Some(name) => format!("{}, {what}, {name} ({})", client.as_str(), path.display()),
        None => format!("{}, {what}, {}", client.as_str(), path.display()),
    }
}

/// The project an auto memory file belongs to: the folder Claude Code names
/// after the project's path, in `<config>/projects/<project>/memory/…`.
fn auto_memory_project(kind: Kind, path: &Path) -> Option<String> {
    if kind != Kind::AutoMemory {
        return None;
    }
    let parts: Vec<String> = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    (0..parts.len().saturating_sub(2))
        .find(|&at| parts[at] == "projects" && parts[at + 2] == "memory")
        .map(|at| parts[at + 1].clone())
}

/// Auto memory files counted by project, in project order. A machine can hold
/// hundreds of them, so the review takes them as one item, not one per file.
fn auto_memory_projects<'a>(
    items: impl IntoIterator<Item = (Kind, &'a Path)>,
) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (kind, path) in items {
        if let Some(project) = auto_memory_project(kind, path) {
            *counts.entry(project).or_default() += 1;
        }
    }
    counts.into_iter().collect()
}

/// The scan's auto memory, summed up for the review skill.
fn auto_memory_summary(inventory: &Inventory) -> Value {
    let projects = auto_memory_projects(
        inventory
            .items
            .iter()
            .map(|item| (item.kind, item.path.as_path())),
    );
    json!({
        "files": projects.iter().map(|(_, files)| files).sum::<usize>(),
        "projects": projects
            .iter()
            .map(|(project, files)| json!({"project": project, "files": files}))
            .collect::<Vec<_>>(),
    })
}

fn files(count: usize) -> String {
    if count == 1 {
        "1 file".to_string()
    } else {
        format!("{count} files")
    }
}

/// What the owner is asked to say yes to, auto memory counted by project.
fn plan_lines(items: &[ApprovedItem]) -> Vec<String> {
    if items.is_empty() {
        return vec![
            "Nothing is approved: your agent will start every challenge with none of your memory."
                .to_string(),
        ];
    }
    let mut lines = vec!["Your agent may use these in challenges:".to_string()];
    for scope in [Scope::Private, Scope::Public] {
        let chosen: Vec<_> = items.iter().filter(|item| item.scope == scope).collect();
        if chosen.is_empty() {
            continue;
        }
        lines.push(
            match scope {
                Scope::Private => "  Private (to do the work, never copied into public work):",
                Scope::Public => "  Public (may also appear in work other people see):",
            }
            .to_string(),
        );
        for item in chosen.iter().filter(|item| item.kind != Kind::AutoMemory) {
            let what = item.kind.as_str().replace('_', " ");
            lines.push(match &item.name {
                Some(name) => format!("    {what}: {name}"),
                None => format!("    {what}: {}", item.path.display()),
            });
        }
        for (project, count) in
            auto_memory_projects(chosen.iter().map(|item| (item.kind, item.path.as_path())))
        {
            lines.push(format!("    auto memory: {} from {project}", files(count)));
        }
    }
    lines.push("Everything else is \"don't use\".".to_string());
    lines
}

fn print_plan(items: &[ApprovedItem], out: Out) {
    for line in plan_lines(items) {
        out.say(line);
    }
}

/// How `memory setup` was asked to run.
#[derive(Debug, Clone)]
pub enum SetupMode {
    Scan,
    Write(PathBuf),
    AcceptAll,
    Check,
}

/// The `memory setup` command.
pub fn setup_command(
    layout: &Layout,
    roots: &Roots,
    mode: SetupMode,
    written_at: &str,
    out: Out,
) -> Result<()> {
    match mode {
        SetupMode::Scan => {
            let inventory = scan::scan(roots);
            out.emit(
                json!({
                    "ok": true,
                    "items": inventory.items,
                    "auto_memory": auto_memory_summary(&inventory),
                    "warnings": inventory.warnings,
                }),
                || print_inventory(&inventory),
            );
            Ok(())
        }
        SetupMode::Write(draft_path) => {
            require_terminal(out.json, "memory setup --write")?;
            let draft = read_draft(&draft_path)?;
            let inventory = scan::scan(roots);
            let items = plan_from_draft(&draft, &inventory)?;
            let path =
                confirm_and_write(layout, roots, items, written_at, &mut TerminalPrompt, out)?;
            out.say(format!("Saved your memory approval to {}.", path.display()));
            Ok(())
        }
        SetupMode::AcceptAll => {
            require_terminal(out.json, "memory setup --accept-all")?;
            let (path, scope) = accept_all(layout, roots, written_at, &mut TerminalPrompt, out)?;
            out.say(format!(
                "Saved: everything approved for {} use, in {}.",
                scope.as_str(),
                path.display()
            ));
            Ok(())
        }
        SetupMode::Check => {
            let approval = Approval::load(layout)?;
            let inventory = scan::scan(roots);
            let reconciled = reconcile(approval.as_ref(), &inventory);
            // Changed hashes are a failure: the approval no longer covers them.
            reconciled.require_reviewed()?;
            let approved: Vec<_> = reconciled
                .approved
                .iter()
                .map(|(item, scope)| json!({ "id": item.id, "scope": scope }))
                .collect();
            let unreviewed: Vec<&str> = reconciled
                .unreviewed
                .iter()
                .map(|item| item.id.as_str())
                .collect();
            let missing: Vec<&str> = reconciled
                .missing
                .iter()
                .map(|item| item.id.as_str())
                .collect();
            out.emit(
                json!({
                    "ok": true,
                    "approval_file": approval.as_ref().map(|_| Approval::path(layout)),
                    "written_at": approval.as_ref().map(|a| a.written_at.clone()),
                    "approved": approved,
                    "changed": [],
                    "missing": missing,
                    "unreviewed": unreviewed,
                }),
                || {
                    match &approval {
                        None => println!("No memory approval yet: nothing will be used."),
                        Some(approval) => println!(
                            "Approval from {}: {} item(s) approved and unchanged.",
                            approval.written_at,
                            reconciled.approved.len()
                        ),
                    }
                    if !reconciled.missing.is_empty() {
                        println!(
                            "{} approved item(s) are gone from disk and will not be used.",
                            reconciled.missing.len()
                        );
                    }
                    if !reconciled.unreviewed.is_empty() {
                        println!(
                            "{} item(s) were never reviewed and will not be used.",
                            reconciled.unreviewed.len()
                        );
                    }
                },
            );
            Ok(())
        }
    }
}

fn read_draft(path: &Path) -> Result<Draft> {
    let bytes = std::fs::read(path)
        .map_err(|error| Error::new(format!("could not read {}: {error}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        Error::new(format!(
            "{} is not a memory review draft ({error}). Nothing was saved",
            path.display()
        ))
    })
}

/// The scan as the owner reads it: one line per item, except auto memory,
/// which is one item with a line per project.
fn inventory_lines(inventory: &Inventory) -> Vec<String> {
    let mut lines = Vec::new();
    if inventory.items.is_empty() {
        lines.push("No memory found for Claude Code, Codex or OpenCode.".to_string());
    }
    let mut by_client: BTreeMap<&str, Vec<&Item>> = BTreeMap::new();
    for item in &inventory.items {
        by_client
            .entry(item.client.as_str())
            .or_default()
            .push(item);
    }
    for (client, items) in by_client {
        lines.push(format!("{client}:"));
        for item in items.iter().filter(|item| item.kind != Kind::AutoMemory) {
            let origin = match item.origin.as_deref() {
                Some(origin) => format!(" [{origin}]"),
                None => String::new(),
            };
            lines.push(format!("  {}{origin}", describe(item)));
        }
        let projects =
            auto_memory_projects(items.iter().map(|item| (item.kind, item.path.as_path())));
        if !projects.is_empty() {
            let total: usize = projects.iter().map(|(_, count)| count).sum();
            lines.push(format!(
                "  {client}, auto memory, {} in {} project(s), reviewed as one item:",
                files(total),
                projects.len()
            ));
            for (project, count) in &projects {
                lines.push(format!("    {project}: {}", files(*count)));
            }
        }
    }
    for warning in &inventory.warnings {
        lines.push(format!("note: {warning}"));
    }
    lines.push(
        "These are your own agent memory, not Daycare memories. Review them with the \
         daycare-memory-review skill."
            .to_string(),
    );
    lines
}

fn print_inventory(inventory: &Inventory) {
    for line in inventory_lines(inventory) {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::unique_dir;
    use std::fs;

    struct Scripted(Vec<Option<&'static str>>);

    impl Prompt for Scripted {
        fn ask(&mut self, _question: &str) -> Option<String> {
            if self.0.is_empty() {
                return None;
            }
            self.0.remove(0).map(str::to_string)
        }
    }

    fn quiet() -> Out {
        Out::new(false).inner()
    }

    fn machine() -> (PathBuf, Layout, Roots) {
        let base = unique_dir("daycare-approval");
        let home = base.join("home");
        let layout = Layout::at(home.join(".claude-daycare"));
        let roots = Roots {
            claude_config_dir: home.join(".claude"),
            claude_json: home.join(".claude.json"),
            codex_home: home.join(".codex"),
            agents_home: home.join(".agents"),
            opencode_config_dir: home.join(".config/opencode"),
            opencode_dot_dir: home.join(".opencode"),
            challenge_root: home.join("daycare/challenges"),
            daycare_root: layout.root().to_path_buf(),
            home: home.clone(),
        };
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/CLAUDE.md"), "be terse\n").unwrap();
        fs::create_dir_all(home.join(".claude/skills/review")).unwrap();
        fs::write(
            home.join(".claude/skills/review/SKILL.md"),
            "---\nname: review\n---\n",
        )
        .unwrap();
        (base, layout, roots)
    }

    fn draft_for(inventory: &Inventory, scope: &str) -> Draft {
        serde_json::from_value(json!({
            "version": 1,
            "items": inventory.items.iter().map(|item| json!({
                "id": item.id, "sha256": item.sha256, "scope": scope,
            })).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn listed(client: Client, kind: Kind, path: &str, name: Option<&str>) -> Item {
        Item {
            id: scan::item_id(client, kind, Path::new(path), name),
            client,
            kind,
            path: PathBuf::from(path),
            name: name.map(str::to_string),
            bytes: 1,
            sha256: "0".repeat(64),
            origin: None,
        }
    }

    fn many_auto_memories() -> Inventory {
        let mut items = vec![
            listed(Client::Claude, Kind::ClaudeMd, "/h/.claude/CLAUDE.md", None),
            listed(
                Client::Codex,
                Kind::Skill,
                "/h/.codex/skills/notes",
                Some("notes"),
            ),
        ];
        for at in 0..30 {
            items.push(listed(
                Client::Claude,
                Kind::AutoMemory,
                &format!("/h/.claude/projects/-h-site/memory/note-{at}.md"),
                None,
            ));
        }
        items.push(listed(
            Client::Claude,
            Kind::AutoMemory,
            "/h/.claude/projects/-h-app/memory/projects/MEMORY.md",
            None,
        ));
        Inventory {
            items,
            warnings: vec![],
        }
    }

    #[test]
    fn auto_memory_is_one_item_with_a_line_per_project() {
        let lines = inventory_lines(&many_auto_memories());
        assert_eq!(
            lines,
            vec![
                "claude:",
                "  claude, claude md, /h/.claude/CLAUDE.md",
                "  claude, auto memory, 31 files in 2 project(s), reviewed as one item:",
                "    -h-app: 1 file",
                "    -h-site: 30 files",
                "codex:",
                "  codex, skill, notes (/h/.codex/skills/notes)",
                "These are your own agent memory, not Daycare memories. Review them with the \
                 daycare-memory-review skill.",
            ]
        );
        assert_eq!(
            auto_memory_summary(&many_auto_memories()),
            json!({"files": 31, "projects": [
                {"project": "-h-app", "files": 1},
                {"project": "-h-site", "files": 30},
            ]})
        );
        // A folder named "projects" inside a memory tree is not a project.
        assert_eq!(
            auto_memory_project(
                Kind::AutoMemory,
                Path::new("/h/.claude/projects/-h-app/memory/projects/MEMORY.md")
            ),
            Some("-h-app".to_string())
        );
        assert_eq!(
            auto_memory_project(
                Kind::ClaudeMd,
                Path::new("/h/.claude/projects/p/memory/a.md")
            ),
            None
        );
    }

    #[test]
    fn the_list_before_the_yes_counts_auto_memory_by_project() {
        let inventory = many_auto_memories();
        let mut items: Vec<ApprovedItem> = inventory
            .items
            .iter()
            .filter(|item| item.kind == Kind::AutoMemory)
            .map(|item| ApprovedItem::from_item(item, Scope::Private))
            .collect();
        items.push(ApprovedItem::from_item(&inventory.items[0], Scope::Public));
        assert_eq!(
            plan_lines(&items),
            vec![
                "Your agent may use these in challenges:",
                "  Private (to do the work, never copied into public work):",
                "    auto memory: 1 file from -h-app",
                "    auto memory: 30 files from -h-site",
                "  Public (may also appear in work other people see):",
                "    claude md: /h/.claude/CLAUDE.md",
                "Everything else is \"don't use\".",
            ]
        );
    }

    #[test]
    fn an_abandoned_review_writes_no_file() {
        let (base, layout, roots) = machine();
        let inventory = scan::scan(&roots);
        let items = plan_from_draft(&draft_for(&inventory, "private"), &inventory).unwrap();
        for answers in [
            vec![None],
            vec![Some("no\n")],
            vec![Some("y\n")],
            vec![Some("\n")],
        ] {
            let error = confirm_and_write(
                &layout,
                &roots,
                items.clone(),
                "2026-11-06T12:00:00Z",
                &mut Scripted(answers),
                quiet(),
            )
            .unwrap_err();
            assert!(error.message().contains("Nothing was saved"), "{error}");
            assert!(!Approval::path(&layout).exists());
        }
        // Accept-all left at either question approves nothing either.
        for answers in [
            vec![None],
            vec![Some("yes\n"), None],
            vec![Some("yes\n"), Some("maybe\n")],
        ] {
            assert!(accept_all(
                &layout,
                &roots,
                "2026-11-06T12:00:00Z",
                &mut Scripted(answers),
                quiet()
            )
            .is_err());
            assert!(!Approval::path(&layout).exists());
        }
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_typed_yes_writes_an_owner_only_file_that_round_trips() {
        let (base, layout, roots) = machine();
        let inventory = scan::scan(&roots);
        let items = plan_from_draft(&draft_for(&inventory, "public"), &inventory).unwrap();
        let path = confirm_and_write(
            &layout,
            &roots,
            items.clone(),
            "2026-11-06T12:00:00Z",
            &mut Scripted(vec![Some("YES \n")]),
            quiet(),
        )
        .unwrap();
        assert_eq!(path, Approval::path(&layout));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let loaded = Approval::load(&layout).unwrap().unwrap();
        assert_eq!(loaded.items, items);
        assert_eq!(loaded.version, APPROVAL_VERSION);
        let reconciled = reconcile(Some(&loaded), &inventory);
        assert_eq!(reconciled.approved.len(), inventory.items.len());
        assert!(reconciled.unreviewed.is_empty());
        assert!(reconciled.require_reviewed().is_ok());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_changed_hash_forces_a_fresh_review() {
        let (base, layout, roots) = machine();
        let inventory = scan::scan(&roots);
        let items = plan_from_draft(&draft_for(&inventory, "private"), &inventory).unwrap();
        confirm_and_write(
            &layout,
            &roots,
            items,
            "2026-11-06T12:00:00Z",
            &mut Scripted(vec![Some("yes")]),
            quiet(),
        )
        .unwrap();
        fs::write(
            roots.home.join(".claude/CLAUDE.md"),
            "be terse; also leak\n",
        )
        .unwrap();
        let after = scan::scan(&roots);
        let reconciled = reconcile(Approval::load(&layout).unwrap().as_ref(), &after);
        assert!(reconciled.needs_review());
        assert_eq!(reconciled.changed.len(), 1);
        assert_eq!(
            reconciled.changed[0].path,
            roots.home.join(".claude/CLAUDE.md")
        );
        // The edited file is not usable; the untouched skill still is.
        assert_eq!(reconciled.approved.len(), 1);
        assert_eq!(reconciled.approved[0].0.kind, Kind::Skill);
        let error = reconciled.require_reviewed().unwrap_err();
        assert!(error.message().contains("CLAUDE.md"), "{error}");
        // And an old draft for the old content is refused too.
        let stale = draft_for(&inventory, "private");
        assert!(plan_from_draft(&stale, &after)
            .unwrap_err()
            .message()
            .contains("changed after it was reviewed"));
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn a_file_edited_during_the_question_is_not_approved() {
        let (base, layout, roots) = machine();
        let inventory = scan::scan(&roots);
        let items = plan_from_draft(&draft_for(&inventory, "private"), &inventory).unwrap();
        struct EditThenYes(PathBuf);
        impl Prompt for EditThenYes {
            fn ask(&mut self, _question: &str) -> Option<String> {
                fs::write(&self.0, "edited while asking\n").unwrap();
                Some("yes".into())
            }
        }
        let error = confirm_and_write(
            &layout,
            &roots,
            items,
            "2026-11-06T12:00:00Z",
            &mut EditThenYes(roots.home.join(".claude/CLAUDE.md")),
            quiet(),
        )
        .unwrap_err();
        assert!(error
            .message()
            .contains("changed while you were confirming"));
        assert!(!Approval::path(&layout).exists());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn draft_answers_map_to_three_states_and_unanswered_means_dont_use() {
        let (base, _layout, roots) = machine();
        let inventory = scan::scan(&roots);
        let claude_md = inventory
            .items
            .iter()
            .find(|item| item.kind == Kind::ClaudeMd)
            .unwrap();
        let skill = inventory
            .items
            .iter()
            .find(|item| item.kind == Kind::Skill)
            .unwrap();
        let draft: Draft = serde_json::from_value(json!({"items": [
            {"id": claude_md.id, "scope": "public"},
            {"id": skill.id, "scope": "none"},
        ]}))
        .unwrap();
        let items = plan_from_draft(&draft, &inventory).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].scope, Scope::Public);

        let unknown: Draft = serde_json::from_value(
            json!({"items": [{"id": "claude:skill:/nope", "scope": "private"}]}),
        )
        .unwrap();
        assert!(plan_from_draft(&unknown, &inventory).is_err());
        let twice: Draft = serde_json::from_value(json!({"items": [
            {"id": claude_md.id, "scope": "private"},
            {"id": claude_md.id, "scope": "public"},
        ]}))
        .unwrap();
        assert!(plan_from_draft(&twice, &inventory).is_err());
        let bad_scope = serde_json::from_value::<Draft>(
            json!({"items": [{"id": claude_md.id, "scope": "everything"}]}),
        );
        assert!(bad_scope.is_err());
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn accept_all_is_private_unless_the_second_question_says_public() {
        let (base, layout, roots) = machine();
        let (_, scope) = accept_all(
            &layout,
            &roots,
            "t",
            &mut Scripted(vec![Some("yes"), Some("no")]),
            quiet(),
        )
        .unwrap();
        assert_eq!(scope, Scope::Private);
        let approval = Approval::load(&layout).unwrap().unwrap();
        assert!(approval
            .items
            .iter()
            .all(|item| item.scope == Scope::Private));
        let (_, scope) = accept_all(
            &layout,
            &roots,
            "t",
            &mut Scripted(vec![Some("yes"), Some("yes")]),
            quiet(),
        )
        .unwrap();
        assert_eq!(scope, Scope::Public);
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn accept_all_leaves_out_daycares_own_skills() {
        let (base, layout, roots) = machine();
        fs::create_dir_all(roots.home.join(".claude/skills/daycare")).unwrap();
        fs::write(
            roots.home.join(".claude/skills/daycare/SKILL.md"),
            crate::skills::DAYCARE.markdown,
        )
        .unwrap();
        accept_all(
            &layout,
            &roots,
            "t",
            &mut Scripted(vec![Some("yes"), Some("no")]),
            quiet(),
        )
        .unwrap();
        let approval = Approval::load(&layout).unwrap().unwrap();
        assert!(approval
            .items
            .iter()
            .all(|item| item.name.as_deref() != Some("daycare")));
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn typed_yes_steps_refuse_json() {
        let error = require_terminal(true, "memory setup --write").unwrap_err();
        assert!(error.message().contains("--json"));
        assert!(error.message().contains("Nothing was saved"));
    }

    #[test]
    fn a_non_file_approval_is_refused_and_an_unknown_version_is_not_guessed() {
        let (base, layout, _roots) = machine();
        fs::create_dir_all(Approval::path(&layout)).unwrap();
        assert!(Approval::load(&layout).is_err());
        fs::remove_dir_all(Approval::path(&layout)).unwrap();
        fs::write(
            Approval::path(&layout),
            r#"{"version":9,"written_at":"t","items":[]}"#,
        )
        .unwrap();
        assert!(Approval::load(&layout)
            .unwrap_err()
            .message()
            .contains("version 9"));
        fs::remove_dir_all(base).ok();
    }
}
