//! `daycare-runner challenge`: list, show, start (plan tasks N6–N13).
//!
//! A challenge is work the owner's agent does on the owner's machine, with the
//! owner's own client and tools, in a folder Daycare sets up. The platform
//! never starts that session: the owner runs `challenge start` in their own
//! terminal, and it starts their client there (`launch`), or with `--print`
//! prints the exact command and environment for them to run themselves. The
//! client is Claude Code, Codex CLI or OpenCode (`client`).
//!
//! `start` does its checks in an order that leaves nothing behind on failure:
//! the session marker, the client's policy guard, the memory approval (a
//! changed hash refuses), the client's own refusals (`client`), the challenge
//! record, every input file downloaded and checked, and only then the
//! enrollment call and the folder itself. The interactive launch then
//! verifies (`verify`), reads the meter (`meter`), and starts the client.
//!
//! Every platform call goes through `PlatformClient` with the identity token,
//! so the release header is sent and a 426 makes `main` update once and run
//! the same command again.

pub mod client;
pub mod exclusion;
pub mod folder;
pub mod launch;
pub mod meter;
pub mod opencode;
pub mod scan;
pub mod submit;
pub mod upload;
pub mod verify;

use crate::approval::scan::{self as memory_scan, Client, Roots};
use crate::approval::{reconcile, Approval};
use crate::out::Out;
use crate::paths::{create_private_dir, sanitize_segment, Layout};
use crate::platform::{download_signed, PlatformClient};
use crate::session::Active;
use crate::{Error, Result};
use exclusion::{PlanInput, SESSION_MARKER_ENV};
use folder::{ChallengeDetail, FolderSpec};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The entry id when this process runs inside a challenge session.
pub fn inside_session() -> Option<String> {
    // `var_os`: a value that is not UTF-8 still marks a session.
    std::env::var_os(SESSION_MARKER_ENV)
        .map(|value| value.to_string_lossy().into_owned())
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

/// What `challenge start` was asked to do.
pub struct StartRequest<'a> {
    pub layout: &'a Layout,
    pub active: &'a Active,
    pub challenge_id: &'a str,
    pub client: Client,
    pub claude_bin: &'a str,
    pub codex_bin: &'a str,
    pub opencode_bin: &'a str,
    /// OpenCode only, and required for it: `provider/model`.
    pub model: Option<&'a str>,
    pub started_at: &'a str,
    /// Print the command instead of starting the client.
    pub print: bool,
    /// Codex only: run with a separate `CODEX_HOME` holding approved parts.
    pub separate_codex_home: bool,
    /// The clock for meter readings.
    pub now: fn() -> String,
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

/// Daycare's private folder for a challenge (generated settings, a separate
/// Codex home) and the challenge folder must not overlap: what is generated
/// there must never be part of what the session's work could upload.
/// `DAYCARE_CHALLENGE_ROOT` could otherwise point the two at one place.
fn guard_apart(private: &Path, folder: &Path) -> Result<()> {
    let physical = |path: &Path| {
        // The deepest existing ancestor, resolved, plus the rest.
        let mut existing = path.to_path_buf();
        let mut rest = Vec::new();
        while std::fs::symlink_metadata(&existing).is_err() {
            match (existing.file_name(), existing.parent()) {
                (Some(name), Some(parent)) => {
                    rest.push(name.to_os_string());
                    existing = parent.to_path_buf();
                }
                _ => break,
            }
        }
        let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
        resolved.extend(rest.into_iter().rev());
        resolved
    };
    for (a, b) in [
        (private.to_path_buf(), folder.to_path_buf()),
        (physical(private), physical(folder)),
    ] {
        if a.starts_with(&b) || b.starts_with(&a) {
            return Err(Error::new(format!(
                "the challenge folder {} and Daycare's own folder for it {} overlap; set {} to \
                 a folder outside {}",
                folder.display(),
                private.display(),
                folder::CHALLENGE_ROOT_ENV,
                private.parent().unwrap_or(private).display()
            )));
        }
    }
    Ok(())
}

fn inherited_daycare_env() -> Vec<String> {
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| name.starts_with("DAYCARE_"))
        .collect();
    names.sort();
    names
}

