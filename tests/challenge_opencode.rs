//! OpenCode as a challenge client (plan task N18), and its N22 tests: the
//! OpenCode inventory in `memory setup --scan`, `challenge start --agent
//! opencode` with and without `--print`, the checks over what OpenCode
//! resolved, and the refusals before any request.
//!
//! The shipped binary runs against a mock platform, a fake HOME, and a
//! recording fake of `opencode` (`support::fake_opencode`) that merges config
//! the way OpenCode 1.18.33 does. No network, no keychain, no model, never a
//! real client: a bare `claude`, `codex` or `opencode` on PATH is a shim that
//! records it was reached and refuses, and the test then fails.

mod support;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, OnceLock};
use support::{MockPlatform, RecordedRequest, Response};

const BIN: &str = env!("CARGO_BIN_EXE_daycare-runner");
const TOKEN: &str = "dev_token_do_not_leak_9876";
const ANTHROPIC_KEY: &str = "sk-ant-planted-api-key-4242";
const OPENAI_KEY: &str = "sk-openai-planted-key-5353";
const MCP_SECRET: &str = "oc-mcp-secret-777";
const MODEL: &str = "openai/gpt-5.5";
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
    opencode_bin: PathBuf,
    claude_marker: PathBuf,
    record: PathBuf,
}

impl Drop for Machine {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        if let Ok(reached) = std::fs::read_to_string(&self.claude_marker) {
            panic!("this test launched a real client: {}", reached.trim());
        }
    }
}

impl Machine {
    fn new(label: &str) -> Machine {
        let root = support::scratch_dir(label);
        let fake_home = root.join("fake-home");
        std::fs::create_dir_all(&fake_home).unwrap();
        let path = support::no_claude_path(&root);
        let claude_marker = support::claude_marker(&root);
        for name in ["codex", "opencode"] {
            support::testdir::write_executable(
                &root.join("no-claude-bin").join(name),
                &format!(
                    "#!/bin/sh\nprintf '{name} %s\\n' \"$*\" >> {}\nexit 97\n",
                    claude_marker.display()
                ),
            );
        }
        let record = root.join("opencode-record");
        let opencode_bin = support::fake_opencode(&root, &record);
        Machine {
            daycare_home: root.join("daycare-state"),
            token_file: root.join("tokens.json"),
            path,
            claude_marker,
            opencode_bin,
            record,
            fake_home,
            root,
        }
    }

    fn challenge_root(&self) -> PathBuf {
        self.fake_home.join("daycare/challenges")
    }

    fn folder(&self) -> PathBuf {
        self.challenge_root().join("c-1")
    }

    fn config_dir(&self) -> PathBuf {
        self.fake_home.join(".config/opencode")
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
            // Planted in the runner's environment so the launch must handle them.
            .env("DAYCARE_DEVICE_TOKEN", TOKEN)
            .env("ANTHROPIC_API_KEY", ANTHROPIC_KEY)
            .env("OPENAI_API_KEY", OPENAI_KEY)
            .env(
                "OPENCODE_CONFIG_CONTENT",
                r#"{"instructions":["/planted.md"]}"#,
            )
            .env("OPENCODE_CONFIG_DIR", self.root.join("planted-config-dir"))
            .env_remove("DAYCARE_WORKSPACE_ROOT")
            .env_remove("DAYCARE_CHALLENGE_SESSION")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("OPENCODE_CONFIG")
            .env_remove("OPENCODE_PERMISSION")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(&[], args)
    }

    fn run_with(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = self.command();
        for (name, value) in env {
            command.env(name, value);
        }
        command.args(args).output().expect("run daycare-runner")
    }

    /// `challenge start c-1 --agent opencode --model <model>` plus `extra`.
    fn start(&self, model: &str, extra: &[&str]) -> Output {
        self.start_with(&[], model, extra)
    }

