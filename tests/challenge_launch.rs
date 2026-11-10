//! `challenge start` without `--print`: the interactive launch (N9), Codex
//! verification (N10), meter readings (N11), the session marker on the
//! remaining commands (N13), and the separate Codex home opt-in.
//!
//! The shipped binary runs against a mock platform, a fake HOME, and
//! recording fakes of `claude` and `codex`. No network, no keychain, no model,
//! never a real client: a bare `claude`, `codex` or `opencode` on PATH is a
//! shim that records it was reached and refuses, and the test then fails.

mod support;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, OnceLock};
use support::{MockPlatform, RecordedRequest, Response};

const BIN: &str = env!("CARGO_BIN_EXE_daycare-runner");
const TOKEN: &str = "dev_token_do_not_leak_9876";
const API_KEY: &str = "sk-ant-planted-api-key-4242";
/// Codex sessions drop it; a Claude session keeps the owner's own.
const OPENAI_KEY: &str = "sk-openai-planted-key-5353";
const CSV: &str = "name,email\nAda,ada@example.test\n";
const GLOBAL_AGENTS: &str = "GLOBAL-CODEX-RULES-do-not-load";

fn sha256(bytes: &[u8]) -> String {
    daycare_runner::self_update::sha256_hex(bytes)
}

/// A fake `codex`: `--version`, `debug prompt-input` (renders a fixture the
/// way Codex joins AGENTS.md files: the Codex home's, then the folder's),
/// `app-server` (the meter), and the interactive session, which records its
/// argv, environment and folder. Python, as `tests/codex_usage.rs` uses.
const FAKE_CODEX: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, sys
record = pathlib.Path('@RECORD@')
args = sys.argv[1:]
def dump_env(path):
    path.write_text(''.join(f'{k}={v}\n' for k, v in sorted(os.environ.items())))
if args == ['--version']:
    print('codex-cli 0.162.1'); sys.exit(0)
if args[:2] == ['debug', 'prompt-input']:
    (record / 'prompt.argv').write_text('\n'.join(args) + '\n')
    dump_env(record / 'prompt.env')
    home = pathlib.Path(os.environ.get('CODEX_HOME') or pathlib.Path(os.environ['HOME']) / '.codex')
    docs = []
    for name in ['AGENTS.override.md', 'AGENTS.md']:
        if (home / name).is_file():
            text = (home / name).read_text().strip()
            if text: docs.append(text)
            break
    # A project doc from somewhere Daycare's scan did not look.
    if os.environ.get('FAKE_CODEX_EXTRA_AGENTS'):
        docs.append(os.environ['FAKE_CODEX_EXTRA_AGENTS'])
    if pathlib.Path('AGENTS.md').is_file():
        docs.append(pathlib.Path('AGENTS.md').read_text().strip())
    joined = '\n\n--- project-doc ---\n\n'.join(docs)
    folder = os.getcwd()
    def fill(value):
        if isinstance(value, str):
            return value.replace('{{FOLDER}}', folder).replace('{{AGENTS_MD}}', joined)
        if isinstance(value, list): return [fill(v) for v in value]
        if isinstance(value, dict): return {k: fill(v) for k, v in value.items()}
        return value
    fixture = json.loads(pathlib.Path(os.environ['FAKE_CODEX_FIXTURE']).read_text())
    print(json.dumps(fill(fixture))); sys.exit(0)
if args == ['app-server']:
    for line in sys.stdin:
        req = json.loads(line)
        if 'id' not in req: continue
        method = req['method']
        with open(record / 'meter-calls', 'a') as calls: calls.write(method + '\n')
        if method == 'account/rateLimits/read' and os.environ.get('FAKE_CODEX_METER') == 'fail':
            print(json.dumps({'id': req['id'], 'error': {'message': 'mock meter failure'}}), flush=True)
            continue
        result = {
            'initialize': {},
            'account/read': {'account': {'type': 'chatgpt', 'planType': 'pro'}},
            'account/rateLimits/read': {'rateLimits': {'limitId': 'codex', 'primary':
                {'usedPercent': 14, 'windowDurationMins': 10080, 'resetsAt': 1791055017}}},
        }.get(method, {})
        print(json.dumps({'id': req['id'], 'result': result}), flush=True)
    sys.exit(0)