/// `challenge start <id>`, with or without `--print`.
pub fn start(request: StartRequest<'_>, out: Out) -> Result<()> {
    refuse_inside_session("challenge start")?;
    let StartRequest {
        layout,
        active,
        challenge_id,
        client,
        claude_bin,
        codex_bin,
        opencode_bin,
        model,
        started_at,
        print,
        separate_codex_home,
        now,
    } = request;
    folder::validate_challenge_id(challenge_id)?;
    match (client, model) {
        (Client::Opencode, None) => {
            return Err(Error::new(
                "OpenCode challenges need --model provider/model, on your own provider API key                  (for example --model openai/gpt-5.5)",
            ))
        }
        (Client::Opencode, Some(model)) => {
            opencode::check_model(model)?;
        }
        (_, Some(_)) => {
            return Err(Error::new(
                "--model is for --agent opencode; Claude Code and Codex use your own default                  model",
            ))
        }
        (_, None) => {}
    }
    if separate_codex_home && client != Client::Codex {
        return Err(Error::new(
            "--separate-codex-home is for Codex; pass --agent codex with it",
        ));
    }
    if separate_codex_home && print {
        return Err(Error::new(
            "--separate-codex-home needs Daycare to start Codex itself, so it can copy your \
             login in and hand a refreshed one back when the session ends. Run it without \
             --print",
        ));
    }
    let challenge_root = folder::challenge_root()?;
    let roots = Roots::discover(layout.root(), &challenge_root)?;
    let private_root = layout.root().join("challenges");
    let codex_home_under =
        |private: &Path| separate_codex_home.then(|| client::separate_codex_home(private));
    let placeholder_private = private_root.join(sanitize_segment(challenge_id));

    match client {
        Client::Claude => crate::workspace::guard_no_managed_claude(claude_bin)?,
        Client::Codex => guard_no_managed_codex(
            codex_home_under(&placeholder_private)
                .as_deref()
                .unwrap_or(&roots.codex_home),
        )?,
        Client::Opencode => opencode::guard_no_managed_config()?,
    }

    // Memory first: a changed approval stops here, before any request.
    let inventory = memory_scan::scan(&roots);
    let approval = Approval::load(layout)?;
    let reconciled = reconcile(approval.as_ref(), &inventory);
    reconciled.require_reviewed()?;
    let inherited = inherited_daycare_env();
    let programs = client::Programs {
        claude: claude_bin.to_string(),
        codex: codex_bin.to_string(),
        opencode: opencode_bin.to_string(),
        opencode_model: model.map(str::to_string),
    };
    let session_client = client::for_client(client, &programs, &roots);
    let program = session_client.program().to_string();

    // A client's refusals (memory it cannot leave out, approved bytes that
    // changed) are known before the network, so plan once with a placeholder
    // folder and entry first.
    let placeholder = challenge_root.join(sanitize_segment(challenge_id));
    let placeholder_home = codex_home_under(&placeholder_private);
    session_client.exclusion_plan(
        &PlanInput {
            inventory: &inventory,
            reconciled: &reconciled,
            folder: &placeholder,
            home: &roots.home,
            claude_config_dir: &roots.claude_config_dir,
            daycare_root: layout.root(),
            entry_id: "pending",
            program: &program,
            inherited_daycare_env: &inherited,
            codex_home: placeholder_home.as_deref(),
        },
        &placeholder_private,
    )?;

    let detail = fetch_detail(active, challenge_id)?;
    if let Some(state) = &detail.state {
        if state != "open" {
            return Err(Error::new(format!(
                "{} is {state}, not open; it cannot be started",
                detail.title
            )));
        }
    }
    let challenge_folder = challenge_root.join(sanitize_segment(&detail.id));
    // Generated client files live under Daycare's own folder, outside the
    // challenge folder, where the session's deny rules cover them.
    let private = private_root.join(sanitize_segment(&detail.id));
    guard_apart(&private, &challenge_folder)?;
    let codex_home = codex_home_under(&private);
    if let Some(home) = &codex_home {
        guard_no_managed_codex(home)?;
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

    create_private_dir(layout.root())?;
    create_private_dir(&private_root)?;
    create_private_dir(&private)?;

    let plan_input = PlanInput {
        inventory: &inventory,
        reconciled: &reconciled,
        folder: &challenge_folder,
        home: &roots.home,
        claude_config_dir: &roots.claude_config_dir,
        daycare_root: layout.root(),
        entry_id: &entry_id,
        program: &program,
        inherited_daycare_env: &inherited,
        codex_home: codex_home.as_deref(),
    };
    let plan = session_client.exclusion_plan(&plan_input, &private)?;
    client::write_plan_files(&plan, &challenge_folder)?;
    let approved: Vec<Value> = copies
        .iter()
        .map(|copy| {
            json!({
                "id": copy.item.id, "scope": copy.scope, "copy": copy.copy,
            })
        })
        .collect();

    if !print {
        return launch::run_session(
            launch::SessionRun {
                client: session_client.as_ref(),
                plan: &plan,
                input: &plan_input,
                layout,
                challenge_id: &detail.id,
                title: &detail.title,
                approved,
                excluded: &plan.excluded,
                notes: launch::notes(client),
                now,
            },
            out,
        );
    }

    let notes = notes(client);
    let launch = &plan.launch;
    let command = launch.shell_command();
    out.emit(
        json!({
            "ok": true,
            "challenge_id": detail.id,
            "entry_id": entry_id,
            "client": client,
            "folder": challenge_folder,
            "launch": launch.to_json(),
            "settings_file": plan.settings_file,
            "settings": plan.settings,
            "mcp_config_file": plan.mcp_config_file,
            "approved": approved,
            "excluded": plan.excluded.iter().map(|excluded| json!({
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
                plan.excluded.len()
            );
            if let Some(path) = &plan.settings_file {
                println!(
                    "{} settings for this session: {}",
                    session_client.product_name(),
                    path.display()
                );
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
        Client::Opencode => notes.extend([
            "OpenCode has no sandbox: its shell commands run as you, under your own OpenCode \
             permissions.",
            "OpenCode runs on your own provider API key. Daycare never reads, stores or sends \
             the key.",
            "Daycare has not checked what OpenCode resolves. Run `challenge start` without \
             --print to have it checked before OpenCode starts.",
            "Needs OpenCode 1.18.33 or newer.",
        ]),
    }
    notes
}
