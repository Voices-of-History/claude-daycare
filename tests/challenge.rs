//! `challenge` and `memory setup`, end to end: the shipped binary against a
//! mock platform, a fake HOME holding planted memory, and a fake `claude`.
//! No network, no keychain, no model, and never the real `~/.claude`,
//! `~/.codex` or `~/.claude-daycare`.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, OnceLock};
use support::{MockPlatform, RecordedRequest, Response};

const BIN: &str = env!("CARGO_BIN_EXE_daycare-runner");
const TOKEN: &str = "dev_token_do_not_leak_9876";
const MCP_SECRET: &str = "planted-bearer-secret-31337";
const MEMORY_TEXT: &str = "PRIVATE-GLOBAL-RULES-do-not-copy";
const CSV: &str = "name,email\nAda,ada@example.test\n";

fn sha256(bytes: &[u8]) -> String {
    daycare_runner::self_update::sha256_hex(bytes)
}

struct Machine {
    root: PathBuf,
    daycare_home: PathBuf,
    fake_home: PathBuf,
    token_file: PathBuf,
    path: String,
    claude_bin: PathBuf,
    claude_marker: PathBuf,
}

impl Drop for Machine {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        if let Ok(reached) = std::fs::read_to_string(&self.claude_marker) {
            panic!("this test launched the real `claude`: {}", reached.trim());
        }
    }
}

impl Machine {
    fn new(label: &str) -> Machine {
        let root = support::scratch_dir(label);
        let fake_home = root.join("fake-home");
        std::fs::create_dir_all(&fake_home).unwrap();
        let claude_bin = support::fake_claude(&root, "", 0, 0);
        Machine {
            daycare_home: root.join("daycare-state"),
            token_file: root.join("tokens.json"),
            path: support::no_claude_path(&root),
            claude_marker: support::claude_marker(&root),
            claude_bin,
            fake_home,
            root,
        }
    }

    fn challenge_root(&self) -> PathBuf {
        self.fake_home.join("daycare/challenges")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .env("DAYCARE_HOME", &self.daycare_home)
            .env("DAYCARE_TOKEN_FILE", &self.token_file)
            .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
            .env("DAYCARE_CHALLENGE_ROOT", self.challenge_root())
            .env("PATH", &self.path)
            .env("HOME", &self.fake_home)
            .env_remove("DAYCARE_WORKSPACE_ROOT")
            .env_remove("DAYCARE_CHALLENGE_SESSION")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("ANTHROPIC_API_KEY")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .expect("run daycare-runner")
    }

