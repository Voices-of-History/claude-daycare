//! Exercise the real orchestrator and Antigravity adapter with a local fake
//! `agy`. No Google sign-in, no model, no network.
mod support;

use daycare_runner::agent::antigravity::AntigravityAgent;
use daycare_runner::launch::SessionMode;
use daycare_runner::paths::Layout;
use daycare_runner::turn::{run_turn, TurnOutcome, TurnPurpose, TurnRequest};
use daycare_runner::workspace::Workspace;
use std::path::PathBuf;
use std::time::Duration;

struct Harness {
    dir: PathBuf,
    workspace: Workspace,
    agent: AntigravityAgent,
}

impl Harness {
    fn new(scenario: &str) -> Self {
        let dir = support::scratch_dir("antigravity-turn");
        let workspace = Workspace::new(dir.join("workspace"));
        workspace.scaffold("Pip", "http://127.0.0.1:1/mcp").unwrap();
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/antigravity-1.5.13");
        std::fs::copy(fixtures.join("world.jsonl"), dir.join("stream.jsonl")).unwrap();
        std::fs::write(dir.join("scenario"), scenario).unwrap();
        let bin = dir.join("fake-agy");
        support::testdir::write_executable(
            &bin,
            r##"#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
scenario = (root / 'scenario').read_text()
args = sys.argv[1:]
if args == ['--version']:
    print({'old': '1.5.12', 'new-major': '2.0.0'}.get(scenario, '1.5.13')); sys.exit(0)
home = pathlib.Path(os.environ['HOME'])
assert 'DAYCARE_DEVICE_TOKEN' not in os.environ or args[:1] != ['models']
if args == ['models']:
    if scenario == 'signed-out':
        print('Error: Please sign in to view available models.'); sys.exit(1)
    print('gemini-3.8-flash-medium   Gemini 3.8 Flash (Medium)'); sys.exit(0)
with (root / 'calls').open('a') as log:
    log.write(json.dumps(args) + '\n')
gemini = home / '.gemini'
assert (home / 'Library' / 'Keychains').is_symlink()
settings = json.loads((gemini / 'antigravity-cli' / 'settings.json').read_text())
assert settings['toolPermission'] == 'strict' and settings['useG1Credits'] is False
agent = (gemini / 'config' / 'agents' / 'daycare.md').read_text()
assert 'tools:\n  - finish\n' in agent and 'Pip' in agent
assert args[args.index('--agent') + 1] == 'daycare'
config = gemini / 'config' / 'mcp_config.json'
(root / 'saw-mcp').write_text(config.read_text() if config.exists() else 'none')
(root / 'settings.json').write_text(json.dumps(settings))
(root / 'stdin').write_text(sys.stdin.read())
stream = (root / 'stream.jsonl').read_text()
if scenario == 'shell':
    stream = stream.replace('"ToolName": "daycare_world_snapshot"', '"ToolName": "x"').replace(
        '"tool_name": "call_mcp_tool", "tool_info": {"name": "call_mcp_tool", "parameters": {"ServerName": "daycare", "ToolName": "x"',
        '"tool_name": "run_command", "tool_info": {"name": "run_command", "parameters": {"CommandLine": "ls ~"')
if scenario == 'built-in':
    extra = json.dumps({"event": "step_update", "step_update": {"conversation_id": "c0ffee00-0000-4000-8000-000000000001", "step_index": 9, "state": "DONE", "step_type": "tool", "tool_name": "manage_task", "tool_info": {"name": "manage_task", "parameters": {}, "output": "ok"}}})
    lines = stream.splitlines()
    at = next(i for i, line in enumerate(lines) if '"event": "result"' in line)
    stream = '\n'.join(lines[:at] + [extra] + lines[at:]) + '\n'
if scenario == 'day-report':
    stream = '\n'.join(line for line in stream.splitlines() if '"tool"' not in line) + '\n'
for line in stream.splitlines():
    print(line, flush=True)
"##,
        );
        let agent = AntigravityAgent::new(bin.to_str().unwrap(), &Layout::at(&dir));
        Self {
            dir,
            workspace,
            agent,
        }
    }

