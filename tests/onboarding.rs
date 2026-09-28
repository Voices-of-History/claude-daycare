//! Real CLI onboarding with isolated homes, fake executables and a local server.
mod support;
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};
use support::{MockPlatform, Response};
const BIN: &str = env!("CARGO_BIN_EXE_daycare-runner");
struct Machine {
    root: PathBuf,
    platform: MockPlatform,
}
impl Machine {
    fn new() -> Self {
        let root = support::scratch_dir("onboarding");
        fs::create_dir_all(root.join("bin")).unwrap();
        let platform = MockPlatform::start(|request| {
            if request.path.ends_with("/visits") {
                return Response::json(
                    400,
                    r#"{"error":"fixture stops before launching a worker"}"#,
                );
            }
            Response::json(
                200,
                r#"{"device_token":"fixture-device","device_id":"device-1","actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}"#,
            )
        });
        Self { root, platform }
    }
    fn agent(&self, name: &str, ready: bool) {
        let bin = self.root.join("bin").join(name);
        let auth = if ready {
            r#"{"loggedIn":true,"subscriptionType":"pro"}"#
        } else {
            r#"{"loggedIn":false}"#
        };
        support::testdir::write_executable(&bin, &format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture; else printf '%s\\n' '{auth}'; fi\n"));
        if ready && name != "claude" {
            let (path, value) = if name == "codex" {
                (
                    ".codex/auth.json",
                    json!({"auth_mode":"chatgpt","tokens":{"access_token":"fixture-access","refresh_token":"fixture-refresh","account_id":"fixture-account"}}),
                )
            } else {
                (
                    ".local/share/opencode/auth.json",
                    json!({"openai":{"type":"oauth","access":"fixture-access","refresh":"fixture-refresh","expires":9999999999999u64}}),
                )
            };
            let path = self.root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, value.to_string()).unwrap();
        }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env_clear()
            .env("HOME", &self.root)
            .env("PATH", self.root.join("bin"))
            .env("DAYCARE_HOME", self.root.join("daycare"))
            .env("DAYCARE_TOKEN_FILE", self.root.join("tokens.json"));
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn enroll(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "enroll",
            "--url",
            &self.platform.base_url,
            "--code",
            "fixture-code",
            "--json",
        ];
        args.extend(extra);
        self.run(&args)
    }
    fn config(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.join("daycare/config.json")).unwrap()).unwrap()
    }
}
#[test]
fn any_single_ready_agent_can_enroll_and_becomes_the_default() {
    for name in ["claude", "codex", "opencode"] {
        let m = Machine::new();
        m.agent(name, true);
        let out = m.enroll(&[]);
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(m.config()["default_agent"], name);
        assert_eq!(m.config()["installed_agents"], json!([name]));
        assert_eq!(m.config()["ready_agents"], json!([name]));
    }
}
#[test]
fn no_ready_agent_never_consumes_a_pairing_code() {
    for installed in [false, true] {
        let m = Machine::new();
        if installed {
            for name in ["claude", "codex", "opencode"] {
                m.agent(name, false);
            }
        }
        let out = m.enroll(&[]);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stdout);
        for name in ["Claude Code", "Codex", "OpenCode"] {
            assert!(err.contains(name), "{err}");
        }
        assert!(m.platform.requests().is_empty());
        assert!(!m.root.join("daycare/config.json").exists());
    }
}
#[test]
fn multiple_ready_agents_require_a_choice_before_pairing() {
    let m = Machine::new();
    m.agent("claude", true);
    m.agent("codex", true);
    assert!(!m.enroll(&[]).status.success());
    assert!(m.platform.requests().is_empty());
    let out = m.enroll(&["--agent", "codex"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(m.config()["default_agent"], "codex");
    assert_eq!(m.config()["ready_agents"], json!(["claude", "codex"]));
}
#[test]
fn a_logged_out_claude_does_not_block_codex_and_setup_changes_the_default() {
    let m = Machine::new();
    m.agent("claude", false);
    m.agent("codex", true);
    assert!(m.enroll(&[]).status.success());
    assert_eq!(m.config()["installed_agents"], json!(["claude", "codex"]));
    assert_eq!(m.config()["ready_agents"], json!(["codex"]));
    m.agent("opencode", true);
    assert!(m.run(&["setup", "--agent", "opencode"]).status.success());
    assert_eq!(m.config()["default_agent"], "opencode");
    // A visit without --agent now reaches OpenCode's explicit-model check.
    let out = m.run(&["visit", "start", "--json"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("--model provider/model"),
        "{out:?}"
    );
    // An explicit Claude selection overrides the default.
    let out = m.run(&[
        "visit", "start", "--agent", "claude", "--model", "invalid", "--json",
    ]);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("--model provider/model"));
}
#[test]
fn skill_installs_one_source_in_all_three_documented_locations() {
    let m = Machine::new();
    let out = m.run(&["skill", "install", "--json"]);
    assert!(out.status.success());
    for path in [
        ".claude/skills/daycare/SKILL.md",
        ".agents/skills/daycare/SKILL.md",
        ".config/opencode/skills/daycare/SKILL.md",
    ] {
        assert_eq!(
            fs::read_to_string(m.root.join(path)).unwrap(),
            include_str!("../skill/SKILL.md")
        );
    }
}

#[test]
fn opencode_default_and_token_override_reach_the_real_start_request() {
    let m = Machine::new();
    m.agent("opencode", true);
    assert!(m.enroll(&[]).status.success());
    for (extra, tokens) in [(vec![], 300000), (vec!["--tokens", "1234"], 1234)] {
        let mut args = vec!["visit", "start", "--model", "openai/fixture", "--json"];
        args.extend(extra);
        let out = m.run(&args);
        assert!(!out.status.success()); // mock intentionally refuses before a worker starts
        let requests = m.platform.requests();
        let body = requests
            .iter()
            .rev()
            .find(|r| r.path.ends_with("/visits"))
            .expect("visit start POST")
            .json();
        assert_eq!(body["agent_kind"], "opencode");
        assert_eq!(body["agent_model"], "openai/fixture");
        assert_eq!(body["budget_tokens"], tokens);
        assert!(body.get("budget_usage_pct").is_none());
    }
    let before = m.platform.requests().len();
    let out = m.run(&[
        "visit",
        "start",
        "--model",
        "openai/fixture",
        "--weekly-percent",
        "2",
        "--json",
    ]);
    assert!(!out.status.success());
    assert_eq!(m.platform.requests().len(), before);
}

#[test]
fn unavailable_choice_does_not_claim_or_overwrite_the_saved_default() {
    let m = Machine::new();
    m.agent("codex", true);
    assert!(!m.enroll(&["--agent", "opencode"]).status.success());
    assert!(m.platform.requests().is_empty());
    assert!(m.enroll(&[]).status.success());
    let original = m.config();
    assert!(!m
        .run(&["setup", "--agent", "opencode", "--json"])
        .status
        .success());
    assert_eq!(m.config(), original);
}

#[test]
fn malformed_logins_and_api_key_codex_auth_fail_without_printing_credentials() {
    for (name, path, auth) in [
        ("claude", "unused", "not-json"),
        (
            "codex",
            ".codex/auth.json",
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-private-value"}"#,
        ),
        ("codex", ".codex/auth.json", r#"{"tokens":{}}"#),
        (
            "opencode",
            ".local/share/opencode/auth.json",
            r#"{"openai":{"type":"oauth","access":"fixture-private-value"}}"#,
        ),
        (
            "opencode",
            ".local/share/opencode/auth.json",
            "malformed fixture-private-value",
        ),
    ] {
        let m = Machine::new();
        m.agent(name, true);
        if name == "claude" {
            support::testdir::write_executable(
                &m.root.join("bin/claude"),
                "#!/bin/sh\necho fixture-private-value\n",
            );
        } else {
            fs::write(m.root.join(path), auth).unwrap();
        }
        let out = m.enroll(&[]);
        assert!(!out.status.success());
        for bytes in [&out.stdout, &out.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains("fixture-private-value"));
        }
        assert!(m.platform.requests().is_empty());
    }
}

#[test]
fn legacy_config_and_explicit_open_keep_their_expected_agent() {
    let m = Machine::new();
    m.agent("codex", true);
    assert!(m.enroll(&[]).status.success());
    for (extra, expected) in [(vec![], "codex"), (vec!["--agent", "opencode"], "opencode")] {
        let mut args = vec!["open", "--json"];
        args.extend(extra);
        let out = m.run(&args);
        assert!(out.status.success(), "{out:?}");
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["agent"], expected);
    }
    let mut legacy = m.config();
    for key in ["default_agent", "installed_agents", "ready_agents"] {
        legacy.as_object_mut().unwrap().remove(key);
    }
    fs::write(m.root.join("daycare/config.json"), legacy.to_string()).unwrap();
    let out = m.run(&["open", "--json"]);
    assert!(out.status.success());
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["agent"], "claude");
}