    fn start_with(&self, env: &[(&str, &str)], model: &str, extra: &[&str]) -> Output {
        let bin = self.opencode_bin.to_str().unwrap().to_string();
        let mut args = vec![
            "challenge",
            "start",
            "c-1",
            "--agent",
            "opencode",
            "--model",
            model,
            "--opencode-bin",
            &bin,
            "--json",
        ];
        args.extend_from_slice(extra);
        self.run_with(env, &args)
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.fake_home.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn enroll(&self, platform: &MockPlatform) {
        let out = self.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
        assert!(out.status.success(), "{}", stderr(&out));
    }

    fn scan(&self) -> Value {
        let out = self.run(&["memory", "setup", "--scan", "--json"]);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Write the approval file directly from a scan; only tests do this.
    fn approve(&self, choose: impl Fn(&Value) -> Option<&'static str>) {
        let items: Vec<Value> = self.scan()["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| {
                let scope = choose(item)?;
                let mut approved = json!({
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
            self.daycare_home.join("approval.json"),
            json!({"version": 1, "written_at": "2026-11-12T12:00:00Z", "items": items}).to_string(),
        )
        .unwrap();
    }

    fn recorded(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.record.join(name)).ok()
    }

    /// One of everything OpenCode reads, and an API-key login.
    fn populate(&self) {
        self.write(
            ".config/opencode/opencode.json",
            &json!({
                "$schema": "https://opencode.ai/config.json",
                "mcp": {
                    "search": {"type": "local", "command": ["search-server"],
                               "environment": {"KEY": MCP_SECRET}},
                    "docs": {"type": "remote", "url": "https://docs.example.test/mcp"},
                },
                "agent": {"reviewer": {"prompt": "INLINE-AGENT-PROMPT", "mode": "subagent"}},
            })
            .to_string(),
        );
        self.write(".config/opencode/AGENTS.md", "# OPENCODE-GLOBAL-RULES\n");
        self.write(
            ".config/opencode/skills/keep/SKILL.md",
            "---\nname: keep\ndescription: Keep it.\n---\nKEEP-SKILL-BODY\n",
        );
        self.write(
            ".config/opencode/skills/notes/SKILL.md",
            "---\nname: notes\ndescription: Notes.\n---\nNOTES-SKILL-BODY\n",
        );
        self.write(
            ".config/opencode/agents/helper.md",
            "---\ndescription: Helps.\n---\nHELPER-AGENT-PROMPT\n",
        );
        self.write(".config/opencode/plugin/notify.ts", "export default {}\n");
        self.write(
            ".local/share/opencode/auth.json",
            &json!({"openai": {"type": "api", "key": OPENAI_KEY},
                    "anthropic": {"type": "api", "key": ANTHROPIC_KEY}})
            .to_string(),
        );
    }

    /// Approve the global AGENTS.md, the `docs` server and the `keep` skill.
    fn approve_some(&self) {
        self.approve(|item| {
            let opencode = item["client"] == "opencode";
            let chosen =
                item["kind"] == "agents_md" || item["name"] == "docs" || item["name"] == "keep";
            (opencode && chosen).then_some("private")
        });
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_of(output: &Output) -> Value {
    serde_json::from_str(stdout(output).trim())
        .unwrap_or_else(|_| panic!("not one JSON value: {}", stdout(output)))
}

fn env_file(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap())
        .unwrap_or_else(|_| panic!("{} is not JSON", path.display()))
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The session (or a check run with its environment) never sees a Daycare
/// credential, a planted config source, or a key for another provider.
fn assert_session_env(env: &HashMap<String, String>, what: &str, keep_anthropic: bool) {
    for name in [
        "DAYCARE_DEVICE_TOKEN",
        "DAYCARE_TOKEN_FILE",
        "DAYCARE_HOME",
        "DAYCARE_CHALLENGE_ROOT",
        "DAYCARE_SKIP_CLAUDE_PREFLIGHT",
        "OPENCODE_CONFIG_CONTENT",
        "OPENCODE_CONFIG_DIR",
        "XDG_CONFIG_HOME",
    ] {
        assert!(!env.contains_key(name), "{what} inherited {name}");
    }
    for (name, value) in env {
        assert!(!value.contains(TOKEN), "{what} carries the token in {name}");
    }
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").is_some(),
        keep_anthropic,
        "{what}: ANTHROPIC_API_KEY"
    );
    assert_eq!(
        env.get("DAYCARE_CHALLENGE_SESSION").map(String::as_str),
        Some("entry-1")
    );
    for switch in daycare_runner::challenge::opencode::CHALLENGE_SWITCHES {
        assert_eq!(env.get(switch).map(String::as_str), Some("1"), "{switch}");
    }
}

/// One open challenge, `c-1`, whose input the same mock serves as storage.
fn platform() -> MockPlatform {
    let base: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let seen_base = Arc::clone(&base);
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
        if request.method == "GET" && path == "/api/daycare/challenges/c-1" {
            return Response::json(
                200,
                &json!({
                    "challenge": {
                        "id": "c-1", "slug": "signup-rescue", "title": "Signup Rescue",
                        "state": "open", "brief_md": "Make the signup form work.",
                        "weights": {"vote": 40, "goal": 40, "third": 20},
                    },
                    "inputs": [{
                        "path": "form/data.csv", "bytes": CSV.len(), "sha256": sha256(CSV.as_bytes()),
                        "url": format!("{base}/storage/v1/object/sign/inputs/c-1/form/data.csv?token=signed"),
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

fn platform_calls(platform: &MockPlatform) -> usize {
    platform
        .requests()
        .iter()
        .filter(|request| !request.path.ends_with("/pair/claim"))
        .count()
}

#[test]
fn the_memory_scan_lists_opencode_memory_by_hash_without_contents() {
    let machine = Machine::new("oc-scan");
    machine.populate();
    let out = machine.run(&["memory", "setup", "--scan", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for secret in [
        MCP_SECRET,
        OPENAI_KEY,
        "OPENCODE-GLOBAL-RULES",
        "INLINE-AGENT-PROMPT",
        "HELPER-AGENT-PROMPT",
        "KEEP-SKILL-BODY",
        "docs.example.test",
    ] {
        assert!(!text.contains(secret), "scan leaked {secret:?}: {text}");
    }
    let scan: Value = serde_json::from_str(&text).unwrap();
    let listed: Vec<(String, Option<String>)> = scan["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["client"] == "opencode")
        .map(|item| {
            (
                item["kind"].as_str().unwrap().to_string(),
                item["name"].as_str().map(str::to_string),
            )
        })
        .collect();
    for (kind, name) in [
        ("agents_md", None),
        ("mcp_server", Some("search")),
        ("mcp_server", Some("docs")),
        ("agent", Some("reviewer")),
        ("agent", Some("helper")),
        ("skill", Some("keep")),
        ("skill", Some("notes")),
        ("plugin", None),
    ] {
        let wanted = (kind.to_string(), name.map(str::to_string));
        assert!(listed.contains(&wanted), "missing {wanted:?} in {listed:?}");
    }
    // The human listing names the client.
    let human = machine.run(&["memory", "setup", "--scan"]);
    assert!(stdout(&human).contains("opencode:"), "{}", stdout(&human));
}

#[test]
fn an_interactive_opencode_start_checks_what_opencode_resolved_then_runs_the_printed_command() {
    let machine = Machine::new("oc-launch");
    machine.populate();
    let platform = platform();
    machine.enroll(&platform);
    machine.approve_some();

    // --print first: the command the owner would run. Nothing starts.
    let printed = machine.start(MODEL, &["--print"]);
    assert!(
        printed.status.success(),
        "{}{}",
        stdout(&printed),
        stderr(&printed)
    );
    let printed = json_of(&printed);
    assert!(
        machine.recorded("call.argv").is_none(),
        "--print started it"
    );
    assert!(machine.recorded("debug-config.argv").is_none());
    let config_file = PathBuf::from(printed["settings_file"].as_str().unwrap());
    assert!(!config_file.starts_with(machine.folder()));
    assert!(config_file.starts_with(&machine.daycare_home));
    let notes = strings(&printed["notes"]).join("\n");
    assert!(notes.contains("own provider API key"), "{notes}");

    let out = machine.start(MODEL, &[]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let result = json_of(&out);
    // The client drew on stderr; stdout is still exactly one result.
    assert!(stderr(&out).contains("OPENCODE-SCREEN"), "{}", stderr(&out));
    assert_eq!(result["client"], "opencode");
    assert_eq!(result["entry_id"], "entry-1");
    assert_eq!(result["session"]["exit_code"], 0);

    // Exactly the command --print printed: no plugin approved, so --pure.
    let argv = lines(&machine.recorded("call.argv").unwrap());
    assert_eq!(argv, strings(&printed["launch"]["args"]));
    assert_eq!(argv, strings(&result["launch"]["args"]));
    assert_eq!(argv, ["--pure", "--model", MODEL]);
    assert_eq!(
        std::fs::canonicalize(machine.recorded("call.cwd").unwrap().trim()).unwrap(),
        std::fs::canonicalize(machine.folder()).unwrap()
    );
    let env = env_file(&machine.recorded("call.env").unwrap());
    assert_session_env(&env, "the OpenCode session", false);
    assert_eq!(
        env.get("OPENCODE_CONFIG").map(PathBuf::from),
        Some(config_file.clone())
    );
    // The owner's own key for the chosen provider, and HOME, are untouched.
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some(OPENAI_KEY)
    );
    assert_eq!(
        env.get("HOME"),
        Some(&machine.fake_home.to_string_lossy().into_owned())
    );

    // Both checks ran with the session's own flags and environment.
    for name in ["debug-config", "debug-agent"] {
        let argv = lines(&machine.recorded(&format!("{name}.argv")).unwrap());
        assert!(argv.contains(&"--pure".to_string()), "{name}: {argv:?}");
        let env = env_file(&machine.recorded(&format!("{name}.env")).unwrap());
        assert_session_env(&env, name, false);
        assert_eq!(
            env.get("OPENCODE_CONFIG").map(PathBuf::from),
            Some(config_file.clone())
        );
    }
    let proven = strings(&result["verification"]["proven"]).join("\n");
    for needle in [
        "opencode debug config",
        "opencode debug agent build",
        "approved (docs)",
        "the only provider it can use is openai",
        "--pure",
        "ones you approved (keep)",
    ] {
        assert!(proven.contains(needle), "missing {needle:?} in {proven}");
    }

    // Daycare's config: switches only, never a secret or the owner's settings.
    let config = read_json(&config_file);
    let folder_agents = machine.folder().join("AGENTS.md");
    assert_eq!(
        strings(&config["instructions"]),
        [folder_agents.to_string_lossy()]
    );
    assert_eq!(config["mcp"], json!({"search": {"enabled": false}}));
    assert_eq!(
        config["agent"],
        json!({"helper": {"disable": true}, "reviewer": {"disable": true}})
    );
    assert_eq!(
        config["permission"]["skill"],
        json!({"*": "deny", "keep": "allow", "notes": "deny"})
    );
    assert_eq!(config["enabled_providers"], json!(["openai"]));
    let config_text = std::fs::read_to_string(&config_file).unwrap();
    for secret in [MCP_SECRET, OPENAI_KEY, "docs.example.test", TOKEN] {
        assert!(!config_text.contains(secret), "{config_text}");
    }
    #[cfg(unix)]
    assert_eq!(mode(&config_file), 0o600);

    // The folder's pointer lists the approved copies; OpenCode reads it
    // through Daycare's config.
    let pointer = std::fs::read_to_string(&folder_agents).unwrap();
    assert!(pointer.contains("Skill `keep`"), "{pointer}");
    assert!(pointer.contains("MCP server `docs`"), "{pointer}");
    assert!(!machine.folder().join("CLAUDE.md").exists());
    let excluded: Vec<String> = result["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["how"].as_str().unwrap().to_string())
        .collect();
    assert!(
        excluded.iter().any(|how| how.starts_with("--pure")),
        "{excluded:?}"
    );

    // OpenCode has no weekly meter: both readings are written, as failed.
    for name in ["start", "end"] {
        let reading = read_json(&machine.folder().join(format!("meter/{name}.json")));
        assert_eq!(reading["ok"], false, "{reading}");
        assert_eq!(reading["client"], "opencode");
        assert!(
            reading["error"].as_str().unwrap().contains("API key"),
            "{reading}"
        );
    }
    let everything = format!("{}{}", stdout(&out), stderr(&out));
    assert!(!everything.contains(TOKEN) && !everything.contains(MCP_SECRET));
}

#[test]
fn an_anthropic_model_keeps_the_owners_anthropic_key() {
    let machine = Machine::new("oc-anthropic");
    machine.populate();
    let platform = platform();
    machine.enroll(&platform);
    machine.approve_some();
    let out = machine.start("anthropic/claude-sonnet-5", &[]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let env = env_file(&machine.recorded("call.env").unwrap());
    assert_session_env(&env, "the OpenCode session", true);
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some(ANTHROPIC_KEY)
    );
}

#[test]
fn what_opencode_cannot_leave_out_refuses_before_any_request() {
    let machine = Machine::new("oc-refuse");
    machine.populate();
    let platform = platform();
    machine.enroll(&platform);
    // Nothing approved: the global AGENTS.md is read in every session.
    let out = machine.start(MODEL, &["--print"]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(
        text.contains(".config/opencode/AGENTS.md") && text.contains("not approved"),
        "{text}"
    );
    assert!(!text.contains("OPENCODE-GLOBAL-RULES"), "{text}");

    // A custom tool is code OpenCode imports in every session.
    machine.write(".config/opencode/tools/lookup.ts", "export default {}\n");
    machine.approve_some();
    let out = machine.start(MODEL, &[]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("tools/lookup.ts"), "{}", stdout(&out));
    std::fs::remove_file(machine.config_dir().join("tools/lookup.ts")).unwrap();

    // A change to OpenCode's own build agent cannot be switched off.
    machine.write(".config/opencode/agents/build.md", "MY-BUILD-PROMPT\n");
    machine.approve_some();
    let out = machine.start(MODEL, &[]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(
        text.contains("build") && text.contains("cannot be switched off"),
        "{text}"
    );
    std::fs::remove_file(machine.config_dir().join("agents/build.md")).unwrap();

    // A subscription login for the chosen provider.
    machine.write(
        ".local/share/opencode/auth.json",
        &json!({"openai": {"type": "oauth", "access": "a", "refresh": "r", "expires": 1}})
            .to_string(),
    );
    let out = machine.start(MODEL, &[]);
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains("subscription login"),
        "{}",
        stdout(&out)
    );

    assert_eq!(platform_calls(&platform), 0);
    assert!(!machine.folder().exists());
    assert!(machine.recorded("call.argv").is_none());
}

#[test]
fn a_config_that_undoes_daycares_switches_refuses_the_launch_and_names_it() {
    let machine = Machine::new("oc-undo");
    machine.populate();
    let platform = platform();
    machine.enroll(&platform);
    machine.approve_some();
    let dot = machine.fake_home.join(".opencode/opencode.json");
    for (config, named) in [
        // ~/.opencode is read after OPENCODE_CONFIG, so it can switch a
        // server back on...
        (
            json!({"mcp": {"search": {"enabled": true}}}),
            "did not approve: search",
        ),
        // ...or allow every skill after Daycare's deny rule.
        (json!({"permission": {"*": "allow"}}), "notes"),
        // ...or bring an agent back.
        (json!({"agent": {"helper": {"disable": false}}}), "helper"),
    ] {
        std::fs::create_dir_all(dot.parent().unwrap()).unwrap();
        std::fs::write(&dot, config.to_string()).unwrap();
        // The scan sees the new file; approve the same things again.
        machine.approve_some();
        let out = machine.start(MODEL, &[]);
        assert!(!out.status.success(), "{config}: {}", stdout(&out));
        let text = stdout(&out);
        assert!(
            text.contains(named) && text.contains("Nothing was launched"),
            "{config}: {text}"
        );
        assert!(!text.contains(MCP_SECRET), "{text}");
        assert!(machine.recorded("call.argv").is_none(), "{config}");
    }
    std::fs::remove_file(&dot).unwrap();

    // Instruction files listed in the owner's own config cannot be left out.
    let global = machine.config_dir().join("opencode.json");
    let mut config = read_json(&global);
    config["instructions"] = json!(["~/team-rules.md"]);
    std::fs::write(&global, config.to_string()).unwrap();
    machine.approve_some();
    let out = machine.start(MODEL, &[]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("~/team-rules.md"), "{}", stdout(&out));

    // An OpenCode older than the one these checks were written against.
    config.as_object_mut().unwrap().remove("instructions");
    std::fs::write(&global, config.to_string()).unwrap();
    machine.approve_some();
    let out = machine.start_with(&[("FAKE_OPENCODE_VERSION", "1.18.0")], MODEL, &[]);
    assert!(!out.status.success());
    assert!(
        stdout(&out).contains("1.18.33 or newer"),
        "{}",
        stdout(&out)
    );
    assert!(machine.recorded("call.argv").is_none());

    // With everything back, it starts.
    let out = machine.start(MODEL, &[]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
}

#[test]
fn plugins_stay_out_with_pure_or_refuse_beside_an_approved_one() {
    let machine = Machine::new("oc-plugins");
    machine.populate();
    machine.write(".config/opencode/plugin/extra.js", "export default {}\n");
    let platform = platform();
    machine.enroll(&platform);
    // One plugin approved, one not: --pure would drop both, so it refuses.
    machine.approve(|item| {
        let opencode = item["client"] == "opencode";
        let chosen = item["kind"] == "agents_md"
            || item["path"].as_str().unwrap().ends_with("plugin/notify.ts");
        (opencode && chosen).then_some("private")
    });
    let out = machine.start(MODEL, &[]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(text.contains("plugin/extra.js"), "{text}");
    assert!(!text.contains("plugin/notify.ts"), "{text}");
    assert_eq!(platform_calls(&platform), 0);

    // Both approved: no --pure, and the resolved plugins are both approved.
    machine.approve(|item| {
        let opencode = item["client"] == "opencode";
        let chosen = item["kind"] == "agents_md" || item["kind"] == "plugin";
        (opencode && chosen).then_some("private")
    });
    let out = machine.start(MODEL, &[]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let argv = lines(&machine.recorded("call.argv").unwrap());
    assert_eq!(argv, ["--model", MODEL]);
    let checked = lines(&machine.recorded("debug-config.argv").unwrap());
    assert!(!checked.contains(&"--pure".to_string()), "{checked:?}");
    let result = json_of(&out);
    let proven = strings(&result["verification"]["proven"]).join("\n");
    assert!(!proven.contains("--pure"), "{proven}");
}
