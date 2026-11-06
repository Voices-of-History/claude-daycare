//! `daycare-runner challenge`: list, show, start (plan tasks N6–N8, N12, N13).
//!
//! A challenge is work the owner's agent does on the owner's machine, with the
//! owner's own client and tools, in a folder Daycare sets up. Daycare never
//! starts that session itself: `challenge start --print` sets up the folder
//! and prints the exact command (and environment) for the owner to run.
//!
//! `start` does its checks in an order that leaves nothing behind on failure:
//! the session marker, the client's policy guard, the memory approval (a
//! changed hash refuses), the challenge record, every input file downloaded
//! and checked, and only then the enrollment call and the folder itself.
//!
//! Every platform call goes through `PlatformClient` with the identity token,
//! so the release header is sent and a 426 makes `main` update once and run
//! the same command again.

pub mod exclusion;
pub mod folder;

use crate::approval::scan::{self as memory_scan, Client, Kind, Roots};
use crate::approval::{reconcile, Approval};
use crate::out::Out;
use crate::paths::{create_private_dir, sanitize_segment, write_atomic, Layout};
use crate::platform::{download_signed, PlatformClient};
use crate::session::Active;
use crate::{Error, Result};
use exclusion::{PlanInput, SESSION_MARKER_ENV};
use folder::{ChallengeDetail, FolderSpec};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The entry id when this process runs inside a challenge session.
pub fn inside_session() -> Option<String> {
    std::env::var(SESSION_MARKER_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Commands that act for the owner refuse to run from inside a challenge
/// session, before any request. A guard rail only: the marker can be unset by
/// a process that wants to, and that process is outside the boundary anyway.
pub fn refuse_inside_session(command: &str) -> Result<()> {
    match inside_session() {
        Some(entry) => Err(Error::new(format!(
            "`daycare-runner {command}` does not run inside a challenge session (entry {entry}). \
             Your owner runs it in their own terminal, outside the challenge"
        ))),
        None => Ok(()),
    }
}

/// `challenge list`.
pub fn list(active: &Active, out: Out) -> Result<()> {
    let client = PlatformClient::new(&active.platform_url);
    let value = client.list_challenges(active.token())?;
    let list = value
        .get("challenges")
        .cloned()
        .unwrap_or(value)
        .as_array()
        .cloned()
        .ok_or_else(|| Error::new("the challenge list was not a list"))?;
    let summaries: Vec<Value> = list
        .iter()
        .map(|challenge| {
            json!({
                "id": challenge.get("id"),
                "slug": challenge.get("slug"),
                "title": challenge.get("title"),
                "state": challenge.get("state").or_else(|| challenge.get("phase")),
                "closes_at": challenge.get("closes_at"),
            })
        })
        .collect();
    out.emit(json!({ "ok": true, "challenges": summaries }), || {
        if summaries.is_empty() {
            println!("No open challenges right now.");
        }
        for challenge in &summaries {
            let field = |key: &str| challenge[key].as_str().unwrap_or("").to_string();
            let closes = match challenge["closes_at"].as_str() {
                Some(at) => format!(", closes {at}"),
                None => String::new(),
            };
            println!("{}  {}{closes}", field("id"), field("title"));
        }
    });
    Ok(())
}

fn fetch_detail(active: &Active, challenge_id: &str) -> Result<ChallengeDetail> {
    folder::validate_challenge_id(challenge_id)?;
    let client = PlatformClient::new(&active.platform_url);
    ChallengeDetail::parse(&client.get_challenge(active.token(), challenge_id)?)
}

/// `challenge show <id>`. Download links are never printed.
pub fn show(active: &Active, challenge_id: &str, out: Out) -> Result<()> {
    let detail = fetch_detail(active, challenge_id)?;
    out.emit(
        json!({ "ok": true, "challenge": detail.public_json() }),
        || print!("{}", folder::brief_text(&detail)),
    );
    Ok(())
}

/// `challenge submit`: Round 2 (plan task N14). Present now so the session
/// marker already guards it.
pub fn submit() -> Result<()> {
    refuse_inside_session("challenge submit")?;
    Err(Error::new(
        "submitting from the runner is not built yet. Upload your output/ folder from the \
         challenge's page on the Daycare site",
    ))
}

/// What `challenge start` was asked to do.
pub struct StartRequest<'a> {
    pub layout: &'a Layout,
    pub active: &'a Active,
    pub challenge_id: &'a str,
    pub client: Client,
    pub claude_bin: &'a str,
    pub codex_bin: &'a str,
    pub started_at: &'a str,
}

/// Codex reads managed and system configuration that no flag can leave out.
/// Unlike the visit seal, the owner's own `config.toml` is expected here.
fn guard_no_managed_codex(codex_home: &Path) -> Result<()> {
    let mut sources = vec![
        PathBuf::from("/etc/codex/requirements.toml"),
        PathBuf::from("/etc/codex/managed_config.toml"),
        PathBuf::from("/etc/codex/config.toml"),
        codex_home.join("requirements.toml"),
        codex_home.join("managed_config.toml"),
    ];
    if cfg!(target_os = "macos") {
        sources.push(PathBuf::from(
            "/Library/Managed Preferences/com.openai.codex.plist",
        ));
    }
    for path in sources {
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(Error::new(format!(
                "Codex on this machine is configured by {}; Daycare cannot leave managed \
                 configuration out of a challenge",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Approved Claude MCP stanzas, read from `~/.claude.json` at start time and
/// checked against the approved hash. Returned for the generated MCP config;
/// never printed.
fn approved_claude_mcp(
    roots: &Roots,
    approved: &[(memory_scan::Item, crate::approval::Scope)],
) -> Result<BTreeMap<String, Value>> {
    let wanted: Vec<&memory_scan::Item> = approved
        .iter()
        .map(|(item, _)| item)
        .filter(|item| item.client == Client::Claude && item.kind == Kind::McpServer)
        .collect();
    let mut stanzas = BTreeMap::new();
    if wanted.is_empty() {
        return Ok(stanzas);
    }
    let bytes = std::fs::read(&roots.claude_json)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        Error::new(format!(
            "{} is not valid JSON; cannot bring its MCP servers",
            roots.claude_json.display()
        ))
    })?;
    for item in wanted {
        let name = item.name.clone().unwrap_or_default();
        let stanza = value["mcpServers"].get(&name).cloned().ok_or_else(|| {
            Error::new(format!(
                "the MCP server {name} is gone from {}",
                roots.claude_json.display()
            ))
        })?;
        if crate::self_update::sha256_hex(stanza.to_string().as_bytes()) != item.sha256 {
            return Err(Error::new(format!(
                "the MCP server {name} changed after you approved it; review it again"
            )));
        }
        stanzas.insert(name, stanza);
    }
    Ok(stanzas)
}

fn inherited_daycare_env() -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| name.starts_with("DAYCARE_"))
        .collect();
    names.sort();
    names
}

/// `challenge start <id> --print`.
pub fn start(request: StartRequest<'_>, out: Out) -> Result<()> {
    refuse_inside_session("challenge start")?;
    let StartRequest {
        layout,
        active,
        challenge_id,
        client,
        claude_bin,
        codex_bin,
        started_at,
    } = request;
    folder::validate_challenge_id(challenge_id)?;
    let challenge_root = folder::challenge_root()?;
    let roots = Roots::discover(layout.root(), &challenge_root)?;

    match client {
        Client::Claude => crate::workspace::guard_no_managed_claude(claude_bin)?,
        Client::Codex => guard_no_managed_codex(&roots.codex_home)?,
    }

    // Memory first: a changed approval stops here, before any request.
    let inventory = memory_scan::scan(&roots);
    let approval = Approval::load(layout)?;
    let reconciled = reconcile(approval.as_ref(), &inventory);
    reconciled.require_reviewed()?;
    let approved_mcp = match client {
        Client::Claude => approved_claude_mcp(&roots, &reconciled.approved)?,
        Client::Codex => BTreeMap::new(),
    };
    let inherited = inherited_daycare_env();
    let program = match client {
        Client::Claude => claude_bin,
        Client::Codex => codex_bin,
    };

    // A Codex refusal (memory it cannot leave out) is known before the
    // network, so check it with a placeholder folder and entry first.
    let placeholder = challenge_root.join(sanitize_segment(challenge_id));
    if client == Client::Codex {
        exclusion::codex_plan(&PlanInput {
            inventory: &inventory,
            reconciled: &reconciled,
            folder: &placeholder,
            home: &roots.home,
            claude_config_dir: &roots.claude_config_dir,
            daycare_root: layout.root(),
            entry_id: "pending",
            program,
            inherited_daycare_env: &inherited,
        })?;
    }

    let detail = fetch_detail(active, challenge_id)?;
    if let Some(state) = &detail.state {
        if state != "open" {
            return Err(Error::new(format!(
                "{} is {state}, not open; it cannot be started",
                detail.title
            )));
        }
    }
    // Every input is downloaded and checked before the entry exists or a
    // single file is written.
    let staged = detail
        .inputs
        .iter()
        .map(|input| folder::stage_input(input, &download_signed))
        .collect::<Result<Vec<_>>>()?;

    let platform = PlatformClient::new(&active.platform_url);
    let attempt =
        platform.start_attempt(active.token(), &detail.id, &active.identity.identity_id)?;
    let entry_id = attempt
        .get("entry_id")
        .or_else(|| attempt.get("entry").and_then(|entry| entry.get("id")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::new("challenge start response had no entry_id"))?;
    if sanitize_segment(&entry_id) != entry_id {
        return Err(Error::new(
            "challenge start returned an entry id this runner cannot use",
        ));
    }

    folder::ensure_root(&challenge_root)?;
    let challenge_folder = challenge_root.join(sanitize_segment(&detail.id));
    let entry = json!({
        "version": 1,
        "challenge_id": detail.id,
        "challenge_slug": detail.slug,
        "title": detail.title,
        "entry_id": entry_id,
        "actor_id": active.identity.identity_id,
        "identity_name": active.identity.name,
        "client": client,
        "started_at": started_at,
        "folder": challenge_folder,
        "approval_written_at": approval.as_ref().map(|a| a.written_at.clone()),
    });
    let copies = folder::write_folder(&FolderSpec {
        folder: &challenge_folder,
        client,
        detail: &detail,
        inputs: &staged,
        approved: &reconciled.approved,
        entry,
    })?;

    // Generated client files live under Daycare's own folder, outside the
    // challenge folder, where the session's deny rules cover them.
    let private = layout
        .root()
        .join("challenges")
        .join(sanitize_segment(&detail.id));
    create_private_dir(layout.root())?;
    create_private_dir(&layout.root().join("challenges"))?;
    create_private_dir(&private)?;

    let plan_input = PlanInput {
        inventory: &inventory,
        reconciled: &reconciled,
        folder: &challenge_folder,
        home: &roots.home,
        claude_config_dir: &roots.claude_config_dir,
        daycare_root: layout.root(),
        entry_id: &entry_id,
        program,
        inherited_daycare_env: &inherited,
    };
    let (launch, excluded, settings, settings_file, mcp_file) = match client {
        Client::Claude => {
            let settings_file = private.join("claude-settings.json");
            let mcp_file = private.join("claude-mcp.json");
            let plan =
                exclusion::claude_plan(&plan_input, &settings_file, &mcp_file, &approved_mcp);
            write_atomic(
                &settings_file,
                format!("{}\n", serde_json::to_string_pretty(&plan.settings)?).as_bytes(),
                0o600,
            )?;
            write_atomic(&mcp_file, plan.mcp_config.to_string().as_bytes(), 0o600)?;
            (
                plan.launch,
                plan.excluded,
                Some(plan.settings),
                Some(settings_file),
                Some(mcp_file),
            )
        }
        Client::Codex => {
            let plan = exclusion::codex_plan(&plan_input)?;
            (plan.launch, plan.excluded, None, None, None)
        }
    };

    let notes = notes(client);
    let command = launch.shell_command();
    out.emit(
        json!({
            "ok": true,
            "challenge_id": detail.id,
            "entry_id": entry_id,
            "client": client,
            "folder": challenge_folder,
            "launch": launch.to_json(),
            "settings_file": settings_file,
            "settings": settings,
            "mcp_config_file": mcp_file,
            "approved": copies.iter().map(|copy| json!({
                "id": copy.item.id, "scope": copy.scope, "copy": copy.copy,
            })).collect::<Vec<_>>(),
            "excluded": excluded.iter().map(|excluded| json!({
                "id": excluded.id, "how": excluded.how,
            })).collect::<Vec<_>>(),
            "unreviewed": reconciled.unreviewed.iter()
                .filter(|item| item.client == client)
                .count(),
            "notes": notes,
        }),
        || {
            println!("Set up {} in {}.", detail.title, challenge_folder.display());
            println!(
                "{} approved item(s) brought in; {} left out.",
                copies.len(),
                excluded.len()
            );
            if let Some(path) = &settings_file {
                println!("Claude Code settings for this session: {}", path.display());
            }
            println!();
            println!("Run this yourself, in your own terminal:");
            println!();
            println!("  {command}");
            println!();
            for note in &notes {
                println!("- {note}");
            }
        },
    );
    Ok(())
}

fn notes(client: Client) -> Vec<&'static str> {
    let mut notes = vec![
        "Daycare does not start this session for you. Run the command above when you are ready.",
        "Only output/ is ever uploaded, and only after you type yes.",
        "A program running as you can still read what you can read. Your account of how the \
         work was done is self-reported.",
    ];
    match client {
        Client::Claude => notes.extend([
            "Deny rules bind Claude's file tools, not shell commands.",
            "Once Claude starts, run /status and /memory: no CLAUDE.md outside this folder \
             should be listed.",
        ]),
        Client::Codex => notes.extend([
            "Codex's workspace-write sandbox limits writes to this folder, not reads.",
            "Needs Codex CLI 0.158.0 or newer.",
        ]),
    }
    notes
}