    fn run_marked(&self, args: &[&str]) -> Output {
        self.command()
            .env("DAYCARE_CHALLENGE_SESSION", "entry-1")
            .args(args)
            .output()
            .expect("run daycare-runner")
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.fake_home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// One of each kind of owner memory Claude Code reads.
    fn plant_claude_memory(&self) {
        self.write(".claude/CLAUDE.md", &format!("# {MEMORY_TEXT}\n"));
        self.write("CLAUDE.md", "home-level instructions\n");
        self.write(".claude/rules/style.md", "Use tabs.\n");
        self.write(
            ".claude/projects/-Users-sam-app/memory/MEMORY.md",
            "auto memory note\n",
        );
        self.write(
            ".claude/skills/review/SKILL.md",
            "---\nname: review\n---\nReview it.\n",
        );
        self.write(
            ".claude/skills/keep/SKILL.md",
            "---\nname: keep\n---\nKeep it.\n",
        );
        self.write(
            ".claude.json",
            &format!(
                r#"{{"mcpServers":{{"github":{{"type":"http","url":"https://mcp.example.test","headers":{{"Authorization":"Bearer {MCP_SECRET}"}}}},"docs":{{"command":"docs-mcp"}}}}}}"#
            ),
        );
    }

    fn enroll(&self, platform: &MockPlatform) {
        let out = self.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn scan(&self) -> serde_json::Value {
        let out = self.run(&["memory", "setup", "--scan", "--json"]);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Write the approval file directly, in the runner's own schema, from a
    /// scan. Only tests do this: the CLI writes it after a typed yes.
    fn approve(&self, choose: impl Fn(&serde_json::Value) -> Option<&'static str>) {
        let scan = self.scan();
        let items: Vec<serde_json::Value> = scan["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| {
                let scope = choose(item)?;
                let mut approved = serde_json::json!({
                    "id": item["id"], "client": item["client"], "kind": item["kind"],
                    "path": item["path"], "sha256": item["sha256"], "scope": scope,
                });
                if let Some(name) = item.get("name") {
                    approved["name"] = name.clone();
                }
                Some(approved)
            })
            .collect();
        std::fs::create_dir_all(&self.daycare_home).unwrap();
        std::fs::write(
            self.approval_file(),
            serde_json::json!({"version": 1, "written_at": "2026-11-06T12:00:00Z", "items": items})
                .to_string(),
        )
        .unwrap();
    }

    fn approval_file(&self) -> PathBuf {
        self.daycare_home.join("approval.json")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_str(stdout(output).trim())
        .unwrap_or_else(|_| panic!("not one JSON value: {}", stdout(output)))
}

/// A platform with one open challenge, `c-1`, whose input is served by the
/// same mock as a signed storage URL.
fn platform_with(input_sha256: Option<String>) -> MockPlatform {
    let base: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let seen_base = Arc::clone(&base);
    let declared = input_sha256.unwrap_or_else(|| sha256(CSV.as_bytes()));
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        let base = seen_base.get().cloned().unwrap_or_default();
        let path = request.path.as_str();
        if path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.method == "GET" && path == "/api/daycare/challenges" {
            return Response::json(
                200,
                r#"{"challenges":[{"id":"c-1","slug":"signup-rescue","title":"Signup Rescue","state":"open","closes_at":"2026-11-20T00:00:00Z"}]}"#,
            );
        }
        if request.method == "GET" && path == "/api/daycare/challenges/c-1" {
            return Response::json(
                200,
                &serde_json::json!({
                    "challenge": {
                        "id": "c-1", "slug": "signup-rescue", "title": "Signup Rescue",
                        "state": "open", "brief_md": "Make the signup form work.",
                        "weights": {"vote": 40, "goal": 40, "third": 20},
                        "third_label": "Accessibility",
                        "closes_at": "2026-11-20T00:00:00Z",
                    },
                    "inputs": [{
                        "path": "form/data.csv", "bytes": CSV.len(), "sha256": declared,
                        "url": format!("{base}/storage/v1/object/sign/inputs/c-1/form/data.csv?token=signed-grant-xyz"),
                    }],
                })
                .to_string(),
            );
        }
        if request.method == "GET" && path.starts_with("/storage/v1/object/sign/inputs/c-1/") {
            return Response::json(200, CSV);
        }
        if request.method == "POST" && path == "/api/daycare/challenges/c-1/attempts" {
            return Response::json(200, r#"{"entry_id":"entry-1"}"#);
        }
        Response::json(404, r#"{"error":"not here"}"#)
    });
    base.set(platform.base_url.clone()).unwrap();
    platform
}

fn challenge_requests(platform: &MockPlatform) -> Vec<RecordedRequest> {
    platform
        .requests()
        .into_iter()
        .filter(|request| !request.path.ends_with("/pair/claim"))
        .collect()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_scan_lists_claude_md_layers_and_user_skills_without_their_contents() {
    let machine = Machine::new("ch-scan");
    machine.plant_claude_memory();
    machine.write(".codex/AGENTS.md", "codex rules\n");
    machine.write(
        ".codex/config.toml",
        "[mcp_servers.search]\ncommand = \"s\"\nenv = { KEY = \"codex-secret-777\" }\n",
    );
    machine.write(".agents/skills/notes/SKILL.md", "---\nname: notes\n---\n");

    let first = machine.run(&["memory", "setup", "--scan", "--json"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let text = stdout(&first);
    for secret in [
        MEMORY_TEXT,
        MCP_SECRET,
        "codex-secret-777",
        "Use tabs",
        "Review it.",
    ] {
        assert!(
            !text.contains(secret),
            "the scan printed {secret:?}: {text}"
        );
    }
    let scan = json_of(&first);
    let has = |kind: &str, path: &Path| {
        scan["items"].as_array().unwrap().iter().any(|item| {
            item["kind"] == kind && item["path"].as_str() == Some(path.to_str().unwrap())
        })
    };
    let home = &machine.fake_home;
    assert!(has("claude_md", &home.join(".claude/CLAUDE.md")), "{scan}");
    assert!(has("project_claude_md", &home.join("CLAUDE.md")), "{scan}");
    assert!(has("rules", &home.join(".claude/rules/style.md")), "{scan}");
    assert!(has(
        "auto_memory",
        &home.join(".claude/projects/-Users-sam-app/memory/MEMORY.md")
    ));
    assert!(has("skill", &home.join(".claude/skills/review")), "{scan}");
    assert!(has("agents_md", &home.join(".codex/AGENTS.md")), "{scan}");
    assert!(has("skill", &home.join(".agents/skills/notes")), "{scan}");
    let github = scan["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "github")
        .unwrap();
    assert_eq!(github["kind"], "mcp_server");
    assert_eq!(
        github["sha256"].as_str().unwrap().len(),
        64,
        "MCP items carry a hash of the stanza"
    );
    let claude_md = scan["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "claude_md")
        .unwrap();
    assert_eq!(
        claude_md["sha256"],
        sha256(format!("# {MEMORY_TEXT}\n").as_bytes())
    );

    // Deterministic: a second run is byte-identical, and the alias agrees.
    let second = machine.run(&["memory", "setup", "--scan", "--json"]);
    assert_eq!(first.stdout, second.stdout);
    let alias = machine.run(&["memory", "approval", "--scan", "--json"]);
    assert_eq!(first.stdout, alias.stdout);
    // The human listing names items, not contents.
    let human = stdout(&machine.run(&["memory", "setup", "--scan"]));
    assert!(human.contains("CLAUDE.md"), "{human}");
    assert!(!human.contains(MEMORY_TEXT), "{human}");
}

#[test]
fn an_abandoned_or_scripted_review_writes_no_approval_file() {
    let machine = Machine::new("ch-abandon");
    machine.plant_claude_memory();
    let scan = machine.scan();
    let draft = machine.root.join("draft.json");
    let items: Vec<_> = scan["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| serde_json::json!({"id": item["id"], "sha256": item["sha256"], "scope": "private"}))
        .collect();
    std::fs::write(
        &draft,
        serde_json::json!({"version": 1, "items": items}).to_string(),
    )
    .unwrap();
    let draft = draft.to_str().unwrap();

    // Under --json: refused, one JSON error, no file.
    let out = machine.run(&["memory", "setup", "--write", draft, "--json"]);
    assert!(!out.status.success());
    let error = json_of(&out);
    assert_eq!(error["ok"], false);
    assert!(
        error["error"].as_str().unwrap().contains("--json"),
        "{error}"
    );
    assert!(!machine.approval_file().exists());

    // No terminal (stdin is /dev/null, stdout a pipe), even with "yes" piped
    // in: refused, no file.
    let out = machine
        .command()
        .args(["memory", "setup", "--write", draft])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(b"yes\n").ok();
            child.wait_with_output()
        })
        .unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("real terminal"), "{}", stderr(&out));
    assert!(!machine.approval_file().exists());

    for args in [
        vec!["memory", "setup", "--accept-all"],
        vec!["memory", "setup", "--accept-all", "--json"],
    ] {
        let out = machine.run(&args);
        assert!(!out.status.success(), "{args:?}");
        assert!(!machine.approval_file().exists(), "{args:?}");
    }
}

#[test]
fn a_changed_hash_forces_a_re_review_in_check_and_in_start() {
    let machine = Machine::new("ch-rehash");
    machine.plant_claude_memory();
    let platform = platform_with(None);
    machine.enroll(&platform);
    machine.approve(|item| (item["kind"] == "claude_md").then_some("private"));

    let ok = machine.run(&["memory", "setup", "--check", "--json"]);
    assert!(ok.status.success(), "{}", stdout(&ok));
    let report = json_of(&ok);
    assert_eq!(report["approved"].as_array().unwrap().len(), 1);

    machine.write(".claude/CLAUDE.md", "# edited after approval\n");
    let changed = machine.run(&["memory", "setup", "--check", "--json"]);
    assert!(!changed.status.success());
    let error = json_of(&changed)["error"].as_str().unwrap().to_string();
    assert!(error.contains(".claude/CLAUDE.md"), "{error}");

    let claude_bin = machine.claude_bin.to_str().unwrap().to_string();
    let start = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--print",
        "--agent",
        "claude",
        "--claude-bin",
        &claude_bin,
        "--json",
    ]);
    assert!(!start.status.success(), "{}", stdout(&start));
    assert!(
        stdout(&start).contains("changed since you reviewed it"),
        "{}",
        stdout(&start)
    );
    assert!(
        challenge_requests(&platform).is_empty(),
        "a refused start still reached the platform: {:?}",
        challenge_requests(&platform)
    );
    assert!(!machine.challenge_root().join("c-1").exists());
}

#[test]
fn challenge_list_and_show_use_the_identity_token_and_never_print_links() {
    let machine = Machine::new("ch-list");
    let platform = platform_with(None);
    machine.enroll(&platform);

    let list = machine.run(&["challenge", "list", "--json"]);
    assert!(list.status.success(), "{}", stdout(&list));
    let list = json_of(&list);
    assert_eq!(list["challenges"][0]["id"], "c-1");
    assert_eq!(list["challenges"][0]["title"], "Signup Rescue");

    let show = machine.run(&["challenge", "show", "c-1", "--json"]);
    assert!(show.status.success(), "{}", stdout(&show));
    let text = stdout(&show);
    assert!(!text.contains("signed-grant-xyz"), "{text}");
    let show = json_of(&show);
    assert_eq!(show["challenge"]["weights"]["goal"], 40);
    assert_eq!(show["challenge"]["inputs"][0]["path"], "form/data.csv");

    let requests = challenge_requests(&platform);
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.authorization(),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }
    // An id that could escape the URL path is refused before any request.
    let bad = machine.run(&["challenge", "show", "../visits", "--json"]);
    assert!(!bad.status.success());
    assert_eq!(challenge_requests(&platform).len(), 2);
}

#[test]
fn start_print_sets_up_the_folder_and_prints_the_exclusions_and_no_token() {
    let machine = Machine::new("ch-start");
    machine.plant_claude_memory();
    let platform = platform_with(None);
    machine.enroll(&platform);
    // The owner approved one skill (private) and one MCP server (public).
    machine.approve(|item| match item["name"].as_str() {
        Some("keep") => Some("private"),
        Some("docs") => Some("public"),
        _ => None,
    });

    let claude_bin = machine.claude_bin.to_str().unwrap().to_string();
    let out = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--print",
        "--agent",
        "claude",
        "--claude-bin",
        &claude_bin,
        "--json",
    ]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    for secret in [TOKEN, MCP_SECRET, MEMORY_TEXT, "signed-grant-xyz"] {
        assert!(
            !text.contains(secret),
            "--print output leaked {secret:?}: {text}"
        );
    }
    let result = json_of(&out);
    assert_eq!(result["entry_id"], "entry-1");

    // The exclusion settings, in full.
    let settings = &result["settings"];
    assert_eq!(settings["autoMemoryEnabled"], false);
    let excludes: Vec<&str> = settings["claudeMdExcludes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let home = &machine.fake_home;
    for path in [
        home.join(".claude/CLAUDE.md"),
        home.join("CLAUDE.md"),
        home.join(".claude/rules/style.md"),
    ] {
        assert!(
            excludes.contains(&path.to_str().unwrap()),
            "{path:?} not in {excludes:?}"
        );
    }
    let deny: Vec<&str> = settings["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for rule in [
        format!("Read(/{}/**)", machine.daycare_home.display()),
        format!("Read(/{}/**)", home.join(".ssh").display()),
        format!("Read(/{}/**)", home.join(".claude/skills/review").display()),
        "Skill(review)".to_string(),
        "mcp__github".to_string(),
    ] {
        assert!(deny.contains(&rule.as_str()), "missing {rule} in {deny:?}");
    }
    assert!(!deny.contains(&"Skill(keep)"));
    assert!(!deny.contains(&"mcp__docs"));
    assert_eq!(settings["skillOverrides"]["review"], "off");

    // The launch: the settings file, approved MCP only, auto memory off, the
    // marker, and no Daycare token anywhere in it.
    let launch = &result["launch"];
    assert_eq!(launch["env_set"]["CLAUDE_CODE_DISABLE_AUTO_MEMORY"], "1");
    assert_eq!(launch["env_set"]["DAYCARE_CHALLENGE_SESSION"], "entry-1");
    let unset: Vec<&str> = launch["env_unset"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for name in [
        "DAYCARE_DEVICE_TOKEN",
        "DAYCARE_TOKEN_FILE",
        "DAYCARE_HOME",
        "ANTHROPIC_API_KEY",
    ] {
        assert!(unset.contains(&name), "{name} not unset: {unset:?}");
    }
    let command = launch["command"].as_str().unwrap();
    assert!(command.contains("--settings"), "{command}");
    assert!(command.contains("--strict-mcp-config"), "{command}");
    assert!(
        command.contains("CLAUDE_CODE_DISABLE_AUTO_MEMORY='1'"),
        "{command}"
    );

    // The settings file is outside the folder, owner-only, and token-free;
    // the MCP file holds only the approved server.
    let folder = PathBuf::from(result["folder"].as_str().unwrap());
    assert_eq!(folder, machine.challenge_root().join("c-1"));
    let settings_file = PathBuf::from(result["settings_file"].as_str().unwrap());
    assert!(!settings_file.starts_with(&folder));
    assert!(settings_file.starts_with(&machine.daycare_home));
    let mcp_file = PathBuf::from(result["mcp_config_file"].as_str().unwrap());
    let mcp: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&mcp_file).unwrap()).unwrap();
    assert_eq!(
        mcp["mcpServers"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["docs"]
    );
    assert!(!std::fs::read_to_string(&settings_file)
        .unwrap()
        .contains(TOKEN));
    #[cfg(unix)]
    {
        assert_eq!(mode(&settings_file), 0o600);
        assert_eq!(mode(&mcp_file), 0o600);
        assert_eq!(mode(&folder), 0o700);
    }

    // The folder.
    assert!(std::fs::read_to_string(folder.join("BRIEF.md"))
        .unwrap()
        .contains("Make the signup form work."));
    assert_eq!(
        std::fs::read_to_string(folder.join("inputs/form/data.csv")).unwrap(),
        CSV
    );
    assert_eq!(std::fs::read_dir(folder.join("output")).unwrap().count(), 0);
    let pointer = std::fs::read_to_string(folder.join("CLAUDE.md")).unwrap();
    assert!(pointer.contains("Daycare wrote this file"), "{pointer}");
    assert!(pointer.contains("Skill `keep`"), "{pointer}");
    assert!(pointer.contains("MCP server `docs`"), "{pointer}");
    assert!(folder
        .join("approved/private/skills/keep/SKILL.md")
        .is_file());
    let entry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(folder.join(".daycare-record.json")).unwrap())
            .unwrap();
    assert_eq!(entry["entry_id"], "entry-1");
    assert_eq!(entry["challenge_id"], "c-1");
    assert_eq!(entry["inputs"][0]["sha256"], sha256(CSV.as_bytes()));
    // The record is not named like the declaration the brief asks for in
    // output/, and the agent is told to leave it alone.
    assert!(!folder.join("ENTRY.json").exists());
    assert!(
        pointer.contains("Leave `.daycare-record.json` and `meter/` alone"),
        "{pointer}"
    );
    for file in [".daycare-record.json", "CLAUDE.md", "BRIEF.md"] {
        let contents = std::fs::read_to_string(folder.join(file)).unwrap();
        assert!(!contents.contains(TOKEN), "{file} holds the token");
        assert!(
            !contents.contains(MEMORY_TEXT),
            "{file} holds unapproved memory"
        );
    }

    // The platform saw the identity token and a body with the actor id only.
    let attempts: Vec<_> = challenge_requests(&platform)
        .into_iter()
        .filter(|request| request.path.ends_with("/attempts"))
        .collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].json(),
        serde_json::json!({"actor_id": "actor-1"})
    );
    assert_eq!(
        attempts[0].authorization(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    // The storage download carried no Daycare credential.
    let download = challenge_requests(&platform)
        .into_iter()
        .find(|request| request.path.starts_with("/storage/"))
        .unwrap();
    assert_eq!(download.authorization(), None);

    // The human form prints the same command for the owner to run.
    let human = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--print",
        "--agent",
        "claude",
        "--claude-bin",
        &claude_bin,
    ]);
    assert!(human.status.success(), "{}", stderr(&human));
    let human = stdout(&human);
    assert!(human.contains("Run this yourself"), "{human}");
    assert!(human.contains("--settings"), "{human}");
    assert!(!human.contains(TOKEN), "{human}");
}