    fn run(&self, purpose: TurnPurpose, mode: SessionMode) -> daycare_runner::Result<TurnOutcome> {
        run_turn(TurnRequest {
            agent: &self.agent,
            workspace: &self.workspace,
            mode,
            message: "Look around.",
            actor_name: "Pip",
            device_token: "mock-device-token",
            archive_path: &self.dir.join("turn.jsonl"),
            timeout: Duration::from_secs(20),
            purpose,
            model: "gemini-3.8-flash-medium",
            mcp_settle: Duration::ZERO,
            token_ceiling: Some(300_000),
        })
    }

    fn new_session() -> SessionMode {
        SessionMode::New {
            reserved_session_id: "unused".into(),
        }
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    fn token_files_left(&self) -> usize {
        walk(&self.dir.join("antigravity"))
            .into_iter()
            .filter(|path| path.ends_with("mcp_config.json"))
            .count()
    }
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Never follow the keychain link out of the seal.
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_world_turn_runs_sealed_and_the_token_file_is_gone_after() {
    let h = Harness::new("world");
    let outcome = h.run(TurnPurpose::World, Harness::new_session()).unwrap();
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    assert_eq!(
        outcome.session_id(),
        Some("c0ffee00-0000-4000-8000-000000000001")
    );
    // The child saw exactly one server, with this turn's token and tools.
    let config: serde_json::Value = serde_json::from_str(&h.read("saw-mcp")).unwrap();
    let servers = config["mcpServers"].as_object().unwrap();
    assert_eq!(servers.keys().collect::<Vec<_>>(), ["daycare"]);
    assert_eq!(
        servers["daycare"]["headers"]["Authorization"],
        "Bearer mock-device-token"
    );
    assert_eq!(
        servers["daycare"]["disabledTools"],
        serde_json::json!(["daycare_memory_save"])
    );
    // The message came on stdin as one stream-json user event.
    let stdin: serde_json::Value = serde_json::from_str(h.read("stdin").trim()).unwrap();
    assert_eq!(stdin["event"], "user");
    assert!(stdin["message"]["content"]
        .as_str()
        .unwrap()
        .contains("Look around."));
    assert!(!h.read("calls").contains("Look around."));
    // And the token did not outlive the turn.
    assert_eq!(h.token_files_left(), 0);
}

#[test]
fn a_harmless_built_in_call_does_not_fail_a_good_turn() {
    let h = Harness::new("built-in");
    let outcome = h.run(TurnPurpose::World, Harness::new_session()).unwrap();
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    assert_eq!(h.token_files_left(), 0);
}

#[test]
fn a_shell_call_fails_the_seal() {
    let h = Harness::new("shell");
    let outcome = h.run(TurnPurpose::World, Harness::new_session()).unwrap();
    let failure = outcome.failure.unwrap();
    assert!(failure.contains("outside Daycare"), "{failure}");
    assert!(failure.contains("run_command"), "{failure}");
    assert_eq!(h.token_files_left(), 0);
}

#[test]
fn a_day_report_has_no_server_and_no_mcp_permission() {
    let h = Harness::new("day-report");
    let outcome = h
        .run(TurnPurpose::DayReport, Harness::new_session())
        .unwrap();
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    assert_eq!(h.read("saw-mcp"), "none");
    let settings: serde_json::Value = serde_json::from_str(&h.read("settings.json")).unwrap();
    assert!(settings["permissions"]["deny"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("mcp(*)")));
}

#[test]
fn a_resumed_turn_names_its_conversation() {
    let h = Harness::new("world");
    h.run(
        TurnPurpose::World,
        SessionMode::Resume {
            session_id: "c0ffee00-0000-4000-8000-000000000001".into(),
        },
    )
    .unwrap();
    assert!(h
        .read("calls")
        .contains(r#""--conversation", "c0ffee00-0000-4000-8000-000000000001""#));
}

#[test]
fn signed_out_old_or_forked_turns_never_start() {
    for (scenario, mode, expected) in [
        ("signed-out", Harness::new_session(), "not signed in"),
        ("old", Harness::new_session(), "1.5.13 or newer"),
        ("new-major", Harness::new_session(), "is not supported yet"),
        (
            "world",
            SessionMode::Fork {
                parent_session_id: "c0ffee00".into(),
            },
            "cannot fork",
        ),
    ] {
        let h = Harness::new(scenario);
        let error = h.run(TurnPurpose::World, mode).unwrap_err().to_string();
        assert!(error.contains(expected), "{scenario}: {error}");
        assert!(h.read("calls").is_empty(), "{scenario} started a turn");
        assert_eq!(h.token_files_left(), 0);
    }
}
