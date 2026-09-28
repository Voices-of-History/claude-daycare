//! Exercise the real orchestrator and OpenCode adapter with a local fake CLI.
mod support;

use daycare_runner::agent::opencode::OpencodeAgent;
use daycare_runner::launch::SessionMode;
use daycare_runner::paths::Layout;
use daycare_runner::turn::{run_turn, TurnOutcome, TurnPurpose, TurnRequest};
use daycare_runner::workspace::Workspace;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

struct Harness {
    dir: PathBuf,
    workspace: Workspace,
    agent: OpencodeAgent,
}

impl Harness {
    fn new(scenario: &str, fixture: &str) -> Self {
        let dir = support::scratch_dir("opencode-turn");
        let workspace = Workspace::new(dir.join("workspace"));
        workspace.scaffold("Pip", "http://127.0.0.1:1/mcp").unwrap();
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode-1.18.33");
        std::fs::copy(fixtures.join(fixture), dir.join("stream.jsonl")).unwrap();
        std::fs::write(dir.join("scenario"), scenario).unwrap();
        let bin = dir.join("fake-opencode");
        std::fs::write(&bin, r##"#!/usr/bin/env python3
import json, os, pathlib, sys, time
root = pathlib.Path(__file__).parent
scenario = (root / 'scenario').read_text()
args = sys.argv[1:]
config = json.loads(pathlib.Path(os.environ['OPENCODE_CONFIG']).read_text())
with (root / 'calls').open('a') as log:
    log.write(json.dumps(args) + '\n')
assert '--pure' in args
assert '--auto' not in args
if args[:2] == ['debug', 'config']:
    print(json.dumps(config))
elif args[:3] == ['debug', 'agent', 'daycare']:
    print(json.dumps({'name': 'daycare', 'prompt': config['agent']['daycare']['prompt'], 'tools': {'bash': False, 'read': False, 'task': False}}))
elif args[:2] == ['mcp', 'list']:
    assert os.environ['DAYCARE_DEVICE_TOKEN'] == 'mock-device-token'
    print('daycare ' + ('failed' if scenario == 'disconnected' else 'connected'))
elif args[0] == 'export':
    assert 'DAYCARE_DEVICE_TOKEN' not in os.environ
    print(json.dumps({'info': {'id': args[1]}, 'messages': [{'info': {'agent': 'build' if scenario == 'fallback' else 'daycare'}}]}))
elif args[0] == 'run':
    (root / 'prompt').write_text(sys.stdin.read())
    (root / 'run-config.json').write_text(json.dumps(config))
    if 'mcp' not in config:
        assert 'DAYCARE_DEVICE_TOKEN' not in os.environ
    stream = (root / 'stream.jsonl').read_text()
    if scenario == 'bash':
        stream = stream.replace('daycare_daycare_identity_get', 'bash')
    for line in stream.splitlines():
        print(line, flush=True)
        if scenario == 'overshoot' and json.loads(line)['type'] == 'step_finish':
            time.sleep(10)
    (root / 'finished').touch()
else:
    sys.exit(2)
"##).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let agent = OpencodeAgent::new(bin.to_str().unwrap(), &Layout::at(&dir));
        Self {
            dir,
            workspace,
            agent,
        }
    }

    fn run(&self, purpose: TurnPurpose, cap: Option<u64>) -> daycare_runner::Result<TurnOutcome> {
        run_turn(TurnRequest {
            agent: &self.agent,
            workspace: &self.workspace,
            mode: SessionMode::New {
                reserved_session_id: "unused".into(),
            },
            message: "Look around.",
            actor_name: "Pip",
            device_token: "mock-device-token",
            archive_path: &self.dir.join("turn.jsonl"),
            timeout: Duration::from_secs(20),
            purpose,
            model: "mock/m1",
            mcp_settle: Duration::ZERO,
            token_ceiling: cap,
        })
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn world_turn_uses_the_seal_and_normalized_receipt() {
    let h = Harness::new("world", "world.jsonl");
    let outcome = h.run(TurnPurpose::World, Some(300_000)).unwrap();
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    assert!(outcome.session_id().unwrap().starts_with("ses_"));
    assert!(outcome
        .receipt
        .unwrap()
        .tool_calls
        .iter()
        .all(|name| name.starts_with("mcp__daycare__")));
}

#[test]
fn token_overshoot_kills_before_the_next_step() {
    let h = Harness::new("overshoot", "world.jsonl");
    let outcome = h.run(TurnPurpose::World, Some(1)).unwrap();
    assert!(outcome.over_token_ceiling);
    assert!(outcome.failure.unwrap().contains("token cap"));
    assert!(outcome.elapsed_ms < 5_000);
    assert!(!h.dir.join("finished").exists());
}

#[test]
fn a_fast_exit_still_reports_token_overshoot() {
    let h = Harness::new("world", "world.jsonl");
    let outcome = h.run(TurnPurpose::World, Some(1)).unwrap();
    assert!(outcome.over_token_ceiling);
    assert!(outcome.failure.unwrap().contains("token cap"));
}

#[test]
fn build_fallback_fails_the_turn() {
    let h = Harness::new("fallback", "world.jsonl");
    let outcome = h.run(TurnPurpose::World, None).unwrap();
    assert!(outcome.failure.unwrap().contains("build"));
}

#[test]
fn disconnected_mcp_refuses_to_start_the_model() {
    let h = Harness::new("disconnected", "world.jsonl");
    assert!(h
        .run(TurnPurpose::World, None)
        .unwrap_err()
        .to_string()
        .contains("not connected"));
    assert!(!h.dir.join("prompt").exists());
}

#[test]
fn connected_preflight_does_not_validate_a_world_turn_with_no_tools() {
    // Preflight says connected, but the actual run has only text and a
    // successful step_finish, as when OpenCode drops MCP after preflight.
    let h = Harness::new("world", "world-resume.jsonl");
    let outcome = h.run(TurnPurpose::World, None).unwrap();
    let receipt = outcome.receipt.as_ref().unwrap();
    assert!(receipt.success);
    assert!(receipt.tool_calls.is_empty());
    assert!(h.dir.join("prompt").exists());
    assert!(
        !outcome.succeeded(),
        "unproven world access passed as completed"
    );
    assert!(!outcome.held);
    assert!(outcome.failure.unwrap().contains("no daycare tool"));
}

#[test]
fn a_bash_call_fails_the_seal() {
    let h = Harness::new("bash", "world.jsonl");
    let outcome = h.run(TurnPurpose::World, None).unwrap();
    assert!(outcome.failure.unwrap().contains("called bash"));
}

#[test]
fn day_report_has_no_mcp_or_device_token() {
    let h = Harness::new("report", "day-report.jsonl");
    let outcome = h.run(TurnPurpose::DayReport, None).unwrap();
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.dir.join("run-config.json")).unwrap())
            .unwrap();
    assert!(config.get("mcp").is_none());
    assert!(!std::fs::read_to_string(h.dir.join("calls"))
        .unwrap()
        .contains("\"mcp\""));
}