#[test]
fn a_wrong_input_sha256_aborts_before_enrolling_or_writing() {
    let machine = Machine::new("ch-badseed");
    let platform = platform_with(Some(sha256(b"some other file")));
    machine.enroll(&platform);
    let claude_bin = machine.claude_bin.to_str().unwrap().to_string();
    let out = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--print",
        "--agent",
        "claude",
        "--claude-bin",
        &claude_bin,
        "--json",
    ]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("sha256"), "{}", stdout(&out));
    assert!(challenge_requests(&platform)
        .iter()
        .all(|request| !request.path.ends_with("/attempts")));
    assert!(!machine.challenge_root().join("c-1").exists());
}

/// Without `--print`, `start` now launches the client interactively; that
/// path is covered in `tests/challenge_launch.rs`, and OpenCode's in
/// `tests/challenge_opencode.rs`. OpenCode needs `--model`, and only OpenCode
/// takes it; both are refused before any request.
#[test]
fn start_with_opencode_needs_a_model_and_is_refused_without_one_before_any_request() {
    let machine = Machine::new("ch-noprint");
    let platform = platform_with(None);
    machine.enroll(&platform);
    for args in [
        vec!["challenge", "start", "c-1", "--agent", "opencode", "--json"],
        vec![
            "challenge",
            "start",
            "c-1",
            "--print",
            "--agent",
            "opencode",
            "--json",
        ],
    ] {
        let out = machine.run(&args);
        assert!(!out.status.success());
        assert!(
            stdout(&out).contains("--model provider/model"),
            "{}",
            stdout(&out)
        );
    }
    let out = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--print",
        "--agent",
        "claude",
        "--model",
        "openai/gpt-5.5",
        "--json",
    ]);
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains("--model is for --agent opencode"),
        "{}",
        stdout(&out)
    );
    assert!(challenge_requests(&platform).is_empty());
}