(record / 'call.argv').write_text('\n'.join(args) + '\n')
dump_env(record / 'call.env')
(record / 'call.cwd').write_text(os.getcwd() + '\n')
home = os.environ.get('CODEX_HOME')
if home:
    listing = sorted(p.name for p in pathlib.Path(home).iterdir())
    (record / 'call.codex-home').write_text('\n'.join(listing) + '\n')
    if os.environ.get('FAKE_CODEX_REFRESH'):
        path = pathlib.Path(home) / 'auth.json'
        auth = json.loads(path.read_text())
        auth['last_refresh'] = '2026-11-07T23:00:00Z'
        auth['tokens']['refresh_token'] = 'refreshed-fake'
        path.write_text(json.dumps(auth))
print('CODEX-SCREEN', flush=True)
sys.exit(int(os.environ.get('FAKE_CODEX_EXIT', '0')))
"#;

struct Machine {
    root: PathBuf,
    daycare_home: PathBuf,
    fake_home: PathBuf,
    token_file: PathBuf,
    path: String,
    claude_bin: PathBuf,
    codex_bin: PathBuf,
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
        Machine::with_claude(label, "")
    }

    /// `screen` is what the fake claude prints on stdout.
    fn with_claude(label: &str, screen: &str) -> Machine {
        let root = support::scratch_dir(label);
        let fake_home = root.join("fake-home");
        std::fs::create_dir_all(&fake_home).unwrap();
        let claude_bin = support::fake_claude(&root, screen, 0, 0);
        let path = support::no_claude_path(&root);
        let claude_marker = support::claude_marker(&root);
        // A bare codex or opencode must refuse as loudly as a bare claude.
        for name in ["codex", "opencode"] {
            support::testdir::write_executable(
                &root.join("no-claude-bin").join(name),
                &format!(
                    "#!/bin/sh\nprintf '{name} %s\\n' \"$*\" >> {}\nexit 97\n",
                    claude_marker.display()
                ),
            );
        }
        let record = root.join("codex-record");
        std::fs::create_dir_all(&record).unwrap();
        let codex_bin = root.join("fake-codex");
        support::testdir::write_executable(
            &codex_bin,
            &FAKE_CODEX.replace("@RECORD@", record.to_str().unwrap()),
        );
        Machine {
            daycare_home: root.join("daycare-state"),
            token_file: root.join("tokens.json"),
            path,
            claude_marker,
            claude_bin,
            codex_bin,
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

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .env("DAYCARE_HOME", &self.daycare_home)
            .env("DAYCARE_TOKEN_FILE", &self.token_file)
            .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
            .env("DAYCARE_CHALLENGE_ROOT", self.challenge_root())
            .env("PATH", &self.path)
            .env("HOME", &self.fake_home)
            // Planted in the runner's environment so the launch must strip them.
            .env("DAYCARE_DEVICE_TOKEN", TOKEN)
            .env("ANTHROPIC_API_KEY", API_KEY)
            .env("OPENAI_API_KEY", OPENAI_KEY)
            .env_remove("DAYCARE_WORKSPACE_ROOT")
            .env_remove("DAYCARE_CHALLENGE_SESSION")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .expect("run daycare-runner")
    }

    fn run_with(&self, env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = self.command();
        for (name, value) in env {
            command.env(name, value);
        }
        command.args(args).output().expect("run daycare-runner")
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
            json!({"version": 1, "written_at": "2026-11-07T12:00:00Z", "items": items}).to_string(),
        )
        .unwrap();
    }

    fn codex_login(&self, refresh_token: &str) {
        self.write(
            ".codex/auth.json",
            &json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null,
                "last_refresh": "2026-11-07T00:00:00Z",
                "tokens": {"account_id": "mock-account", "access_token": "access-fake",
                    "refresh_token": refresh_token, "id_token": "id"}})
            .to_string(),
        );
    }

    fn fixture(&self, name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    fn recorded(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.record.join(name)).ok()
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

fn physical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The child must never see a Daycare credential or the owner's API keys.
fn assert_clean_env(env: &HashMap<String, String>, what: &str) {
    for name in [
        "DAYCARE_DEVICE_TOKEN",
        "DAYCARE_TOKEN_FILE",
        "DAYCARE_HOME",
        "DAYCARE_CHALLENGE_ROOT",
        "DAYCARE_SKIP_CLAUDE_PREFLIGHT",
        "ANTHROPIC_API_KEY",
    ] {
        assert!(!env.contains_key(name), "{what} inherited {name}");
    }
    for (name, value) in env {
        assert!(!value.contains(TOKEN), "{what} carries the token in {name}");
        assert!(
            !value.contains(API_KEY),
            "{what} carries an API key in {name}"
        );
    }
    assert_eq!(
        env.get("DAYCARE_CHALLENGE_SESSION").map(String::as_str),
        Some("entry-1")
    );
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
fn an_interactive_claude_start_runs_the_printed_command_with_the_marker_and_no_token() {
    let machine = Machine::with_claude("launch-claude", "CLAUDE-SCREEN\n");
    machine.write(".claude/CLAUDE.md", "# PRIVATE-GLOBAL-RULES\n");
    machine.write(
        ".claude/skills/keep/SKILL.md",
        "---\nname: keep\n---\nKeep it.\n",
    );
    let platform = platform();
    machine.enroll(&platform);
    machine.approve(|item| (item["name"] == "keep").then_some("private"));
    let claude_bin = machine.claude_bin.to_str().unwrap().to_string();

    // --print first: the command the owner would run.
    let printed = machine.run(&[
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
    assert!(printed.status.success(), "{}", stdout(&printed));
    let printed = json_of(&printed);
    assert!(
        support::recorded_argv(&machine.root).is_empty(),
        "--print started the client"
    );

    let out = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--agent",
        "claude",
        "--claude-bin",
        &claude_bin,
        "--json",
    ]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    // The client drew on stderr; stdout is still exactly one result.
    let result = json_of(&out);
    assert!(stderr(&out).contains("CLAUDE-SCREEN"), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("/status and /memory"),
        "{}",
        stderr(&out)
    );
    assert_eq!(result["entry_id"], "entry-1");
    assert_eq!(result["session"]["exit_code"], 0);

    // Exactly the flags and settings file --print printed, in the folder.
    let argv = support::recorded_argv(&machine.root);
    assert_eq!(argv, strings(&printed["launch"]["args"]));
    assert_eq!(argv, strings(&result["launch"]["args"]));
    assert_eq!(argv[0], "--settings");
    assert_eq!(argv[1], printed["settings_file"].as_str().unwrap());
    assert!(!PathBuf::from(&argv[1]).starts_with(machine.folder()));
    assert!(argv.contains(&"--strict-mcp-config".to_string()));
    assert_eq!(
        PathBuf::from(support::recorded_cwd(&machine.root)),
        physical(&machine.folder())
    );
    let env = support::recorded_env(&machine.root);
    assert_clean_env(&env, "the Claude session");
    // The owner's other tools keep their own settings in a Claude session.
    assert_eq!(
        env.get("OPENAI_API_KEY").map(String::as_str),
        Some(OPENAI_KEY)
    );
    assert_eq!(
        env.get("CLAUDE_CODE_DISABLE_AUTO_MEMORY")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        env.get("HOME"),
        Some(&machine.fake_home.to_string_lossy().into_owned())
    );

    // Readings before and after, both taken with the /usage reader.
    let start = read_json(&machine.folder().join("meter/start.json"));
    let end = read_json(&machine.folder().join("meter/end.json"));
    for reading in [&start, &end] {
        assert_eq!(reading["ok"], true, "{reading}");
        assert_eq!(reading["used_percentage"], 64.0);
        assert_eq!(reading["client"], "claude");
        assert_eq!(reading["entry_id"], "entry-1");
    }
    assert_eq!(start["reading"], "start");
    assert_eq!(end["session"]["exit_code"], 0);
    assert_eq!(result["meter"]["end"]["used_percentage"], 64.0);
    let usage_calls = std::fs::read_to_string(machine.root.join("usage-calls")).unwrap();
    assert_eq!(usage_calls.lines().count(), 2, "{usage_calls}");
    #[cfg(unix)]
    {
        assert_eq!(mode(&machine.folder().join("meter")), 0o700);
        assert_eq!(mode(&machine.folder().join("meter/end.json")), 0o600);
    }
    // Nothing secret in what was written or printed.
    let text = format!(
        "{}{}{}",
        stdout(&out),
        serde_json::to_string(&start).unwrap(),
        serde_json::to_string(&end).unwrap()
    );
    assert!(!text.contains(TOKEN) && !text.contains(API_KEY), "{text}");
}

#[test]
fn a_human_start_says_what_to_check_and_where_the_readings_are() {
    let machine = Machine::new("launch-human");
    let platform = platform();
    machine.enroll(&platform);
    let claude_bin = machine.claude_bin.to_str().unwrap().to_string();
    let out = machine.run(&[
        "challenge",
        "start",
        "c-1",
        "--agent",
        "claude",
        "--claude-bin",
        &claude_bin,
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for needle in [
        "Run /status and /memory",
        "Starting Claude Code in",
        "Claude Code ended with exit code 0",
        "a reading, not a limit",
        "meter",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in {text}");
    }
    assert!(!text.contains(TOKEN));
}

#[test]
fn an_interactive_codex_start_proves_its_prompt_then_launches_and_records_a_failed_reading() {
    let machine = Machine::new("launch-codex");
    machine.codex_login("owner-refresh");
    machine.write(
        ".codex/config.toml",
        "[mcp_servers.search]\ncommand = \"s\"\nenv = { KEY = \"codex-secret-777\" }\n",
    );
    let platform = platform();
    machine.enroll(&platform);
    let codex_bin = machine.codex_bin.to_str().unwrap().to_string();
    let fixture = machine.fixture("codex-prompt-input-challenge.json");
    let out = machine.run_with(
        &[
            ("FAKE_CODEX_FIXTURE", &fixture),
            ("FAKE_CODEX_METER", "fail"),
        ],
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let result = json_of(&out);
    assert!(stderr(&out).contains("CODEX-SCREEN"), "{}", stderr(&out));

    // The prompt was rendered with the launch's own switches, in the folder.
    let launch_args = strings(&result["launch"]["args"]);
    let probe = lines(
        &machine
            .recorded("prompt.argv")
            .expect("no prompt-input run"),
    );
    assert_eq!(probe[..2], ["debug", "prompt-input"]);
    assert_eq!(probe[2..probe.len() - 3], launch_args[4..]);
    assert_eq!(
        probe[probe.len() - 3..],
        [
            "-c",
            "sandbox_mode=\"workspace-write\"",
            "DAYCARE-CHALLENGE-PROBE"
        ]
    );
    assert!(launch_args
        .windows(2)
        .any(|w| w == ["-c", "mcp_servers.search.enabled=false"]));
    assert_clean_env(
        &env_file(&machine.recorded("prompt.env").unwrap()),
        "the prompt check",
    );
    let proven = strings(&result["verification"]["proven"]);
    assert!(
        proven
            .iter()
            .any(|line| line.contains("this folder's AGENTS.md")),
        "{proven:?}"
    );

    // Then the session: the same command, the marker, no token, no API key.
    assert_eq!(lines(&machine.recorded("call.argv").unwrap()), launch_args);
    let env = env_file(&machine.recorded("call.env").unwrap());
    assert_clean_env(&env, "the Codex session");
    assert!(!env.contains_key("OPENAI_API_KEY"));
    assert!(
        !env.contains_key("CODEX_HOME"),
        "no opt-in, so the owner's own home"
    );
    assert_eq!(
        PathBuf::from(machine.recorded("call.cwd").unwrap().trim()),
        physical(&machine.folder())
    );

    // The app-server meter failed both times; both files say so.
    for name in ["start", "end"] {
        let reading = read_json(&machine.folder().join(format!("meter/{name}.json")));
        assert_eq!(reading["ok"], false, "{reading}");
        assert!(
            reading["error"]
                .as_str()
                .unwrap()
                .contains("mock meter failure"),
            "{reading}"
        );
        assert!(reading.get("used_percentage").is_none());
    }
    let end = read_json(&machine.folder().join("meter/end.json"));
    assert_eq!(end["session"]["started"], true);
    assert!(machine
        .recorded("meter-calls")
        .unwrap()
        .contains("account/rateLimits/read"));
    assert!(!stdout(&out).contains("codex-secret-777"));

    // A working meter: the readings are numbers.
    let out = machine.run_with(
        &[("FAKE_CODEX_FIXTURE", &fixture)],
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(out.status.success(), "{}", stdout(&out));
    let start = read_json(&machine.folder().join("meter/start.json"));
    assert_eq!(start["ok"], true);
    assert_eq!(start["used_percentage"], 14.0);
    assert_eq!(start["meter_key"], "codex_weekly:codex");
}

#[test]
fn an_unapproved_skill_in_the_codex_prompt_refuses_the_launch_and_names_it() {
    let machine = Machine::new("launch-codex-skill");
    machine.codex_login("owner-refresh");
    machine.write(
        ".agents/skills/notes/SKILL.md",
        "---\nname: notes\n---\nTake notes.\n",
    );
    let platform = platform();
    machine.enroll(&platform);
    let codex_bin = machine.codex_bin.to_str().unwrap().to_string();
    let fixture = machine.fixture("codex-prompt-input-challenge-skill.json");
    let out = machine.run_with(
        &[("FAKE_CODEX_FIXTURE", &fixture)],
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(!out.status.success(), "{}", stdout(&out));
    let error = json_of(&out)["error"].as_str().unwrap().to_string();
    assert!(
        error.contains("skills you did not approve: notes"),
        "{error}"
    );
    assert!(
        machine.recorded("call.argv").is_none(),
        "Codex was started anyway"
    );
    assert!(!machine.folder().join("meter/start.json").exists());

    // Approved, the same prompt passes and Codex starts.
    machine.approve(|item| (item["name"] == "notes").then_some("private"));
    let out = machine.run_with(
        &[("FAKE_CODEX_FIXTURE", &fixture)],
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(machine.recorded("call.argv").is_some());
}

#[test]
fn a_separate_codex_home_holds_only_approved_parts_and_hands_the_login_back() {
    let machine = Machine::new("launch-codex-home");
    machine.codex_login("owner-refresh");
    machine.write(".codex/AGENTS.md", &format!("{GLOBAL_AGENTS}\n"));
    machine.write(
        ".codex/config.toml",
        "model = \"gpt\"\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n\n[mcp_servers.search]\ncommand = \"s\"\nenv = { KEY = \"codex-secret-777\" }\n",
    );
    let platform = platform();
    machine.enroll(&platform);
    machine.approve(|item| (item["name"] == "docs").then_some("private"));
    let codex_bin = machine.codex_bin.to_str().unwrap().to_string();
    let fixture = machine.fixture("codex-prompt-input-challenge.json");
    let env = [
        ("FAKE_CODEX_FIXTURE", fixture.as_str()),
        ("FAKE_CODEX_REFRESH", "1"),
    ];

    // Without the opt-in, the unapproved global AGENTS.md still refuses,
    // before any request, and the refusal names the opt-in.
    let refused = machine.run_with(
        &env,
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(!refused.status.success());
    let error = json_of(&refused)["error"].as_str().unwrap().to_string();
    assert!(
        error.contains(".codex/AGENTS.md") && error.contains("--separate-codex-home"),
        "{error}"
    );
    // Not with --print (the login could not be handed back), nor for Claude.
    for args in [
        vec![
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--separate-codex-home",
            "--print",
            "--json",
        ],
        vec![
            "challenge",
            "start",
            "c-1",
            "--agent",
            "claude",
            "--separate-codex-home",
            "--json",
        ],
    ] {
        let out = machine.run_with(&env, &args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            stdout(&out).contains("--separate-codex-home"),
            "{}",
            stdout(&out)
        );
    }
    assert_eq!(
        platform_calls(&platform),
        0,
        "a refused start reached the platform"
    );

    let out = machine.run_with(
        &env,
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--separate-codex-home",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let result = json_of(&out);

    // The session ran with its own CODEX_HOME, under Daycare's folder.
    let home = machine.daycare_home.join("challenges/c-1/codex-home");
    assert_eq!(result["codex_home"], home.to_str().unwrap());
    let session_env = env_file(&machine.recorded("call.env").unwrap());
    assert_clean_env(&session_env, "the Codex session");
    assert_eq!(
        session_env.get("CODEX_HOME"),
        Some(&home.to_string_lossy().into_owned())
    );
    assert_eq!(
        session_env.get("HOME"),
        Some(&machine.fake_home.to_string_lossy().into_owned())
    );
    assert!(!home.starts_with(machine.folder()));
    assert!(!physical(&home).starts_with(physical(&machine.folder())));
    let prompt_env = env_file(&machine.recorded("prompt.env").unwrap());
    assert_eq!(prompt_env.get("CODEX_HOME"), session_env.get("CODEX_HOME"));

    // It holds the login and the approved server, nothing else.
    assert_eq!(
        lines(&machine.recorded("call.codex-home").unwrap()),
        ["auth.json", "auth.lock", "config.toml"]
    );
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(config.contains("[mcp_servers.docs]"), "{config}");
    assert!(
        !config.contains("search") && !config.contains("codex-secret-777"),
        "{config}"
    );
    assert!(!config.contains("model"), "{config}");
    assert!(!home.join("AGENTS.md").exists());
    let launch_args = strings(&result["launch"]["args"]);
    assert!(
        !launch_args.iter().any(|a| a.starts_with("mcp_servers.")),
        "{launch_args:?}"
    );
    #[cfg(unix)]
    {
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join("auth.json")), 0o600);
    }
    // Nothing of the login is in the challenge folder.
    for entry in walk(&machine.folder()) {
        let text = std::fs::read_to_string(&entry).unwrap_or_default();
        for secret in ["access-fake", "owner-refresh", "refreshed-fake"] {
            assert!(
                !text.contains(secret),
                "{} holds the login",
                entry.display()
            );
        }
    }

    // The session refreshed its login; the owner's own copy got it back, so
    // their own `codex` stays logged in.
    let owner = read_json(&machine.fake_home.join(".codex/auth.json"));
    assert_eq!(owner["tokens"]["refresh_token"], "refreshed-fake");
    assert_eq!(read_json(&home.join("auth.json")), owner);

    // A second start rewrites the home: an approval withdrawn is gone.
    machine.approve(|_| None);
    let out = machine.run_with(
        &env,
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--separate-codex-home",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(out.status.success(), "{}", stdout(&out));
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(!config.contains("docs"), "{config}");

    // An approved global AGENTS.md is copied in, and the prompt check matches
    // it by hash.
    machine.approve(|item| (item["kind"] == "agents_md").then_some("private"));
    let out = machine.run_with(
        &env,
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--separate-codex-home",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(out.status.success(), "{}", stdout(&out));
    assert_eq!(
        std::fs::read_to_string(home.join("AGENTS.md")).unwrap(),
        format!("{GLOBAL_AGENTS}\n")
    );
    let proven = strings(&json_of(&out)["verification"]["proven"]);
    assert!(
        proven
            .iter()
            .any(|line| line.contains("which you approved")),
        "{proven:?}"
    );
}

#[test]
fn agents_md_text_codex_loads_is_matched_by_hash_and_unmatched_text_refuses_unquoted() {
    let machine = Machine::new("launch-codex-agents");
    machine.codex_login("owner-refresh");
    machine.write(".codex/AGENTS.md", &format!("{GLOBAL_AGENTS}\n"));
    let platform = platform();
    machine.enroll(&platform);
    machine.approve(|item| (item["kind"] == "agents_md").then_some("private"));
    let codex_bin = machine.codex_bin.to_str().unwrap().to_string();
    let fixture = machine.fixture("codex-prompt-input-challenge.json");

    // Approved, the owner's own Codex home loads it natively, and the prompt
    // check matches it by hash.
    let out = machine.run_with(
        &[("FAKE_CODEX_FIXTURE", &fixture)],
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let proven = strings(&json_of(&out)["verification"]["proven"]);
    assert!(
        proven
            .iter()
            .any(|line| line.contains(".codex/AGENTS.md, which you approved")),
        "{proven:?}"
    );
    std::fs::remove_file(machine.record.join("call.argv")).unwrap();

    // Codex also loads instructions from somewhere the scan did not see: the
    // launch is refused, the text is not quoted, and Codex never starts.
    let out = machine.run_with(
        &[
            ("FAKE_CODEX_FIXTURE", &fixture),
            ("FAKE_CODEX_EXTRA_AGENTS", "UNSEEN-PROJECT-RULES"),
        ],
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--codex-bin",
            &codex_bin,
            "--json",
        ],
    );
    assert!(!out.status.success());
    let error = json_of(&out)["error"].as_str().unwrap().to_string();
    assert!(error.contains("matches nothing you approved"), "{error}");
    assert!(
        !error.contains("UNSEEN-PROJECT-RULES"),
        "the refusal quoted memory: {error}"
    );
    assert!(
        machine.recorded("call.argv").is_none(),
        "Codex was started anyway"
    );
}

#[test]
fn run_run_once_enroll_and_update_refuse_inside_a_challenge_before_any_request() {
    let machine = Machine::new("launch-marker");
    let platform = platform();
    machine.enroll(&platform);
    let before = platform.requests().len();
    let claude_bin = machine.claude_bin.to_str().unwrap().to_string();
    for args in [
        vec!["run", "--claude-bin", &claude_bin, "--json"],
        vec!["run-once", "--claude-bin", &claude_bin, "--json"],
        vec![
            "enroll",
            "--url",
            &platform.base_url,
            "--code",
            "PAIR-9999",
            "--json",
        ],
        vec!["update", "--json"],
        vec!["update", "--url", &platform.base_url, "--json"],
    ] {
        let out = machine.run_with(&[("DAYCARE_CHALLENGE_SESSION", "entry-1")], &args);
        assert!(!out.status.success(), "{args:?}");
        let error = json_of(&out);
        assert_eq!(error["ok"], false);
        let message = error["error"].as_str().unwrap();
        assert!(
            message.contains("inside a challenge session"),
            "{args:?}: {message}"
        );
        assert!(message.contains("entry-1"), "{message}");
    }
    assert_eq!(
        platform.requests().len(),
        before,
        "a marked command reached the platform"
    );
    assert!(
        support::recorded_argv(&machine.root).is_empty(),
        "a marked command ran claude"
    );
    // An empty marker is no marker: `update` on a dev build answers itself.
    let out = machine.run_with(&[("DAYCARE_CHALLENGE_SESSION", "")], &["update", "--json"]);
    assert!(out.status.success(), "{}", stdout(&out));
}

#[test]
fn a_client_that_fails_still_gets_its_end_reading_and_its_exit_code_reported() {
    let machine = Machine::new("launch-exit");
    machine.codex_login("owner-refresh");
    let platform = platform();
    machine.enroll(&platform);
    let codex_bin = machine.codex_bin.to_str().unwrap().to_string();
    let fixture = machine.fixture("codex-prompt-input-challenge.json");
    let out = machine.run_with(
        &[("FAKE_CODEX_FIXTURE", &fixture), ("FAKE_CODEX_EXIT", "3")],
        &[
            "challenge",
            "start",
            "c-1",
            "--agent",
            "codex",
            "--codex-bin",
            &codex_bin,
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Codex ended with exit code 3"),
        "{}",
        stdout(&out)
    );
    let end = read_json(&machine.folder().join("meter/end.json"));
    assert_eq!(end["session"]["started"], true);
    assert_eq!(end["session"]["exit_code"], 3);
    assert_eq!(end["ok"], true);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