#[test]
fn xdg_skill_destination_is_checked_before_any_library_is_written() {
    let m = Machine::new();
    let xdg = m.root.join("custom-config");
    let native = xdg.join("opencode/skills/daycare/SKILL.md");
    fs::create_dir_all(native.parent().unwrap()).unwrap();
    fs::write(&native, "someone else's skill").unwrap();
    let out = m
        .command()
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["skill", "install", "--json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!m.root.join(".agents/skills/daycare/SKILL.md").exists());
    assert_eq!(fs::read_to_string(&native).unwrap(), "someone else's skill");
    fs::write(&native, "---\nname: daycare\n---\nOld version").unwrap();
    let out = m
        .command()
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["skill", "install", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        fs::read_to_string(&native).unwrap(),
        include_str!("../skill/SKILL.md")
    );
    assert_eq!(
        fs::read_to_string(native.with_extension("md.bak")).unwrap(),
        "---\nname: daycare\n---\nOld version"
    );
}

#[test]
fn custom_login_locations_match_the_adapters() {
    for (name, relative, variable) in [
        ("codex", ".codex", "CODEX_HOME"),
        ("opencode", ".local/share", "XDG_DATA_HOME"),
    ] {
        let m = Machine::new();
        m.agent(name, true);
        let custom = m.root.join("custom-login");
        fs::rename(m.root.join(relative), &custom).unwrap();
        assert!(!m.enroll(&[]).status.success());
        let out = m
            .command()
            .env(variable, &custom)
            .args([
                "enroll",
                "--url",
                &m.platform.base_url,
                "--code",
                "fixture-code",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{name}: {out:?}");
        assert_eq!(m.config()["default_agent"], name);
    }
}

#[test]
fn a_hung_claude_login_probe_does_not_block_a_ready_codex() {
    let m = Machine::new();
    m.agent("claude", true);
    m.agent("codex", true);
    support::testdir::write_executable(
        &m.root.join("bin/claude"),
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture; else exec /bin/sleep 30; fi\n",
    );
    let start = std::time::Instant::now();
    let out = m.enroll(&[]);
    assert!(out.status.success(), "{out:?}");
    assert!(start.elapsed() < std::time::Duration::from_secs(15));
    assert_eq!(m.config()["default_agent"], "codex");
}