#[test]
fn codex_start_uses_config_switches_and_refuses_an_unapproved_agents_md() {
    let machine = Machine::new("ch-codex");
    machine.write(
        ".codex/config.toml",
        "model = \"gpt\"\n[mcp_servers.search]\ncommand = \"s\"\nenv = { KEY = \"codex-secret-777\" }\n",
    );
    let platform = platform_with(None);
    machine.enroll(&platform);

    let out = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--print",
        "--agent",
        "codex",
        "--json",
    ]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(
        !text.contains(TOKEN) && !text.contains("codex-secret-777"),
        "{text}"
    );
    let result = json_of(&out);
    let args: Vec<&str> = result["launch"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let pairs: Vec<[&str; 2]> = args.windows(2).map(|w| [w[0], w[1]]).collect();
    for pair in [
        ["--disable", "memories"],
        ["--sandbox", "workspace-write"],
        ["-c", "mcp_servers.search.enabled=false"],
        ["-c", "skills.include_instructions=false"],
    ] {
        assert!(pairs.contains(&pair), "{pair:?} not in {args:?}");
    }
    assert!(result["launch"]["env_set"].get("HOME").is_none());
    assert!(result["launch"]["env_set"].get("CODEX_HOME").is_none());
    let folder = PathBuf::from(result["folder"].as_str().unwrap());
    assert!(folder.join("AGENTS.md").is_file());

    // An unapproved global AGENTS.md cannot be left out by config, so the
    // start refuses, before any request.
    machine.write(".codex/AGENTS.md", "codex rules\n");
    let before = challenge_requests(&platform).len();
    let out = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--print",
        "--agent",
        "codex",
        "--json",
    ]);
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains(".codex/AGENTS.md"),
        "{}",
        stdout(&out)
    );
    assert_eq!(challenge_requests(&platform).len(), before);
}

#[test]
fn the_marker_blocks_visit_start_and_friends_inside_a_challenge() {
    let machine = Machine::new("ch-marker");
    let platform = platform_with(None);
    machine.enroll(&platform);
    let requests_before = platform.requests().len();
    let claude_bin = machine.claude_bin.to_str().unwrap().to_string();

    let out = machine.run_marked(&["visit", "start", "--claude-bin", &claude_bin, "--json"]);
    assert!(!out.status.success());
    let error = json_of(&out);
    assert_eq!(error["ok"], false);
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("inside a challenge session"),
        "{error}"
    );
    for args in [
        vec!["memory", "setup", "--scan", "--json"],
        vec!["challenge", "start", "c-1", "--print", "--json"],
        vec!["challenge", "submit", "--json"],
    ] {
        let out = machine.run_marked(&args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            stdout(&out).contains("inside a challenge session"),
            "{args:?}: {}",
            stdout(&out)
        );
    }
    assert_eq!(
        platform.requests().len(),
        requests_before,
        "a marked command reached the platform"
    );
    assert!(
        !machine.root.join("call.argv").exists(),
        "a marked visit start launched the agent"
    );
    // Reading stays allowed.
    assert!(machine
        .run_marked(&["challenge", "list", "--json"])
        .status
        .success());
}

#[test]
fn plain_skill_install_still_installs_the_daycare_skill_and_adds_the_review_skill() {
    let machine = Machine::new("ch-skill");
    let out = machine.run(&["skill", "install", "--json"]);
    assert!(out.status.success(), "{}", stdout(&out));
    let result = json_of(&out);
    assert_eq!(result["changed"], true);
    let home = &machine.fake_home;
    let libraries = [
        ".claude/skills",
        ".agents/skills",
        ".config/opencode/skills",
    ];
    for library in libraries {
        assert_eq!(
            std::fs::read_to_string(home.join(library).join("daycare/SKILL.md")).unwrap(),
            include_str!("../skill/SKILL.md")
        );
        assert_eq!(
            std::fs::read_to_string(home.join(library).join("daycare-memory-review/SKILL.md"))
                .unwrap(),
            include_str!("../skill/daycare-memory-review/SKILL.md")
        );
    }
    let again = machine.run(&["skill", "install"]);
    assert!(stdout(&again).contains("already installed and current"));

    // An edited copy of the review skill is upgraded, with a .bak kept.
    let review = home.join(".claude/skills/daycare-memory-review/SKILL.md");
    let original = std::fs::read_to_string(&review).unwrap();
    std::fs::write(&review, format!("{original}\nmy note\n")).unwrap();
    assert!(machine.run(&["skill", "install"]).status.success());
    assert_eq!(std::fs::read_to_string(&review).unwrap(), original);
    assert_eq!(
        std::fs::read_to_string(review.with_extension("md.bak")).unwrap(),
        format!("{original}\nmy note\n")
    );

    // A foreign skill at the review skill's path refuses the whole install
    // without --force, and nothing else is written.
    let agents_review = home.join(".agents/skills/daycare-memory-review/SKILL.md");
    std::fs::write(&agents_review, "---\nname: someone-elses\n---\n").unwrap();
    let daycare = home.join(".claude/skills/daycare/SKILL.md");
    std::fs::remove_file(&daycare).unwrap();
    let refused = machine.run(&["skill", "install"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("not Daycare's"),
        "{}",
        stderr(&refused)
    );
    assert!(
        !daycare.exists(),
        "a refused install still wrote the daycare skill"
    );
    let forced = machine.run(&["skill", "install", "--force"]);
    assert!(forced.status.success());
    assert_eq!(
        std::fs::read_to_string(&agents_review).unwrap(),
        include_str!("../skill/daycare-memory-review/SKILL.md")
    );

    let show =
        json_of(&machine.run(&["skill", "show", "--name", "daycare-memory-review", "--json"]));
    assert_eq!(show["name"], "daycare-memory-review");
}

/// The positive path, on a real pseudo-terminal: the person types yes and the
/// approval is written. macOS `script` only; the logic is covered on every
/// platform by the unit tests with a scripted prompt.
#[cfg(target_os = "macos")]
#[test]
fn a_typed_yes_on_a_real_terminal_writes_the_approval() {
    use std::io::Write;
    let machine = Machine::new("ch-tty");
    machine.plant_claude_memory();
    let mut child = machine
        .command()
        .args(["-q", "/dev/null", BIN, "memory", "setup", "--accept-all"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn_script();
    // Answer each question only once it is on screen, as a person would:
    // input sent ahead of the prompt can arrive behind script's own EOF.
    let mut stdin = child.stdin.take().unwrap();
    let mut screen = child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut seen = String::new();
        let mut buffer = [0u8; 4096];
        while let Ok(count) = screen.read(&mut buffer) {
            if count == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&buffer[..count]));
            let _ = send.send(seen.clone());
        }
        seen
    });
    let wait_for = |needle: &str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match receive.recv_timeout(left) {
                Ok(seen) if seen.contains(needle) => return,
                Ok(_) => continue,
                Err(_) => panic!("never saw {needle:?}"),
            }
        }
    };
    wait_for("allow private use of all of them");
    stdin.write_all(b"yes\n").unwrap();
    wait_for("Type yes or no");
    stdin.write_all(b"no\n").unwrap();
    let status = child.wait().unwrap();
    drop(stdin);
    let screen = reader.join().unwrap();
    assert!(
        machine.approval_file().is_file(),
        "no approval was written ({status}): {screen}"
    );
    let approval: serde_json::Value =
        serde_json::from_slice(&std::fs::read(machine.approval_file()).unwrap()).unwrap();
    let items = approval["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|item| item["scope"] == "private"));
    assert!(items
        .iter()
        .any(|item| item["kind"] == "claude_md" && item["sha256"].as_str().unwrap().len() == 64));
    assert_eq!(mode(&machine.approval_file()), 0o600);
}

#[cfg(target_os = "macos")]
trait SpawnScript {
    fn spawn_script(&mut self) -> std::process::Child;
}

#[cfg(target_os = "macos")]
impl SpawnScript for Command {
    /// Re-target an already configured runner command at `/usr/bin/script`,
    /// keeping its environment and arguments.
    fn spawn_script(&mut self) -> std::process::Child {
        let args: Vec<_> = self.get_args().map(|a| a.to_owned()).collect();
        let mut script = Command::new("/usr/bin/script");
        for (name, value) in self.get_envs() {
            match value {
                Some(value) => script.env(name, value),
                None => script.env_remove(name),
            };
        }
        script
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run script(1)")
    }
}
