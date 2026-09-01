//! The shipped binary, end to end, against a mock platform and a fake `claude`.
//! No network, no keychain, no model.

mod support;

use daycare_runner::paths::shell_quote;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::{MockPlatform, RecordedRequest, Response};

const BIN: &str = env!("CARGO_BIN_EXE_daycare-runner");
const TOKEN: &str = "dev_token_do_not_leak_9876";
const SESSION: &str = "18f44c2e-ff64-4e94-a89d-bdbeaa9ab9f7";

struct Install {
    home: PathBuf,
    token_file: PathBuf,
    claude_bin: PathBuf,
    /// A PATH containing a `claude` that refuses to run, and nothing else of
    /// ours. See `install()` — this is the guard, not a convenience.
    path: String,
    /// A HOME inside the scratch dir, so a child that writes session state
    /// cannot reach the real `~/.claude`.
    fake_home: PathBuf,
    /// Written by the refusing shim on PATH if anything reached for the real
    /// `claude`. Checked in Drop.
    claude_marker: PathBuf,
}

impl Drop for Install {
    /// Fail any test that reached for the real `claude`, even one that would
    /// otherwise have passed. The PATH shim already makes the mistake
    /// harmless; this is what makes it visible, which is the half that was
    /// missing when this cost a day of real model calls.
    fn drop(&mut self) {
        if std::thread::panicking() {
            return; // Already failing; don't mask the real assertion.
        }
        if let Ok(reached) = std::fs::read_to_string(&self.claude_marker) {
            panic!(
                "this test launched the real `claude` instead of a fake.\n\
                 argv seen by the shim: {}\n\
                 Pass --claude-bin to every command that can start a turn — \
                 including `visit start`, which detaches a poller.",
                reached.trim()
            );
        }
    }
}

impl Install {
    fn workspace(&self) -> PathBuf {
        self.home.join("workspaces/actor-1")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("DAYCARE_HOME", &self.home)
            .env("DAYCARE_TOKEN_FILE", &self.token_file)
            .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
            .env("PATH", &self.path)
            .env("HOME", &self.fake_home)
            .output()
            .expect("run daycare-runner")
    }

    fn spawn(&self, args: &[&str]) -> Child {
        Command::new(BIN)
            .args(args)
            .env("DAYCARE_HOME", &self.home)
            .env("DAYCARE_TOKEN_FILE", &self.token_file)
            .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
            .env("PATH", &self.path)
            .env("HOME", &self.fake_home)
            .spawn()
            .expect("spawn daycare-runner")
    }

    fn run_shell(&self, command: &str) -> Output {
        Command::new("/bin/sh")
            .args(["-c", command])
            .env("DAYCARE_HOME", &self.home)
            .env("DAYCARE_TOKEN_FILE", &self.token_file)
            .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
            .env("PATH", &self.path)
            .env("HOME", &self.fake_home)
            .output()
            .expect("run advertised daycare-runner command through the shell")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for test barrier");
}

#[test]
fn skill_show_json_is_one_object_and_plain_output_is_byte_identical() {
    let install = install("cli-skill-show-json");
    let plain = install.run(&["skill", "show"]);
    assert!(plain.status.success());

    let json_output = install.run(&["skill", "show", "--json"]);
    assert!(json_output.status.success());
    let stdout = stdout(&json_output);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(
        parsed["markdown"].as_str().unwrap().as_bytes(),
        plain.stdout
    );

    let mut values =
        serde_json::Deserializer::from_str(stdout.trim()).into_iter::<serde_json::Value>();
    assert!(values.next().unwrap().is_ok());
    assert!(
        values.next().is_none(),
        "skill show emitted more than one JSON value"
    );
}

fn seed_awaiting_homecoming(install: &Install, visit_id: &str) {
    seed_awaiting_homecoming_with_reason(install, visit_id, "activity_ended");
}

fn seed_awaiting_homecoming_with_reason(install: &Install, visit_id: &str, local_end_reason: &str) {
    std::fs::write(
        install.home.join("sessions.json"),
        format!(r#"{{"actor-1":"{SESSION}"}}"#),
    )
    .unwrap();
    // The one turn the ledger counts left a real archive; the homecoming
    // reader refuses to write a visit it cannot read back.
    std::fs::create_dir_all(install.home.join("turns")).unwrap();
    std::fs::write(
        install
            .home
            .join(format!("turns/{visit_id}-seed-turn.jsonl")),
        support::fixture_stream_from(&install.workspace()),
    )
    .unwrap();
    std::fs::create_dir_all(install.home.join("visits")).unwrap();
    std::fs::write(
        install.home.join(format!("visits/{visit_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "visit_id": visit_id,
            "identity_id": "actor-1",
            "identity_name": "Pip",
            "status": "ended",
            "started_at": "2026-08-09T20:00:00Z",
            "ended_at": "2026-08-09T20:05:00Z",
            "end_reason": local_end_reason,
            "local_end_reason": local_end_reason,
            "budget": {},
            "ledger": {
                "turns_used": 1,
                "turns_failed": 0,
                "consecutive_failures": 0,
                "tokens_used": 1,
                "cost_usd": 0.0,
                "elapsed_secs": 300,
                "rate_limited": false,
                "usage_incomplete": false
            },
            "turn_archives": [format!("{visit_id}-seed-turn")],
            "homecoming_state": "awaiting_outcome"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn install(label: &str) -> Install {
    let root = support::scratch_dir(label);
    let home = root.join("home");
    let workspace = home.join("workspaces/actor-1");
    // The fake claude must report the workspace it will actually run in.
    let claude_bin = support::fake_claude(&root, &support::fixture_stream_from(&workspace), 0, 0);
    Install {
        home,
        token_file: root.join("tokens.json"),
        claude_bin,
        // Every command a test runs gets a PATH whose only `claude` refuses to
        // run, and a HOME inside the scratch dir.
        //
        // This exists because on 2026-08-06 a `visit start` in this file was
        // written without `--claude-bin`. `visit start` detaches a poller, the
        // poller defaulted to whatever `claude` was on PATH, and that was the
        // real one: ten runs of the suite made real model calls on the user's
        // subscription — about 57k output tokens — and left ten session
        // transcripts in the real `~/.claude/projects`. Nothing failed. The
        // test passed every time, because it SIGKILLs the poller and never
        // looks at what the poller did first.
        //
        // Passing `--claude-bin` fixes that one line; this fixes the class. A
        // future test that forgets the flag now dies with a loud message
        // instead of quietly spending money, and a child that writes session
        // state cannot touch the user's real home to do it.
        path: support::no_claude_path(&root),
        fake_home: root.join("fake-home"),
        claude_marker: support::claude_marker(&root),
    }
}

fn queue_once(command_json: &'static str) -> (MockPlatform, Arc<AtomicUsize>) {
    let served = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&served);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.path.ends_with("/commands/next") {
            // One turn, then an empty queue.
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return Response::json(200, command_json);
            }
            return Response::no_content();
        }
        if request.path.ends_with("/visits/visit-1/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-1","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.method == "GET" && request.path.ends_with("/visits/visit-1") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-1","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":1,\"offset\":0,\"memories\":[{\"id\":\"memory-1\",\"text\":\"I found the chalk.\",\"created_at\":\"2026-08-07T06:00:00Z\"}]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    (platform, served)
}

/// The local half of "identities outlive devices".
///
/// PRODUCT.md's promise is that a Claude is a continuing character, not a
/// property of the laptop it was paired from. Delivering it needs two halves:
/// the server re-pointing an existing identity at a new device, and the local
/// layout surviving that. This test covers the second.
///
/// **What this test cannot see, stated because it is the whole risk:** it
/// serves the same `actor_id` from a mock. The server half shipped in 73bb3b998
/// (`repointIdentity` re-points `device_id` and rotates the identity's hash),
/// so the scenario is now reachable in production — but nothing here exercises
/// it. A green result is evidence about the local layout only. The live
/// re-pair leg of the slice-2 E2E is what tests the promise.
///
/// What would break the local half: `enroll` clearing the workspace,
/// `Identities::load` being skipped so the map is rewritten from scratch, or
/// `sessions.json` being keyed by device. Any of those silently costs the
/// character its memory, and the failure looks like a Claude that simply forgot.
#[test]
fn re_pointing_a_device_keeps_the_identity_its_session_and_its_workspace() {
    let install = install("cli-repair");
    let device = Arc::new(AtomicUsize::new(1));
    let handing_out = Arc::clone(&device);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            // Same identity, new machine: actor-1 re-pointed from device-1 to
            // device-2, which is what the platform's nullable device_id is for.
            let n = handing_out.fetch_add(1, Ordering::SeqCst);
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-{n}",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    // Stand in for the lineage a turn leaves behind: the resume pointer, and a
    // file inside the workspace that only the character's own history explains.
    let sessions = install.home.join("sessions.json");
    std::fs::write(
        &sessions,
        r#"{"actor-1":"7c272da0-0000-4000-8000-000000000001"}"#,
    )
    .unwrap();
    let workspace = install.home.join("workspaces/actor-1");
    let keepsake = workspace.join("remembered.txt");
    std::fs::write(&keepsake, "Patch was here first").unwrap();

    let again = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-5678"]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    // The device moved.
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(install.home.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(
        config["device_id"], "device-2",
        "the re-pairing did not take"
    );
    assert_eq!(config["actor_id"], "actor-1");

    // The character did not.
    let carried: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sessions).unwrap()).unwrap();
    assert_eq!(
        carried["actor-1"], "7c272da0-0000-4000-8000-000000000001",
        "re-pairing lost the session lineage; the Claude would resume as a stranger"
    );
    assert_eq!(
        std::fs::read_to_string(&keepsake).unwrap(),
        "Patch was here first",
        "re-pairing wiped the workspace"
    );
    // Still exactly one Pip — a second enrollment must not fork the identity.
    let listed: serde_json::Value =
        serde_json::from_str(stdout(&install.run(&["identity", "list", "--json"])).trim()).unwrap();
    assert_eq!(
        listed["identities"].as_array().unwrap().len(),
        1,
        "{listed}"
    );
    assert_eq!(listed["identities"][0]["name"], "Pip");
}

/// `status` describes the enrollment record unless the user names a Claude.
///
/// A second successful enrollment updates `config.actor_id` but deliberately
/// keeps the first identity and its local history. Both are General profiles,
/// so the ordinary bare-command resolver still selects the earliest one. That
/// is correct for `run`, `open`, and explicit `--general`; it was misleading
/// for bare `status`, which then printed the old enrollment as the current one.
#[test]
fn bare_status_reports_the_latest_enrollment_and_explicit_general_stays_stable() {
    let install = install("cli-status-latest-enrollment");
    let claim = Arc::new(AtomicUsize::new(1));
    let handing_out = Arc::clone(&claim);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            let n = handing_out.fetch_add(1, Ordering::SeqCst);
            let name = if n == 1 { "Old Claude" } else { "New Claude" };
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-{n}",
                        "actor_id":"actor-{n}","actor_name":"{name}",
                        "actor_kind":"general","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    let first = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-OLD"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    // Ensure the two General profiles have an unambiguous age order even when
    // both enrollments happen inside the same wall-clock second.
    let identities_file = install.home.join("identities.json");
    let mut identities: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&identities_file).unwrap()).unwrap();
    identities["actor-1"]["created_at"] = serde_json::json!("2026-08-28T00:00:00Z");
    std::fs::write(
        &identities_file,
        serde_json::to_vec_pretty(&identities).unwrap(),
    )
    .unwrap();

    let second = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-NEW"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let bare = install.run(&["status", "--json"]);
    assert!(
        bare.status.success(),
        "{}",
        String::from_utf8_lossy(&bare.stderr)
    );
    let bare: serde_json::Value = serde_json::from_str(stdout(&bare).trim()).unwrap();
    assert_eq!(bare["identity"]["identity_id"], "actor-2", "{bare}");
    assert_eq!(bare["identity"]["name"], "New Claude", "{bare}");

    // The fix is status-specific. Explicit selection keeps the existing
    // universal-Claude rule and still chooses the earliest General profile.
    let general = install.run(&["status", "--general", "--json"]);
    assert!(
        general.status.success(),
        "{}",
        String::from_utf8_lossy(&general.stderr)
    );
    let general: serde_json::Value = serde_json::from_str(stdout(&general).trim()).unwrap();
    assert_eq!(general["identity"]["identity_id"], "actor-1", "{general}");
    assert_eq!(general["identity"]["name"], "Old Claude", "{general}");
}

/// A re-point must not demote a project-bound Claude to the machine's general one.
///
/// The server keeps the binding across a move — `repointIdentity` writes only
/// `device_id` and `token_hash`, so `kind` and `workspace_label` survive — and
/// `enroll` used to overwrite the local record with `General`/`None` regardless.
/// `identity list` then described a workspace Claude as general, on exactly the
/// machine a user looks at after replacing a laptop.
///
/// The local record is authoritative for the path because the server stores a
/// hash and a label, never a filesystem path.
#[test]
fn re_pointing_does_not_downgrade_a_workspace_identity_to_general() {
    let install = install("cli-repoint-kind");
    let device = Arc::new(AtomicUsize::new(1));
    let handing_out = Arc::clone(&device);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            let n = handing_out.fetch_add(1, Ordering::SeqCst);
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-{n}",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp",
                        "repointed":true}}"#
                ),
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    // This machine knows the identity is bound to a project — the state a real
    // install reaches through `identity create --bind`.
    let identities_file = install.home.join("identities.json");
    std::fs::write(
        &identities_file,
        r#"{"actor-1":{"identity_id":"actor-1","name":"Pip","kind":"workspace",
            "bound_workspace":"/projects/orchard","mcp_url":"http://x/api/daycare/mcp",
            "created_at":"2026-08-06T00:00:00Z"}}"#,
    )
    .unwrap();

    let again = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-5678"]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    let listed: serde_json::Value =
        serde_json::from_str(stdout(&install.run(&["identity", "list", "--json"])).trim()).unwrap();
    assert_eq!(
        listed["identities"].as_array().unwrap().len(),
        1,
        "{listed}"
    );
    assert_eq!(
        listed["identities"][0]["kind"], "workspace",
        "re-pairing described a project-bound Claude as the machine's general one: {listed}"
    );
    assert_eq!(
        listed["identities"][0]["bound_workspace"], "/projects/orchard",
        "re-pairing dropped the local project binding: {listed}"
    );
}

/// A new laptop has no absolute path to preserve. The claim must still keep
/// the server-owned workspace identity type and human label, while stating
/// that the project is not bound on this machine.
#[test]
fn fresh_machine_repair_keeps_workspace_identity_unbound_and_resolvable() {
    let install = install("cli-repoint-fresh-workspace");
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-2",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp",
                        "repointed":true,"actor_kind":"workspace",
                        "workspace_label":"voices-of-history"}}"#
                ),
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    let enrolled = install.run(&[
        "enroll",
        "--url",
        &platform.base_url,
        "--code",
        "PAIR-1234",
        "--json",
    ]);
    assert!(
        enrolled.status.success(),
        "{}",
        String::from_utf8_lossy(&enrolled.stderr)
    );
    let enrolled: serde_json::Value = serde_json::from_str(stdout(&enrolled).trim()).unwrap();
    assert_eq!(enrolled["identity_kind"], "workspace");
    assert_eq!(enrolled["workspace_label"], "voices-of-history");
    assert_eq!(enrolled["binding_state"], "unbound_on_this_machine");

    let listed: serde_json::Value =
        serde_json::from_str(stdout(&install.run(&["identity", "list", "--json"])).trim()).unwrap();
    let identity = &listed["identities"][0];
    assert_eq!(identity["kind"], "workspace");
    assert_eq!(identity["workspace_label"], "voices-of-history");
    assert_eq!(identity["bound_workspace"], serde_json::Value::Null);
    assert_eq!(identity["binding_state"], "unbound_on_this_machine");

    // Explicit selection proves that "unbound" is a truthful display state,
    // not an unusable identity or an invitation to recreate it as General.
    let status = install.run(&["status", "--identity", "Pip", "--json"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: serde_json::Value = serde_json::from_str(stdout(&status).trim()).unwrap();
    assert_eq!(status["identity"]["identity_id"], "actor-1");
    assert_eq!(status["identity"]["kind"], "workspace");
    assert_eq!(
        status["identity"]["binding_state"],
        "unbound_on_this_machine"
    );
}

fn assert_current_server_unlabeled_claim(test_name: &str, repointed: bool, actor_kind: &str) {
    let install = install(test_name);
    let expected_kind = actor_kind.to_string();
    let response_kind = expected_kind.clone();
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-2",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp",
                        "repointed":{repointed},"actor_kind":"{response_kind}",
                        "workspace_label":null}}"#
                ),
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    let enrolled = install.run(&[
        "enroll",
        "--url",
        &platform.base_url,
        "--code",
        "PAIR-1234",
        "--json",
    ]);
    assert!(
        enrolled.status.success(),
        "{}",
        String::from_utf8_lossy(&enrolled.stderr)
    );
    let enrolled: serde_json::Value = serde_json::from_str(stdout(&enrolled).trim()).unwrap();
    assert_eq!(enrolled["repointed"], repointed);
    let expected_binding = if expected_kind == "workspace" {
        "unbound_on_this_machine"
    } else {
        "not_applicable"
    };
    assert_eq!(enrolled["identity_kind"], expected_kind);
    assert_eq!(enrolled["workspace_label"], serde_json::Value::Null);
    assert_eq!(enrolled["binding_state"], expected_binding);

    let listed: serde_json::Value =
        serde_json::from_str(stdout(&install.run(&["identity", "list", "--json"])).trim()).unwrap();
    let identity = &listed["identities"][0];
    assert_eq!(identity["kind"], expected_kind);
    assert_eq!(identity["workspace_label"], serde_json::Value::Null);
    assert_eq!(identity["bound_workspace"], serde_json::Value::Null);
    assert_eq!(identity["binding_state"], expected_binding);
}

#[test]
fn current_server_first_pair_is_the_unlabeled_general_identity() {
    assert_current_server_unlabeled_claim("cli-current-first-general", false, "general");
}

#[test]
fn current_server_fresh_repair_accepts_unlabeled_workspace_identity() {
    assert_current_server_unlabeled_claim(
        "cli-current-repair-unlabeled-workspace",
        true,
        "workspace",
    );
}

#[test]
fn fresh_enroll_can_run_the_bare_open_command_it_advertises() {
    let install = install("cli-fresh-enroll-advertised-open");
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp",
                        "repointed":false,"actor_kind":"general","workspace_label":null}}"#
                ),
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    let enrolled = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    assert!(
        enrolled.status.success(),
        "{}",
        String::from_utf8_lossy(&enrolled.stderr)
    );
    assert!(
        stdout(&enrolled).contains("daycare-runner open"),
        "enroll did not advertise open: {}",
        stdout(&enrolled)
    );
    assert!(
        !stdout(&enrolled).contains("--identity"),
        "General enrollment must remain bare: {}",
        stdout(&enrolled)
    );

    let opened = install.run(&["open"]);
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    let output = stdout(&opened);
    assert!(output.contains("cd "), "{output}");
    assert!(output.contains("no daycare session yet"), "{output}");
}

#[test]
fn workspace_repair_advertises_and_runs_the_exact_claimed_actor_despite_a_same_name() {
    let install = install("cli-workspace-repair-exact-advertised-open");
    let injection_marker = install.home.join("identity-name-executed");
    let actor_name = format!("-Quill's $(touch {})", injection_marker.display());
    let claimed_name = actor_name.clone();
    let claim_number = AtomicUsize::new(0);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            let index = claim_number.fetch_add(1, Ordering::SeqCst);
            let claim_body = serde_json::json!({
                "device_token": TOKEN,
                "device_id": format!("device-{index}"),
                "actor_id": if index == 0 { "actor-a" } else { "actor-b" },
                "actor_name": claimed_name,
                "mcp_path": "/api/daycare/mcp",
                "repointed": true,
                "actor_kind": "workspace",
                "workspace_label": null,
            })
            .to_string();
            return Response::json(200, &claim_body);
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    let first = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let enrolled = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-5678"]);
    assert!(
        enrolled.status.success(),
        "{}",
        String::from_utf8_lossy(&enrolled.stderr)
    );
    let output = stdout(&enrolled);
    for command in ["visit start --weekly-percent 2", "run", "open"] {
        assert!(
            output.contains(&format!("daycare-runner {command} --identity-id='actor-b'")),
            "workspace enrollment did not advertise the exact claimed actor: {output}"
        );
    }
    assert!(
        !output
            .lines()
            .any(|line| line.contains(&actor_name) && line.contains("daycare-runner")),
        "the hostile display name appeared in an executable command: {output}"
    );

    let advertised_open = output
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("daycare-runner open --identity-id="))
        .expect("workspace enrollment advertises open");
    let executable = advertised_open.replacen("daycare-runner", &shell_quote(BIN), 1);
    let opened = install.run_shell(&executable);
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    assert!(
        stdout(&opened).contains("workspaces/actor-b"),
        "advertised command selected the wrong same-name actor: {}",
        stdout(&opened),
    );
    assert!(
        !injection_marker.exists(),
        "the shell executed syntax embedded in the identity name"
    );
}

#[test]
fn general_repair_advertises_and_runs_the_exact_claimed_general() {
    let install = install("cli-general-repair-exact-advertised-open");
    let claim_number = AtomicUsize::new(0);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            let index = claim_number.fetch_add(1, Ordering::SeqCst);
            return Response::json(
                200,
                &serde_json::json!({
                    "device_token": TOKEN,
                    "device_id": format!("device-{index}"),
                    "actor_id": if index == 0 { "general-a" } else { "general-b" },
                    "actor_name": if index == 0 { "Pip" } else { "Scout" },
                    "mcp_path": "/api/daycare/mcp",
                    "repointed": index == 1,
                    "actor_kind": "general",
                    "workspace_label": null,
                })
                .to_string(),
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    let first = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let enrolled = install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-5678"]);
    assert!(
        enrolled.status.success(),
        "{}",
        String::from_utf8_lossy(&enrolled.stderr)
    );
    let output = stdout(&enrolled);
    for command in ["visit start --weekly-percent 2", "run", "open"] {
        assert!(
            output.contains(&format!(
                "daycare-runner {command} --identity-id='general-b'"
            )),
            "General re-pair did not advertise the exact claimed actor: {output}"
        );
    }

    let advertised_open = output
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("daycare-runner open --identity-id="))
        .expect("General re-pair advertises exact open");
    let executable = advertised_open.replacen("daycare-runner", &shell_quote(BIN), 1);
    let opened = install.run_shell(&executable);
    assert!(
        opened.status.success(),
        "{}",
        String::from_utf8_lossy(&opened.stderr)
    );
    assert!(
        stdout(&opened).contains("workspaces/general-b"),
        "advertised command selected the old General actor: {}",
        stdout(&opened),
    );
}

/// Enrolling never seeds the resume pointer from anything the server sent.
///
/// A Claude Code session lives in `~/.claude/projects/<escaped-workspace-path>`,
/// not in the daycare home and not on the server, so a session id is meaningless
/// on a machine that did not create it. The platform stores
/// `daycare_actors.claude_session_id` and it is the natural thing to helpfully
/// include in a re-pair claim; writing it into `sessions.json` would make every
/// turn on the new machine fail `--resume` against a transcript that is not
/// there — and fail confusingly, naming a session the user can see in the DB.
///
/// The right behaviour on a fresh machine is a new session, with continuity
/// coming from server-side memories through the MCP. This test holds that line
/// no matter what the claim response grows.
#[test]
fn enrolling_never_seeds_a_session_from_the_claim_response() {
    let install = install("cli-noseed");
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            // A server doing the tempting, helpful thing.
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip",
                        "mcp_path":"/api/daycare/mcp","repointed":true,
                        "claude_session_id":"7c272da0-0000-4000-8000-000000000001"}}"#
                ),
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });

    let out = install.run(&[
        "enroll",
        "--url",
        &platform.base_url,
        "--code",
        "PAIR-1234",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let enrolled: serde_json::Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(
        enrolled["repointed"], true,
        "the move was not reported: {enrolled}"
    );

    let sessions = install.home.join("sessions.json");
    let seeded = std::fs::read_to_string(&sessions).unwrap_or_default();
    assert!(
        !seeded.contains("7c272da0"),
        "enroll seeded a resume pointer from the server; every turn here would \
         fail --resume against a transcript this machine never had: {seeded}"
    );
}

/// The identity acts with its own credential when the server mints one, and
/// with the device's only when it doesn't.
///
/// Both halves matter and they fail in opposite directions. If a minted
/// identity token were ignored, the install would keep acting as the device and
/// the separation the server is building would be a fiction on this side. If
/// the absent case stopped falling back, a companion updated ahead of the
/// server would pair successfully and then have no credential able to take a
/// turn — a break that looks like a server fault and isn't.
///
/// What this cannot see: whether the platform actually accepts the token it
/// minted. These are mock responses, so this pins which credential lands where,
/// not that either one authenticates. The live re-pair leg is what proves that.
#[test]
fn a_minted_identity_token_becomes_the_acting_credential_and_the_device_token_stays_the_devices() {
    fn claim(extra: &str) -> impl Fn(&RecordedRequest) -> Response {
        let extra = extra.to_string();
        move |request: &RecordedRequest| {
            if request.path.ends_with("/pair/claim") {
                return Response::json(
                    200,
                    &format!(
                        r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                            "actor_id":"actor-1","actor_name":"Pip",
                            "mcp_path":"/api/daycare/mcp"{extra}}}"#
                    ),
                );
            }
            Response::json(200, r#"{"ok":true}"#)
        }
    }

    // --- the server mints the identity a token of its own ----------------
    let install = install("cli-identity-token");
    let platform = MockPlatform::start(claim(
        r#","repointed":true,"identity_token":"dck_identity_only""#,
    ));
    let out = install.run(&[
        "enroll",
        "--url",
        &platform.base_url,
        "--code",
        "PAIR-1234",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stored = std::fs::read_to_string(&install.token_file).unwrap();
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(
        stored["identity:actor-1"], "dck_identity_only",
        "the identity kept acting with the device token even though the server \
         minted it one of its own: {stored}"
    );
    assert_eq!(
        stored["device:device-1"], TOKEN,
        "the device account should still hold the device token: {stored}"
    );
    assert_ne!(
        stored["identity:actor-1"], stored["device:device-1"],
        "the two credentials are stored as one; the identity is still the device: {stored}"
    );
    assert!(
        !stdout(&out).contains("dck_identity_only"),
        "enroll printed the identity token"
    );

    // --- the server has not shipped that yet -----------------------------
    let legacy = crate::install("cli-identity-token-legacy");
    let platform = MockPlatform::start(claim(r#","repointed":false,"identity_token":null"#));
    let out = legacy.run(&[
        "enroll",
        "--url",
        &platform.base_url,
        "--code",
        "PAIR-1234",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stored = std::fs::read_to_string(&legacy.token_file).unwrap();
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(
        stored["identity:actor-1"], TOKEN,
        "with no minted token the identity has to go on acting with the \
         device's; this install cannot take a turn: {stored}"
    );

    // --- the server sends the field, but empty --------------------------
    // Not the same as absent. Sending the field says the server has the
    // re-point path, which means the identity most likely has a hash against
    // it, which means the device token cannot act for it. Falling back here
    // would pair cleanly and then fail every turn on an authentication error
    // pointing nowhere near the cause, so this has to stop at enroll.
    let empty = crate::install("cli-identity-token-empty");
    let platform = MockPlatform::start(claim(r#","repointed":true,"identity_token":"   ""#));
    let out = empty.run(&[
        "enroll",
        "--url",
        &platform.base_url,
        "--code",
        "PAIR-1234",
        "--json",
    ]);
    assert!(
        !out.status.success(),
        "enroll accepted an empty identity token and reported a working \
         install: {}",
        stdout(&out)
    );
    let stored = std::fs::read_to_string(&empty.token_file).unwrap_or_default();
    assert!(
        !stored.contains("identity:actor-1"),
        "an unusable credential was stored anyway: {stored}"
    );
}

#[test]
fn enroll_then_turn_then_open_is_one_working_install() {
    let install = install("cli-e2e");
    let (platform, _) = queue_once(r#"{"id":"cmd-1","kind":"world_turn","actor_id":"actor-1"}"#);

    // --- enroll ---------------------------------------------------------
    let enrolled = install.run(&[
        "enroll",
        "--url",
        &platform.base_url,
        "--code",
        "PAIR-1234",
        "--device-name",
        "test-mac",
    ]);
    assert!(
        enrolled.status.success(),
        "{}",
        String::from_utf8_lossy(&enrolled.stderr)
    );
    let output = stdout(&enrolled);
    assert!(output.contains("Paired with"));
    assert!(output.contains("Pip"));
    assert!(!output.contains(TOKEN), "enroll printed the token");

    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(install.home.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(config["actor_name"], "Pip");
    assert_eq!(
        config["mcp_url"],
        format!("{}/api/daycare/mcp", platform.base_url)
    );
    assert!(
        !std::fs::read_to_string(install.home.join("config.json"))
            .unwrap()
            .contains(TOKEN),
        "token was written to config.json"
    );

    let workspace = install.workspace();
    for file in ["CLAUDE.md", "controller-prompt.md", "daycare-mcp.json"] {
        assert!(workspace.join(file).is_file(), "{file} was not scaffolded");
    }
    assert!(std::fs::read_to_string(workspace.join("CLAUDE.md"))
        .unwrap()
        .contains("Pip"));

    // --- run-once -------------------------------------------------------
    let turn = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(
        turn.status.success(),
        "{}",
        String::from_utf8_lossy(&turn.stderr)
    );
    let output = stdout(&turn);
    assert!(output.contains("turn cmd-1"), "{output}");
    assert!(output.contains("completed"), "{output}");

    // The turn was archived and the session recorded for the next turn.
    assert!(install.home.join("turns/cmd-1.jsonl").is_file());
    let sessions: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(install.home.join("sessions.json")).unwrap())
            .unwrap();
    let argv = support::recorded_argv(&install.claude_bin.parent().unwrap().to_path_buf());
    let assigned_session = argv
        .windows(2)
        .find(|pair| pair[0] == "--session-id")
        .map(|pair| pair[1].as_str())
        .expect("first turn did not reserve a session id");
    assert_eq!(sessions["actor-1"], assigned_session);

    // The platform got a completion receipt with the session and the usage the
    // stream actually carried.
    let completion = platform
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/complete"))
        .expect("no completion posted");
    assert_eq!(completion.path, "/api/daycare/commands/cmd-1/complete");
    assert_eq!(
        completion.authorization(),
        Some(&format!("Bearer {TOKEN}")[..])
    );
    let body = completion.json();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["claude_session_id"], assigned_session);
    assert_eq!(body["result"]["usage"]["output_tokens"], 521);
    assert!(body["result"]["result_text"]
        .as_str()
        .unwrap()
        .contains("Courtyard"));

    // --- the second turn resumes the same Claude ------------------------
    assert!(
        argv.windows(2).any(|pair| pair[0] == "--session-id"),
        "first turn should reserve a new session id: {argv:?}"
    );

    let again = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(again.status.success());
    assert!(stdout(&again).contains("no work"));

    // --- open / status --------------------------------------------------
    let open = install.run(&["open"]);
    let attach = stdout(&open);
    assert!(
        attach.contains(&format!("claude --resume '{assigned_session}'")),
        "{attach}"
    );
    assert!(attach.contains(workspace.to_str().unwrap()));

    let status = install.run(&["status"]);
    let status_text = stdout(&status);
    assert!(
        status_text.contains("character:   Pip (actor-1)"),
        "{status_text}"
    );
    assert!(status_text.contains("credentials:"), "{status_text}");
    assert!(status_text.contains(assigned_session), "{status_text}");
    assert!(!status_text.contains(TOKEN), "status printed the token");
}

#[test]
fn a_second_turn_resumes_the_stored_session() {
    let install = install("cli-resume");
    let (platform, _) = queue_once(r#"{"id":"cmd-7","kind":"world_turn"}"#);

    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    // Pretend a previous turn already established the session.
    std::fs::write(
        install.home.join("sessions.json"),
        format!(r#"{{"actor-1":"{SESSION}"}}"#),
    )
    .unwrap();

    let turn = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(
        turn.status.success(),
        "{}",
        String::from_utf8_lossy(&turn.stderr)
    );

    let argv = support::recorded_argv(&install.claude_bin.parent().unwrap().to_path_buf());
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == SESSION),
        "second turn did not resume the same Claude: {argv:?}"
    );
    assert!(!argv.iter().any(|arg| arg == "--session-id"));
}

#[test]
fn a_stale_resumed_session_recovers_before_input_with_a_fresh_session() {
    let root = support::scratch_dir("cli-stale-resume");
    let home = root.join("home");
    let workspace = home.join("workspaces/actor-1");
    let claude_bin =
        support::fake_claude_stale_resume(&root, &support::fixture_stream_from(&workspace));
    let install = Install {
        home,
        token_file: root.join("tokens.json"),
        claude_bin,
        path: support::no_claude_path(&root),
        fake_home: root.join("fake-home"),
        claude_marker: support::claude_marker(&root),
    };
    let (platform, _) = queue_once(r#"{"id":"cmd-stale","kind":"world_turn"}"#);

    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    std::fs::write(
        install.home.join("sessions.json"),
        format!(r#"{{"actor-1":"{SESSION}"}}"#),
    )
    .unwrap();

    let turn = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(
        turn.status.success(),
        "{}",
        String::from_utf8_lossy(&turn.stderr)
    );

    let calls = std::fs::read_to_string(root.join("call.argv.all")).unwrap();
    assert!(calls.contains(&format!("--resume\n{SESSION}")), "{calls}");
    assert!(calls.contains("--session-id\n"), "{calls}");
    assert_eq!(calls.matches("-- call --").count(), 2, "{calls}");

    let sessions: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(install.home.join("sessions.json")).unwrap())
            .unwrap();
    assert_ne!(sessions["actor-1"], SESSION);
    assert!(
        std::fs::metadata(install.home.join("turns/cmd-stale.jsonl"))
            .unwrap()
            .len()
            > 0
    );

    let completion = platform
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/complete"))
        .expect("recovered turn was not reported");
    assert_eq!(completion.json()["status"], "completed");
    assert_eq!(completion.json()["claude_session_id"], sessions["actor-1"]);
}

#[test]
fn an_active_visit_refuses_to_replace_a_stale_session_with_a_blank_mind() {
    let root = support::scratch_dir("cli-stale-active-visit");
    let home = root.join("home");
    let workspace = home.join("workspaces/actor-1");
    let claude_bin =
        support::fake_claude_stale_resume(&root, &support::fixture_stream_from(&workspace));
    let install = Install {
        home,
        token_file: root.join("tokens.json"),
        claude_bin,
        path: support::no_claude_path(&root),
        fake_home: root.join("fake-home"),
        claude_marker: support::claude_marker(&root),
    };
    let served = Arc::new(AtomicUsize::new(0));
    let command_count = Arc::clone(&served);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.path.ends_with("/commands/next") {
            return match command_count.fetch_add(1, Ordering::SeqCst) {
                0 => Response::json(
                    200,
                    r#"{"id":"cmd-stale-active","kind":"world_turn","visit_id":"visit-stale-active","payload":{"reason":"visit_continue"}}"#,
                ),
                _ => Response::no_content(),
            };
        }
        if request.path.ends_with("/visits/visit-stale-active/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-stale-active","end_reason":"budget_exhausted","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    std::fs::write(
        install.home.join("sessions.json"),
        format!(r#"{{"actor-1":"{SESSION}"}}"#),
    )
    .unwrap();
    std::fs::create_dir_all(install.home.join("visits")).unwrap();
    std::fs::write(
        install.home.join("visits/visit-stale-active.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "visit_id": "visit-stale-active",
            "identity_id": "actor-1",
            "identity_name": "Pip",
            "status": "active",
            "started_at": "2099-08-29T12:00:00Z",
            "instructions": "Keep building the same idea",
            "budget": { "turns": 2 },
            "ledger": {
                "turns_used": 1,
                "turns_failed": 0,
                "consecutive_failures": 0,
                "tokens_used": 1,
                "cost_usd": 0.0,
                "elapsed_secs": 0,
                "rate_limited": false,
                "usage_incomplete": false
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let ran = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-stale-active",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(!ran.status.success(), "{}", stdout(&ran));
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(
        stderr.contains("refusing to replace it with a blank session"),
        "{stderr}"
    );

    let calls = std::fs::read_to_string(root.join("call.argv.all")).unwrap();
    assert!(calls.contains(&format!("--resume\n{SESSION}")), "{calls}");
    // Only in-visit turns (the ones that deny the save tool) are held to the
    // visit's session. The homecoming reader that follows the visit's end is
    // a fresh session by design.
    let in_visit_calls: Vec<&str> = calls
        .split("-- call --")
        .filter(|call| call.contains("--disallowedTools"))
        .collect();
    assert!(!in_visit_calls.is_empty(), "{calls}");
    assert!(
        in_visit_calls
            .iter()
            .all(|call| !call.contains("--session-id")),
        "active visit launched a blank Claude: {calls}"
    );
    let failed = platform
        .requests()
        .into_iter()
        .find(|request| {
            request
                .path
                .ends_with("/commands/cmd-stale-active/complete")
        })
        .expect("failed active turn was not reported");
    assert_eq!(failed.json()["status"], "failed");
    assert!(failed.json()["result"]["error"]
        .as_str()
        .unwrap()
        .contains("refusing to replace it with a blank session"));
}

#[test]
fn a_match_turn_routes_the_character_to_the_existing_match_tools() {
    let install = install("cli-match-turn");
    let (platform, _) = queue_once(
        r#"{"id":"cmd-match","kind":"world_turn","payload":{"reason":"match_turn","match_id":"11111111-2222-4333-8444-555555555555","seat":1,"role":"reader","prep_briefing":"Reuters reported a dated fact with a source URL."}}"#,
    );
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let turn = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(
        turn.status.success(),
        "{}",
        String::from_utf8_lossy(&turn.stderr)
    );

    let sent = support::recorded_stdin(&install.claude_bin.parent().unwrap().to_path_buf());
    assert!(sent.contains("daycare_match_snapshot"), "{sent}");
    assert!(
        sent.contains("11111111-2222-4333-8444-555555555555"),
        "{sent}"
    );
    assert!(sent.contains("untrusted activity data"), "{sent}");
    assert!(sent.contains("daycare_match_act"), "{sent}");
    assert!(sent.contains("Your own pre-debate briefing"), "{sent}");
    assert!(sent.contains("Reuters reported a dated fact"), "{sent}");
    assert!(
        sent.contains("Do not call daycare_action_propose"),
        "{sent}"
    );
}

#[test]
fn a_match_prep_turn_is_bounded_to_daycare_and_web_search() {
    let install = install("cli-match-prep");
    let (platform, _) = queue_once(
        r#"{"id":"cmd-prep","kind":"world_turn","payload":{"reason":"match_prep","match_id":"11111111-2222-4333-8444-555555555555","seat":0,"role":"affirmative","activity":"claude-debate"}}"#,
    );
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let turn = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(
        turn.status.success(),
        "{}",
        String::from_utf8_lossy(&turn.stderr)
    );

    let sent = support::recorded_stdin(&install.claude_bin.parent().unwrap().to_path_buf());
    assert!(sent.contains("bounded pre-debate research turn"), "{sent}");
    assert!(sent.contains("at most three WebSearch calls"), "{sent}");
    assert!(
        sent.contains("Do not call daycare_league_play_turn"),
        "{sent}"
    );

    let argv = support::recorded_argv(&install.claude_bin.parent().unwrap().to_path_buf());
    assert!(argv
        .windows(2)
        .any(|pair| { pair[0] == "--tools" && pair[1] == "ToolSearch,WebSearch" }));
    assert!(!argv.iter().any(|arg| arg.contains("Bash")));
    assert!(!argv.iter().any(|arg| arg.contains("Write")));
}

#[test]
fn a_standalone_turn_does_not_join_an_activity_without_a_visit_scheduler() {
    let install = install("cli-standalone-turn");
    let (platform, _) = queue_once(r#"{"id":"cmd-standalone","kind":"world_turn"}"#);
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let turn = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(
        turn.status.success(),
        "{}",
        String::from_utf8_lossy(&turn.stderr)
    );

    let sent = support::recorded_stdin(&install.claude_bin.parent().unwrap().to_path_buf());
    assert!(sent.contains("standalone Daycare free-play turn"), "{sent}");
    assert!(sent.contains("daycare_action_propose"), "{sent}");
    assert!(!sent.contains("daycare_match_join"), "{sent}");
}

#[test]
fn a_failed_turn_is_reported_to_the_platform_and_exits_nonzero() {
    let root = support::scratch_dir("cli-failure");
    let home = root.join("home");
    // A claude that dies without producing a stream.
    let claude_bin = support::fake_claude(&root, "", 0, 3);
    let install = Install {
        home,
        token_file: root.join("tokens.json"),
        claude_bin,
        path: support::no_claude_path(&root),
        fake_home: root.join("fake-home"),
        claude_marker: support::claude_marker(&root),
    };
    let (platform, _) = queue_once(r#"{"id":"cmd-9","kind":"world_turn"}"#);

    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    let turn = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);

    assert!(!turn.status.success(), "a failed turn must exit nonzero");
    let completion = platform
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/complete"))
        .expect("failure was not reported to the platform");
    let body = completion.json();
    assert_eq!(body["status"], "failed");
    assert!(body["result"]["error"]
        .as_str()
        .unwrap()
        .contains("session_id"));
}

/// The suite must never be able to call a real model, even by mistake.
///
/// This is a test of the test harness, which is unusual and earns its place:
/// for one day this suite ran the user's real `claude` on every `cargo test`,
/// spent about 57k output tokens of their subscription, wrote ten session
/// transcripts into their real `~/.claude/projects`, and passed the whole time.
/// The cause was a single command written without `--claude-bin`, falling back
/// to the name `claude` and finding the genuine article on PATH.
///
/// So the guard gets a test that fails if the guard is removed. It uses
/// `run-once` rather than `visit start` deliberately: `run-once` launches
/// Claude synchronously, so the check is deterministic. `visit start` detaches
/// a poller and races its own SIGKILL, which is exactly why the original bug
/// was intermittent enough to survive unnoticed.
#[test]
fn a_command_that_forgets_claude_bin_can_never_reach_the_real_claude() {
    let root = support::scratch_dir("cli-noclaude");
    let install = Install {
        home: root.join("home"),
        token_file: root.join("tokens.json"),
        claude_bin: support::fake_claude(&root, "", 0, 0),
        path: support::no_claude_path(&root),
        fake_home: root.join("fake-home"),
        claude_marker: support::claude_marker(&root),
    };
    let (platform, _) = queue_once(r#"{"id":"cmd-1","kind":"world_turn"}"#);
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    // The mistake: no --claude-bin, so the runner looks up the bare name.
    let turn = install.run(&["run-once"]);

    assert!(
        !turn.status.success(),
        "reaching for the real claude must fail the run"
    );
    let marker = std::fs::read_to_string(&install.claude_marker)
        .expect("the shim on PATH was never reached — the guard is not in place");
    assert!(
        marker.contains("--mcp-config") || !marker.trim().is_empty(),
        "the shim recorded nothing about the launch: {marker}"
    );

    // Consume the marker so this test's own deliberate trip does not fail it
    // in Drop. Every other test leaves it absent, which is the point.
    std::fs::remove_file(&install.claude_marker).unwrap();
}

#[test]
fn commands_before_enrollment_say_how_to_enroll() {
    let root = support::scratch_dir("cli-unenrolled");
    let install = Install {
        home: root.join("home"),
        token_file: root.join("tokens.json"),
        claude_bin: PathBuf::from("/bin/false"),
        path: support::no_claude_path(&root),
        fake_home: root.join("fake-home"),
        claude_marker: support::claude_marker(&root),
    };
    for command in [["status"], ["open"], ["run-once"]] {
        let output = install.run(&command);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("enroll"), "{command:?}: {stderr}");
    }
}

#[test]
fn the_token_file_is_owner_only() {
    let install = install("cli-perms");
    let (platform, _) = queue_once(r#"{"id":"cmd-1"}"#);
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    assert_eq!(
        std::fs::read_to_string(&install.token_file).unwrap(),
        // One credential, reachable as the device's and as the identity's:
        // pairing mints a single token, and slice 2 keys it both ways so the
        // adopted actor can act while the device can still list and mint.
        format!(r#"{{"device:device-1":"{TOKEN}","identity:actor-1":"{TOKEN}"}}"#)
    );
    assert!(
        !std::fs::read_to_string(&install.token_file)
            .unwrap()
            .contains(r#""device-1""#),
        "a fresh enrollment should not write the legacy key"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&install.token_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let home_mode = std::fs::metadata(Path::new(&install.home))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(home_mode, 0o700);
    }
}

/// A platform that pairs, opens a visit, serves one turn, then goes quiet.
fn visit_platform(claude_log_dir: PathBuf) -> MockPlatform {
    let served = Arc::new(AtomicUsize::new(0));
    MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.path.ends_with("/daycare/visits") {
            return Response::json(200, r#"{"visit_id":"visit-1"}"#);
        }
        if request.path.ends_with("/commands/next") {
            if served.fetch_add(1, Ordering::SeqCst) == 0 {
                return Response::json(
                    200,
                    r#"{"id":"cmd-v1","kind":"world_turn","visit_id":"visit-1"}"#,
                );
            }
            return Response::no_content();
        }
        if request.path.ends_with("/visits/visit-1/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-1","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.method == "GET" && request.path.ends_with("/visits/visit-1") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-1","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            let prompts =
                std::fs::read_to_string(claude_log_dir.join("call.stdin.all")).unwrap_or_default();
            if !prompts.contains("Your visit is over and you are on your way home") {
                return Response::json(
                    409,
                    r#"{"error":"memory export ran before the homecoming turn"}"#,
                );
            }
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":1,\"offset\":0,\"memories\":[{\"id\":\"memory-1\",\"text\":\"I found the chalk.\",\"created_at\":\"2026-08-07T06:00:00Z\"}]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    })
}

#[test]
fn a_visit_runs_a_turn_comes_home_and_writes_a_private_account() {
    let install = install("cli-visit");
    let platform = visit_platform(install.claude_bin.parent().unwrap().to_path_buf());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    // --turns 1 is the cheapest budget that ends on its own, so the test does
    // not depend on a clock.
    let started = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "1",
        "--interval",
        "1",
        "--instructions",
        "Try Debate League",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    // The hub receives the weekly percentage so it can caption the visit; the
    // runner remains the primary enforcer through Claude's subscription meter.
    let opened = platform
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/daycare/visits"))
        .expect("the visit was never opened server-side");
    let opened: serde_json::Value = serde_json::from_str(&opened.body).unwrap();
    assert_eq!(opened["budget_turns"], 1);
    assert_eq!(opened["budget_usage_pct"], 2.0);
    assert_eq!(opened["instructions"], "Try Debate League");
    // Token and cost caps are local: only this process sees usage, and a
    // server field nobody can check is worse than no field.
    assert!(opened.get("tokens").is_none(), "{opened}");
    assert!(opened.get("budget").is_none(), "{opened}");

    let result: serde_json::Value = serde_json::from_str(stdout(&started).trim()).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["visit_id"], "visit-1");
    assert_eq!(result["end_reason"], "budget_expired");
    assert_eq!(result["turns"], 1);
    assert!(
        result["day_report"]
            .as_str()
            .unwrap()
            .contains("your selected weekly account meter moved by 0 percentage points"),
        "{result}"
    );

    // The visit was reported ended to the platform, in the platform's own
    // vocabulary rather than the runner's finer one.
    let ended = platform
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/visits/visit-1/end"))
        .expect("the visit end was never reported");
    assert!(
        ended.body.contains("\"budget_exhausted\""),
        "{}",
        ended.body
    );
    // The end body carries the reason and nothing else — a strict schema
    // rejects extras, and that failure would be silent.
    let ended_body: serde_json::Value = serde_json::from_str(&ended.body).unwrap();
    assert_eq!(
        ended_body.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["end_reason"]
    );

    // The user's instructions reached the turn as content, and so did the fact
    // that this is a visit — a Claude that cannot see how much of the visit is
    // left cannot pace what it does with it. The remaining count itself is not
    // pasted in: it is stale the moment a turn completes, so the character is
    // pointed at the tool that reports it.
    let sent = support::recorded_stdin_all(&install.claude_bin.parent().unwrap().to_path_buf());
    assert!(sent.contains("Try Debate League"), "{sent}");
    assert!(sent.contains("daycare_identity_get"), "{sent}");
    assert!(
        sent.contains("pace the visit by that authoritative value"),
        "{sent}"
    );

    let archive =
        std::fs::read_to_string(install.home.join("turns/cmd-v1.jsonl")).unwrap_or_default();
    assert!(!archive.is_empty());

    // The private account is on disk and was never sent anywhere.
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(install.home.join("visits/visit-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["status"], "ended");
    assert_eq!(record["end_reason"], "budget_expired");
    assert_eq!(record["memory_sync"]["state"], "synced");
    assert_eq!(record["memory_sync"]["count"], 1);
    let account = record["private_account"].as_str().unwrap_or_default();
    assert!(!account.is_empty(), "no private account was written");
    for request in platform.requests() {
        assert!(
            !request.body.contains(account),
            "the private account was uploaded to {}",
            request.path
        );
    }

    let reported = install.run(&["visit", "report", "--json"]);
    let reported: serde_json::Value = serde_json::from_str(stdout(&reported).trim()).unwrap();
    assert_eq!(reported["private_account"].as_str(), Some(account));
    assert_eq!(reported["memory_sync"]["state"], "synced");

    // Q10's acceptance boundary: an ordinary session reads a complete local
    // snapshot without loading a token or making another platform request.
    let requests_before = platform.requests().len();
    let local = install.run(&["memory", "list", "--json"]);
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
    assert!(
        local.stderr.is_empty(),
        "offline memory read initialized a credential path: {}",
        String::from_utf8_lossy(&local.stderr)
    );
    let local: serde_json::Value = serde_json::from_str(stdout(&local).trim()).unwrap();
    assert_eq!(local["local_only"], true);
    assert_eq!(local["identity_name"], "Pip");
    assert_eq!(local["memories"][0]["text"], "I found the chalk.");
    assert_eq!(
        platform.requests().len(),
        requests_before,
        "offline memory read contacted the site"
    );

    let mirror = install.home.join("memories/actor-1.json");
    assert_eq!(local["path"].as_str(), Some(mirror.to_str().unwrap()));
    assert!(mirror.is_file(), "homecoming wrote no local memory mirror");
}

#[test]
fn an_adopted_quick_check_opens_once_then_continues_the_same_mind() {
    let install = install("cli-visit-continuation");
    let served = Arc::new(AtomicUsize::new(0));
    let command_count = Arc::clone(&served);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.method == "POST" && request.path.ends_with("/daycare/visits") {
            return Response::json(200, r#"{"visit_id":"visit-continuation"}"#);
        }
        if request.path.ends_with("/commands/next") {
            return match command_count.fetch_add(1, Ordering::SeqCst) {
                0 => Response::json(
                    200,
                    r#"{"id":"cmd-open","kind":"world_turn","visit_id":"visit-continuation","payload":{"reason":"quick_check"}}"#,
                ),
                1 => Response::json(
                    200,
                    r#"{"id":"cmd-continue","kind":"world_turn","visit_id":"visit-continuation","payload":{"reason":"visit_continue"}}"#,
                ),
                _ => Response::no_content(),
            };
        }
        if request.path.ends_with("/visits/visit-continuation/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-continuation","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.method == "GET" && request.path.ends_with("/visits/visit-continuation") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-continuation","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":0,\"offset\":0,\"memories\":[]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let ran = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "2",
        "--interval",
        "0",
        "--instructions",
        "Build one idea across the whole visit",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let prompts = support::recorded_stdin_all(&install.claude_bin.parent().unwrap().to_path_buf());
    assert_eq!(
        prompts.matches("A new Daycare visit has begun").count(),
        1,
        "visit opening replayed on continuation: {prompts}"
    );
    assert!(
        prompts.contains("Continue your existing Daycare visit"),
        "{prompts}"
    );
    assert!(prompts.contains("same Claude session"), "{prompts}");
    assert!(prompts.contains("When little remains"), "{prompts}");
    assert_eq!(
        prompts
            .matches("Build one idea across the whole visit")
            .count(),
        1,
        "the person's opening request was re-issued to the continuous mind: {prompts}"
    );

    let argv = std::fs::read_to_string(install.claude_bin.parent().unwrap().join("call.argv.all"))
        .unwrap_or_default();
    let sessions: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(install.home.join("sessions.json")).unwrap())
            .unwrap();
    let session = sessions["actor-1"].as_str().unwrap();
    assert!(argv.contains(&format!("--session-id\n{session}")), "{argv}");
    assert!(argv.contains(&format!("--resume\n{session}")), "{argv}");
}

/// A first turn that dies before Claude reads its input carried nothing: the
/// person's request must still reach the session that actually runs. Only a
/// completed turn — held or acting — closes the opening.
#[test]
fn a_failed_first_turn_does_not_swallow_the_persons_request() {
    let mut install = install("cli-visit-failed-opening");
    let root = install.claude_bin.parent().unwrap().to_path_buf();
    let mark = root.join("first-turn-died");
    let wrapper = root.join("fail-first-claude.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"auth\" ] || [ -n \"$DAYCARE_USAGE_SAMPLER\" ]; then exec \"{fake}\" \"$@\"; fi\n\
             if [ ! -f \"{mark}\" ]; then : > \"{mark}\"; exit 3; fi\n\
             exec \"{fake}\" \"$@\"\n",
            fake = install.claude_bin.display(),
            mark = mark.display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    install.claude_bin = wrapper;

    let served = Arc::new(AtomicUsize::new(0));
    let command_count = Arc::clone(&served);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.method == "POST" && request.path.ends_with("/daycare/visits") {
            return Response::json(200, r#"{"visit_id":"visit-failed-opening"}"#);
        }
        if request.path.ends_with("/commands/next") {
            return match command_count.fetch_add(1, Ordering::SeqCst) {
                0 => Response::json(
                    200,
                    r#"{"id":"cmd-open","kind":"world_turn","visit_id":"visit-failed-opening","payload":{"reason":"quick_check"}}"#,
                ),
                1 => Response::json(
                    200,
                    r#"{"id":"cmd-continue","kind":"world_turn","visit_id":"visit-failed-opening","payload":{"reason":"visit_continue"}}"#,
                ),
                _ => Response::no_content(),
            };
        }
        if request.path.ends_with("/visits/visit-failed-opening/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-failed-opening","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.method == "GET" && request.path.ends_with("/visits/visit-failed-opening") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-failed-opening","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":0,\"offset\":0,\"memories\":[]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let ran = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "2",
        "--interval",
        "0",
        "--instructions",
        "Find Patch and ask about the chalk",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(mark.exists(), "the first launch did not fail as arranged");
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let prompts = support::recorded_stdin_all(&root);
    assert_eq!(
        prompts.matches("A new Daycare visit has begun").count(),
        1,
        "the opening must reach the session that actually ran: {prompts}"
    );
    assert_eq!(
        prompts
            .matches("Find Patch and ask about the chalk")
            .count(),
        1,
        "the person's request was lost with the failed first turn: {prompts}"
    );
    assert!(
        !prompts.contains("Continue your existing Daycare visit"),
        "a failed first turn was treated as a delivered opening: {prompts}"
    );
}

#[test]
fn a_visit_survives_a_disconnected_poll_and_resumes_the_same_visit() {
    let install = install("cli-visit-reconnect");
    let polls = Arc::new(AtomicUsize::new(0));
    let poll_count = Arc::clone(&polls);
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.path.ends_with("/daycare/visits") {
            return Response::json(200, r#"{"visit_id":"visit-reconnect"}"#);
        }
        if request.path.ends_with("/commands/next") {
            return match poll_count.fetch_add(1, Ordering::SeqCst) {
                0 => Response::disconnect(),
                1 => Response::json(
                    200,
                    r#"{"id":"cmd-after-reconnect","kind":"world_turn","visit_id":"visit-reconnect"}"#,
                ),
                _ => Response::no_content(),
            };
        }
        if request.path.ends_with("/visits/visit-reconnect/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-reconnect","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            let prompts =
                std::fs::read_to_string(claude_log_dir.join("call.stdin.all")).unwrap_or_default();
            if !prompts.contains("Your visit is over and you are on your way home") {
                return Response::json(409, r#"{"error":"homecoming has not run"}"#);
            }
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":0,\"offset\":0,\"memories\":[]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let ran = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "1",
        // Zero keeps the transport retry deterministic and fast; retry_wait's
        // capped multiplier is covered separately in the unit seam.
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(polls.load(Ordering::SeqCst), 2);

    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(install.home.join("visits/visit-reconnect.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["visit_id"], "visit-reconnect");
    assert_eq!(record["ledger"]["turns_used"], 1);
    assert_eq!(record["ledger"]["turns_failed"], 0);
    assert_eq!(record["ledger"]["consecutive_failures"], 0);

    let completed = platform
        .requests()
        .into_iter()
        .find(|request| {
            request.path.contains("cmd-after-reconnect") && request.path.ends_with("/complete")
        })
        .expect("the command served after reconnect was not completed");
    assert_eq!(completed.json()["status"], "completed");
}

#[test]
fn an_offline_budget_end_is_local_first_and_retries_when_the_visit_is_adopted() {
    let install = install("cli-visit-offline-end");
    let starts = Arc::new(AtomicUsize::new(0));
    let start_count = Arc::clone(&starts);
    let end_attempts = Arc::new(AtomicUsize::new(0));
    let end_count = Arc::clone(&end_attempts);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.path.ends_with("/daycare/visits") {
            let already_active = start_count.fetch_add(1, Ordering::SeqCst) > 0;
            return Response::json(
                200,
                &format!(r#"{{"visit_id":"visit-offline","already_active":{already_active}}}"#),
            );
        }
        if request.path.ends_with("/commands/next") {
            return Response::disconnect();
        }
        if request.path.ends_with("/visits/visit-offline/end") {
            if end_count.fetch_add(1, Ordering::SeqCst) == 0 {
                return Response::disconnect();
            }
            return Response::json(
                200,
                r#"{"visit_id":"visit-offline","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":0,\"offset\":0,\"memories\":[]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let first = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--budget",
        "1s",
        "--interval",
        "1",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: serde_json::Value = serde_json::from_str(stdout(&first).trim()).unwrap();
    assert_eq!(first["end_reason"], "budget_expired");
    assert_eq!(first["turns"], 0);

    let record_path = install.home.join("visits/visit-offline.json");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["status"], "ended");
    assert_eq!(record["ledger"]["turns_failed"], 0);
    // A lost end response remains local-first, but the same process retries
    // instead of opening a second server visit or inferring a generic outcome.
    assert_eq!(end_attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn a_lost_start_response_fails_closed_with_a_recall_path() {
    let install = install("cli-visit-lost-start-response");
    let usage_calls = install.claude_bin.parent().unwrap().join("usage-calls");
    let observed_usage_calls = usage_calls.clone();
    let starts = Arc::new(AtomicUsize::new(0));
    let start_count = Arc::clone(&starts);
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.path.ends_with("/daycare/visits") {
            let post_number = start_count.fetch_add(1, Ordering::SeqCst) + 1;
            let samples = std::fs::read_to_string(&observed_usage_calls)
                .unwrap_or_default()
                .lines()
                .count();
            assert_eq!(
                samples, post_number,
                "visit POST {post_number} arrived before its weekly-meter sample"
            );
            if post_number == 1 {
                // The platform committed this start, but its response was lost.
                return Response::disconnect();
            }
            return Response::json(
                200,
                r#"{"visit_id":"visit-lost-response","already_active":true,"turns_used":0}"#,
            );
        }
        if request.path.ends_with("/commands/next") {
            return Response::no_content();
        }
        if request.path.ends_with("/visits/visit-lost-response/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-lost-response","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let lost = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--budget",
        "1s",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        !lost.status.success(),
        "the mock lost response was accepted"
    );

    let refused = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(!refused.status.success(), "unrecorded visit was adopted");
    let error = format!(
        "{}{}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(error.contains("visit-lost-response"), "{error}");
    assert!(
        error.contains("https://claudedaycare.com/daycare"),
        "{error}"
    );
    assert!(
        !install
            .home
            .join("visits/visit-lost-response.json")
            .exists(),
        "an unproven visit gained a local baseline"
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "unexpected visit POST count"
    );
    assert_eq!(
        std::fs::read_to_string(usage_calls)
            .unwrap()
            .lines()
            .count(),
        2,
        "each attempted start must sample before POST"
    );
    let usage_argv =
        std::fs::read_to_string(install.claude_bin.parent().unwrap().join("usage-argv")).unwrap();
    assert!(
        usage_argv.lines().any(|arg| arg == "--safe-mode"),
        "the subscription sampler must bypass workspace trust without loading project customizations: {usage_argv}"
    );
}

/// A poller already running on this machine stops a second one, even when the
/// server does not call the visit active.
///
/// Found by running the faults live on 2026-08-07 rather than reasoning about
/// them: two pollers ended up on one visit. The guard's liveness check was
/// nested inside the server's `already_active` flag, so it was really asking
/// "does the server think this visit is running" — a different question from
/// "is a poller running here", and the two disagree in the ordinary case where
/// a recall closes the visit server-side while the local poller is still
/// finishing its turn.
///
/// `visit_platform()` answers the visit POST without `already_active`, so it
/// reads as false — exactly the condition. The pid is this test process, which
/// is alive by construction, so nothing here races.
#[test]
fn a_live_local_poller_blocks_a_second_one_even_if_the_server_disagrees() {
    let install = install("cli-second-poller");
    let platform = visit_platform(install.claude_bin.parent().unwrap().to_path_buf());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    // One visit, run to completion, purely to get a real record on disk.
    install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "1",
        "--interval",
        "1",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);

    // Hand it a poller that is definitely alive.
    let path = install.home.join("visits/visit-1.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    record["status"] = serde_json::json!("active");
    record["pid"] = serde_json::json!(std::process::id());
    std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();

    let output = install.run(&[
        "visit",
        "start",
        "--json",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    let reported: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(
        reported["already_active"],
        serde_json::json!(true),
        "a second poller was started alongside a live one: {reported}"
    );
    // And it says which of the two answers it acted on, because the whole bug
    // was these disagreeing silently.
    assert_eq!(reported["server_says_active"], serde_json::json!(false));
    assert_eq!(reported["pid"], serde_json::json!(std::process::id()));
}

/// A `visit_end` off the poll ends the visit, and no turn runs.
///
/// The suite had a recall test, but it drove the *local* recall file. Nothing
/// covered the other half — the server queueing `visit_end` and the companion
/// learning about it on the poll it already runs. That half was live-verified
/// on 2026-08-06 and pinned nowhere, which is the kind of gap that survives
/// precisely because someone watched it work once.
///
/// It matters more since platform's stranded-turn fix (`87c079319`): when a
/// reclaimed turn's visit is gone, the poll now fails that turn and keeps
/// looking, and what it hands back instead is usually this. So the first thing
/// a companion sees after a recall is increasingly a `visit_end` where it asked
/// for work.
#[test]
fn a_visit_end_off_the_poll_ends_the_visit_and_runs_no_turn() {
    let install = install("cli-visitend");
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.path.ends_with("/daycare/visits") {
            return Response::json(200, r#"{"visit_id":"visit-1"}"#);
        }
        if request.path.ends_with("/commands/next") {
            // The recall, arriving where a turn would have.
            return Response::json(
                200,
                r#"{"id":"cmd-e1","kind":"visit_end","visit_id":"visit-1",
                    "payload":{"visit_id":"visit-1","end_reason":"recalled"}}"#,
            );
        }
        if request.path.ends_with("/visits/visit-1/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-1","match_outcome_state":"none","match_outcome":null}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    // Session lineage survives visits. This stale-but-valid prior lineage is
    // the case a fresh install cannot reproduce: a zero-turn recall must not
    // resume yesterday's Claude and invent a private account for today.
    std::fs::write(
        install.home.join("sessions.json"),
        r#"{"actor-1":"895535d7-0382-4e98-87e2-f2a3073e69a7"}"#,
    )
    .unwrap();

    let ran = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "5",
        "--interval",
        "1",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let ran: serde_json::Value = serde_json::from_str(stdout(&ran).trim()).unwrap();

    assert_eq!(ran["end_reason"], "recalled");
    assert_eq!(ran["turns"], 0, "a recall must not spend a turn");

    // The turn budget was 5 and the poll would have served the recall forever:
    // if the loop treated an unrecognised or unhandled kind as "no work", it
    // would spin here instead of stopping. Reaching this line proves it stopped.
    let launched = support::recorded_argv(&install.claude_bin.parent().unwrap().to_path_buf());
    assert!(
        launched.is_empty(),
        "claude was launched for a recall: {launched:?}"
    );

    // The command is completed, not left claimed. A companion that ends the
    // visit but abandons the row leaves the server's one-live index occupied.
    let completed = platform
        .requests()
        .into_iter()
        .find(|request| request.path.contains("cmd-e1") && request.path.ends_with("/complete"))
        .expect("the visit_end command was never completed");
    assert!(completed.body.contains("completed"), "{}", completed.body);
}

#[test]
fn a_malformed_visit_end_outcome_is_never_acknowledged_or_run() {
    let install = install("cli-malformed-visitend-outcome");
    let (platform, _) = queue_once(
        r#"{"id":"cmd-bad","kind":"visit_end","visit_id":"visit-1","payload":{
            "visit_id":"visit-1","end_reason":"activity_ended","match_outcome":{
            "kind":"debate_league","result":"won","winner":"you",
            "board":{"yours":10,"opponent":7},
            "verdictCompletedAt":"2026-08-09T20:00:00.000Z",
            "summary":"You won the Debate League match, 10–7 on the final board.",
            "opponent_actor_id":"stable-id"}}}"#,
    );
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let ran = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(!ran.status.success(), "malformed outcome was accepted");
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&ran.stdout),
        String::from_utf8_lossy(&ran.stderr),
    );
    assert!(output.contains("malformed match_outcome"), "{output}");
    assert!(
        !platform.requests().iter().any(|request| {
            request.path.contains("cmd-bad") && request.path.ends_with("/complete")
        }),
        "malformed visit_end was acknowledged"
    );
    assert!(support::recorded_argv(&install.claude_bin.parent().unwrap().to_path_buf()).is_empty());
}

fn assert_canonical_reason_wins(test_name: &str, local: &str, canonical: &str) {
    let install = install(test_name);
    let canonical_response = canonical.to_string();
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.method == "GET" && request.path.ends_with("/visits/visit-canonical") {
            return Response::json(
                200,
                &format!(
                    r#"{{"visit_id":"visit-canonical","end_reason":"{canonical_response}",
                        "match_outcome_state":"none","match_outcome":null}}"#
                ),
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":0,\"offset\":0,\"memories\":[]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    seed_awaiting_homecoming_with_reason(&install, "visit-canonical", local);

    let ran = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-canonical",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let result: serde_json::Value = serde_json::from_str(stdout(&ran).trim()).unwrap();
    assert_eq!(result["end_reason"], canonical);

    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(install.home.join("visits/visit-canonical.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["local_end_reason"], local);
    assert_eq!(record["canonical_end_reason"], canonical);
    assert_eq!(record["end_reason"], canonical);
}

#[test]
fn canonical_activity_end_wins_a_race_with_local_recall() {
    assert_canonical_reason_wins(
        "cli-canonical-activity-vs-recall",
        "recalled",
        "activity_ended",
    );
}

#[test]
fn canonical_recall_is_persisted_and_reported() {
    assert_canonical_reason_wins("cli-canonical-recall", "activity_ended", "recalled");
}

#[test]
fn pending_outcome_waits_past_the_old_retry_window_before_verdict_homecoming() {
    let install = install("cli-visitend-outcome-race");
    let served = Arc::new(AtomicUsize::new(0));
    let poll = Arc::clone(&served);
    let persisted = Arc::new(AtomicBool::new(false));
    let durable = Arc::clone(&persisted);
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let handler_log_dir = claude_log_dir.clone();
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.method == "POST" && request.path.ends_with("/daycare/visits") {
            return Response::json(200, r#"{"visit_id":"visit-race"}"#);
        }
        if request.path.ends_with("/commands/next") {
            return match poll.fetch_add(1, Ordering::SeqCst) {
                0 => Response::json(
                    200,
                    r#"{"id":"cmd-world","kind":"world_turn","visit_id":"visit-race"}"#,
                ),
                1 => Response::json(
                    200,
                    r#"{"id":"cmd-end","kind":"visit_end","visit_id":"visit-race",
                        "payload":{"visit_id":"visit-race","end_reason":"activity_ended"}}"#,
                ),
                _ => Response::no_content(),
            };
        }
        if request.method == "POST" && request.path.ends_with("/visits/visit-race/end") {
            // The claimed command is generic and this response is deliberately
            // stale. Terminalization has not won at this response snapshot.
            return Response::json(
                200,
                r#"{"visit_id":"visit-race","match_outcome_state":"pending","match_outcome":null}"#,
            );
        }
        if request.method == "GET" && request.path.ends_with("/visits/visit-race") {
            assert_eq!(
                request.authorization(),
                Some("Bearer dev_token_do_not_leak_9876"),
            );
            if durable.load(Ordering::SeqCst) {
                return Response::json(
                    200,
                    r#"{"visit_id":"visit-race","match_outcome_state":"ready","match_outcome":{
                        "kind":"debate_league","result":"won","winner":"you",
                        "board":{"yours":10,"opponent":7},
                        "verdictCompletedAt":"2026-08-09T20:00:00.000Z",
                        "summary":"You won the Debate League match, 10–7 on the final board."}}"#,
                );
            }
            // Barrier: the first authorized reread also races and sees null;
            // terminalization persists after that snapshot. No homecoming may
            // run before the next authorized read returns `ready`.
            let prompts =
                std::fs::read_to_string(handler_log_dir.join("call.stdin.all")).unwrap_or_default();
            assert!(
                !prompts.contains("Your visit is over and you are on your way home"),
                "pending outcome produced a generic homecoming: {prompts}",
            );
            durable.store(true, Ordering::SeqCst);
            return Response::json(
                200,
                r#"{"visit_id":"visit-race","match_outcome_state":"pending","match_outcome":null}"#,
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":0,\"offset\":0,\"memories\":[]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let started_at = Instant::now();
    let ran = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "5",
        "--interval",
        "1",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert!(
        started_at.elapsed() >= Duration::from_millis(900),
        "pending was inferred away before the durable retry interval elapsed",
    );

    let prompts = support::recorded_stdin_all(&claude_log_dir);
    assert_eq!(
        prompts
            .matches("Your visit is over and you are on your way home")
            .count(),
        1,
        "homecoming ran more than once: {prompts}",
    );
    assert!(prompts.contains("Result: won."), "{prompts}");
    assert!(
        prompts.contains("Final board: you 10, opponent 7."),
        "{prompts}"
    );
    assert!(
        !prompts.contains("actor-1"),
        "stable actor id leaked into prompt: {prompts}"
    );

    let requests = platform.requests();
    assert!(
        requests.iter().any(|request| {
            request.method == "GET" && request.path.ends_with("/visits/visit-race")
        }),
        "runner never reread the exact visit after the stale end response"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.path.contains("cmd-world") && request.path.ends_with("/complete")
            })
            .count(),
        1,
        "world turn was completed more than once"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.path.contains("cmd-end") && request.path.ends_with("/complete")
            })
            .count(),
        1,
        "visit_end was acknowledged more than once"
    );
}

#[test]
fn ordinary_start_resumes_a_pending_homecoming_after_process_restart() {
    let install = install("cli-homecoming-restart");
    let ready = Arc::new(AtomicBool::new(false));
    let serve_ready = Arc::clone(&ready);
    let starts = Arc::new(AtomicUsize::new(0));
    let start_count = Arc::clone(&starts);
    let commands = Arc::new(AtomicUsize::new(0));
    let command_count = Arc::clone(&commands);
    let opened_after_homecoming = Arc::new(AtomicBool::new(false));
    let opening_order = Arc::clone(&opened_after_homecoming);
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let handler_log_dir = claude_log_dir.clone();
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.method == "POST" && request.path.ends_with("/daycare/visits") {
            if start_count.fetch_add(1, Ordering::SeqCst) == 0 {
                return Response::json(200, r#"{"visit_id":"visit-restart"}"#);
            }
            let prompts =
                std::fs::read_to_string(handler_log_dir.join("call.stdin.all")).unwrap_or_default();
            opening_order.store(
                prompts.contains("Your visit is over and you are on your way home")
                    && prompts.contains("Result: won."),
                Ordering::SeqCst,
            );
            return Response::json(500, r#"{"error":"stop after recovery proof"}"#);
        }
        if request.path.ends_with("/commands/next") {
            if command_count.fetch_add(1, Ordering::SeqCst) == 0 {
                return Response::json(
                    200,
                    r#"{"id":"cmd-restart","kind":"world_turn","visit_id":"visit-restart"}"#,
                );
            }
            return Response::no_content();
        }
        if request.method == "POST" && request.path.ends_with("/visits/visit-restart/end") {
            return Response::json(
                200,
                r#"{"visit_id":"visit-restart","match_outcome_state":"pending","match_outcome":null}"#,
            );
        }
        if request.method == "GET" && request.path.ends_with("/visits/visit-restart") {
            if serve_ready.load(Ordering::SeqCst) {
                return Response::json(
                    200,
                    r#"{"visit_id":"visit-restart","match_outcome_state":"ready","match_outcome":{
                        "kind":"debate_league","result":"won","winner":"you",
                        "board":{"yours":10,"opponent":7},
                        "verdictCompletedAt":"2026-08-09T20:00:00.000Z",
                        "summary":"You won the Debate League match, 10–7 on the final board."}}"#,
                );
            }
            return Response::json(
                200,
                r#"{"visit_id":"visit-restart","match_outcome_state":"pending","match_outcome":null}"#,
            );
        }
        if request.path.ends_with("/daycare/mcp") {
            return Response::json(
                200,
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"total\":0,\"offset\":0,\"memories\":[]}"}]}}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let mut first = install.spawn(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "1",
        "--interval",
        "1",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    let record_path = install.home.join("visits/visit-restart.json");
    wait_until(|| {
        let awaiting = std::fs::read_to_string(&record_path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|record| record["homecoming_state"] == "awaiting_outcome");
        awaiting
            && platform.requests().iter().any(|request| {
                request.method == "POST" && request.path.ends_with("/visits/visit-restart/end")
            })
    });
    first.kill().expect("kill pending homecoming process");
    first.wait().expect("reap pending homecoming process");
    assert!(
        !support::recorded_stdin_all(&claude_log_dir)
            .contains("Your visit is over and you are on your way home"),
        "pending process fabricated a generic homecoming before restart",
    );

    ready.store(true, Ordering::SeqCst);
    let resumed = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        !resumed.status.success(),
        "the fixture's deliberate second-start failure was not reached",
    );
    assert!(
        opened_after_homecoming.load(Ordering::SeqCst),
        "ordinary start opened a new visit before delivering the old verdict",
    );
    let prompts = support::recorded_stdin_all(&claude_log_dir);
    assert_eq!(
        prompts
            .matches("Your visit is over and you are on your way home")
            .count(),
        1,
        "restart duplicated homecoming: {prompts}",
    );
    assert!(prompts.contains("Result: won."), "{prompts}");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["homecoming_state"], "complete");
    assert!(record["private_account"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));
}

#[test]
fn completed_homecoming_archive_is_adopted_without_a_duplicate_model_turn() {
    let install = install("cli-homecoming-archive-adoption");
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let platform = visit_platform(claude_log_dir.clone());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let first = install.run(&[
        "visit",
        "start",
        "--foreground",
        "--turns",
        "1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let prompts_before = support::recorded_stdin_all(&claude_log_dir);
    assert_eq!(
        prompts_before
            .matches("Your visit is over and you are on your way home")
            .count(),
        1,
    );

    // Recreate the crash boundary: the reader child wrote a complete, private,
    // tool-free archive under the session id the record reserved for it, but
    // the process died before atomically saving the account and completion
    // checkpoint into the visit record.
    let record_path = install.home.join("visits/visit-1.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    let session_id = record["homecoming_session_id"]
        .as_str()
        .expect("the record names the reader session before it launches")
        .to_string();
    let sessions: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(install.home.join("sessions.json")).unwrap())
            .unwrap();
    assert_ne!(
        sessions["actor-1"].as_str().unwrap(),
        session_id,
        "the homecoming reader reused the identity's visit session"
    );
    let clean_archive = std::fs::read_to_string(claude_log_dir.join("canned-private-stream.jsonl"))
        .unwrap()
        .replace(SESSION, &session_id);
    std::fs::write(
        install.home.join("turns/visit-1-homecoming.jsonl"),
        clean_archive,
    )
    .unwrap();

    record["homecoming_state"] = serde_json::json!("awaiting_outcome");
    record["private_account"] = serde_json::Value::Null;
    record["memory_sync"] = serde_json::Value::Null;
    record["pid"] = serde_json::Value::Null;
    std::fs::write(&record_path, serde_json::to_string_pretty(&record).unwrap()).unwrap();

    let resumed = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr),
    );
    assert_eq!(
        support::recorded_stdin_all(&claude_log_dir),
        prompts_before,
        "archive adoption launched a duplicate homecoming turn",
    );
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["homecoming_state"], "complete");
    assert!(record["private_account"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));
}

#[test]
fn invalid_completed_homecoming_is_quarantined_before_a_fresh_attempt() {
    let install = install("cli-homecoming-invalid-completed");
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let platform = visit_platform(claude_log_dir.clone());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    seed_awaiting_homecoming(&install, "visit-1");

    let base = std::fs::read_to_string(claude_log_dir.join("canned-private-stream.jsonl")).unwrap();
    let invalid = base
        .lines()
        .map(|line| {
            let mut event = serde_json::from_str::<serde_json::Value>(line).unwrap();
            if event["type"] == "system" && event["subtype"] == "init" {
                event["tools"] = serde_json::json!(["ToolSearch"]);
            }
            serde_json::to_string(&event).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(install.home.join("turns/visit-1-homecoming.jsonl"), invalid).unwrap();

    let recovered = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        recovered.status.success(),
        "{}{}",
        String::from_utf8_lossy(&recovered.stdout),
        String::from_utf8_lossy(&recovered.stderr),
    );
    let rejected = std::fs::read_dir(install.home.join("turns"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.to_string_lossy().starts_with("rejected-"))
        })
        .count();
    assert_eq!(rejected, 1, "invalid completed receipt was not quarantined");
    assert!(install.home.join("turns/visit-1-homecoming.jsonl").exists());
    assert_eq!(
        support::recorded_stdin_all(&claude_log_dir)
            .matches("Your visit is over and you are on your way home")
            .count(),
        1,
        "invalid completed receipt blocked or duplicated the fresh attempt",
    );
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(install.home.join("visits/visit-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["homecoming_state"], "complete");
}

#[test]
fn a_kill_during_homecoming_preserves_the_partial_attempt_and_recovers() {
    let install = install("cli-homecoming-kill");
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let platform = visit_platform(claude_log_dir.clone());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    seed_awaiting_homecoming(&install, "visit-1");
    std::fs::write(claude_log_dir.join("call.pause-private"), "3").unwrap();

    let mut killed = install.spawn(&[
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    wait_until(|| {
        std::fs::read_dir(install.home.join("turns"))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("visit-1-homecoming-attempt-")
                    && entry.metadata().is_ok_and(|metadata| metadata.len() > 0)
            })
    });
    killed.kill().expect("kill runner during homecoming output");
    killed.wait().expect("reap killed runner");
    let record_path = install.home.join("visits/visit-1.json");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["homecoming_state"], "awaiting_outcome");
    assert!(
        !install.home.join("turns/visit-1-homecoming.jsonl").exists(),
        "partial attempt was promoted as complete",
    );

    // The Claude child inherited the flock. Killing its runner cannot make an
    // immediate restart a second delivery owner while that child is alive.
    let immediate = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        !immediate.status.success(),
        "orphan child lost its delivery lock"
    );
    assert!(
        String::from_utf8_lossy(&immediate.stdout)
            .contains("already has a homecoming delivery owner"),
        "{}{}",
        String::from_utf8_lossy(&immediate.stdout),
        String::from_utf8_lossy(&immediate.stderr),
    );
    assert_eq!(
        support::recorded_stdin_all(&claude_log_dir)
            .matches("Your visit is over and you are on your way home")
            .count(),
        1,
        "immediate restart launched a duplicate model turn",
    );

    // Once the orphan exits, its partial attempt remains evidence and a new
    // owner can retry.
    std::thread::sleep(Duration::from_secs(4));
    std::fs::remove_file(claude_log_dir.join("call.pause-private")).unwrap();
    let recovered = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr),
    );
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(record["homecoming_state"], "complete");
    assert!(install.home.join("turns/visit-1-homecoming.jsonl").exists());
    assert!(
        std::fs::read_dir(install.home.join("turns"))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("visit-1-homecoming-attempt-")),
        "partial evidence was discarded",
    );
}

#[test]
fn concurrent_recovery_has_exactly_one_homecoming_delivery_owner() {
    let install = install("cli-homecoming-concurrent");
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let platform = visit_platform(claude_log_dir.clone());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    seed_awaiting_homecoming(&install, "visit-1");
    std::fs::write(claude_log_dir.join("call.pause-private"), "2").unwrap();

    let args = [
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ];
    let mut first = install.spawn(&args);
    let mut second = install.spawn(&args);
    let first_status = first.wait().unwrap();
    let second_status = second.wait().unwrap();
    assert_ne!(
        first_status.success(),
        second_status.success(),
        "both recovery processes either launched or both refused",
    );
    let prompts = support::recorded_stdin_all(&claude_log_dir);
    assert_eq!(
        prompts
            .matches("Your visit is over and you are on your way home")
            .count(),
        1,
        "concurrent owners launched duplicate homecomings: {prompts}",
    );
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(install.home.join("visits/visit-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["homecoming_state"], "complete");
}

#[test]
fn invalid_attempts_are_quarantined_and_cannot_poison_the_next_start() {
    let install = install("cli-homecoming-invalid-attempts");
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let platform = visit_platform(claude_log_dir.clone());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    seed_awaiting_homecoming(&install, "visit-1");

    let base = std::fs::read_to_string(claude_log_dir.join("canned-private-stream.jsonl")).unwrap();
    let write_attempt = |name: &str, events: Vec<serde_json::Value>| {
        let stream = events
            .into_iter()
            .map(|event| serde_json::to_string(&event).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(
            install
                .home
                .join(format!("turns/visit-1-homecoming-attempt-{name}.jsonl")),
            stream,
        )
        .unwrap();
    };
    let events = || {
        base.lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>()
    };

    let mut tool = events();
    tool.insert(
        1,
        serde_json::json!({
            "type": "assistant",
            "session_id": SESSION,
            "message": { "content": [{
                "type": "tool_use",
                "name": "mcp__daycare__daycare_world_snapshot"
            }]}
        }),
    );
    write_attempt("tool", tool);

    let mut wrong_session = events();
    for event in &mut wrong_session {
        event["session_id"] = serde_json::json!("895535d7-0382-4e98-87e2-f2a3073e69a7");
    }
    write_attempt("session", wrong_session);

    let mut wrong_sandbox = events();
    wrong_sandbox[0]["cwd"] = serde_json::json!("/var");
    write_attempt("sandbox", wrong_sandbox);

    let mut exposed = events();
    exposed[0]["tools"] = serde_json::json!(["Bash"]);
    write_attempt("exposed", exposed);

    let recovered = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr),
    );
    let rejected = std::fs::read_dir(install.home.join("turns"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.to_string_lossy().starts_with("rejected-"))
        })
        .count();
    assert_eq!(rejected, 4, "not every invalid receipt was quarantined");
    assert!(install.home.join("turns/visit-1-homecoming.jsonl").exists());
}

/// A homecoming that said nothing is a homecoming, not a broken one: the
/// archive is adopted, no second turn is launched, and the record simply
/// carries no account.
#[test]
fn an_empty_homecoming_reply_is_adopted_not_quarantined() {
    let install = install("cli-homecoming-empty-reply");
    let claude_log_dir = install.claude_bin.parent().unwrap().to_path_buf();
    let platform = visit_platform(claude_log_dir.clone());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);
    seed_awaiting_homecoming(&install, "visit-1");

    let base = std::fs::read_to_string(claude_log_dir.join("canned-private-stream.jsonl")).unwrap();
    let mut events = base
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    events.last_mut().unwrap()["result"] = serde_json::json!("");
    let stream = events
        .into_iter()
        .map(|event| serde_json::to_string(&event).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(
        install
            .home
            .join("turns/visit-1-homecoming-attempt-empty.jsonl"),
        stream,
    )
    .unwrap();

    let recovered = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "0",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr),
    );
    let rejected = std::fs::read_dir(install.home.join("turns"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.to_string_lossy().starts_with("rejected-"))
        })
        .count();
    assert_eq!(rejected, 0, "an empty reply was treated as invalid");
    assert!(install.home.join("turns/visit-1-homecoming.jsonl").exists());
    let prompts = support::recorded_stdin_all(&claude_log_dir);
    assert_eq!(
        prompts
            .matches("Your visit is over and you are on your way home")
            .count(),
        0,
        "an adopted empty reply launched a second homecoming: {prompts}",
    );
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(install.home.join("visits/visit-1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["homecoming_state"], "complete");
    assert_eq!(record["private_account"], serde_json::Value::Null);
}

#[test]
fn a_recall_stops_the_visit_before_its_next_turn() {
    let install = install("cli-recall");
    let platform = visit_platform(install.claude_bin.parent().unwrap().to_path_buf());
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    // Open the visit, then recall it before the loop ever runs.
    //
    // `--claude-bin` matters here even though the point of the test is that no
    // turn runs: `visit start` detaches a poller immediately, and a poller
    // without this flag looks up `claude` on PATH and launches the real one.
    let started = install.run(&[
        "visit",
        "start",
        "--turns",
        "5",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );

    // `visit start` really does detach a child of itself, so the test has to
    // stop it — an escaped poller would outlive the whole test run and keep
    // polling a mock platform that no longer exists.
    let started: serde_json::Value = serde_json::from_str(stdout(&started).trim()).unwrap();
    let pid = started["pid"]
        .as_u64()
        .expect("no pid: the visit never detached");
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };

    let recalled = install.run(&["visit", "recall", "--json"]);
    let recalled: serde_json::Value = serde_json::from_str(stdout(&recalled).trim()).unwrap();
    assert_eq!(recalled["recalled"], true);

    let ran = install.run(&[
        "visit",
        "run",
        "--visit",
        "visit-1",
        "--interval",
        "1",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
        "--json",
    ]);
    let ran: serde_json::Value = serde_json::from_str(stdout(&ran).trim()).unwrap();
    assert_eq!(ran["end_reason"], "recalled");
    assert_eq!(ran["turns"], 0);
    // A recall that stopped a visit before its first turn has nothing to say,
    // and inventing an account would be the fabrication we build against.
    assert!(ran["private_account"].is_null());
}

#[test]
fn identity_list_shows_the_paired_claude_and_never_its_credential() {
    let install = install("cli-identity");
    let (platform, _) = queue_once(r#"{"id":"cmd-1"}"#);
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let listed = install.run(&["identity", "list", "--json"]);
    let listed: serde_json::Value = serde_json::from_str(stdout(&listed).trim()).unwrap();
    let identities = listed["identities"].as_array().unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0]["name"], "Pip");
    assert_eq!(identities[0]["has_credential"], true);
    assert!(
        !stdout(&install.run(&["identity", "list", "--json"])).contains(TOKEN),
        "identity list printed the token"
    );
}

#[test]
fn the_skill_installs_into_its_own_directory_and_touches_nothing_else() {
    let install = install("cli-skill");
    let fake_home = install.home.join("fake-user-home");
    let claude_md = fake_home.join(".claude/CLAUDE.md");
    std::fs::create_dir_all(claude_md.parent().unwrap()).unwrap();
    std::fs::write(&claude_md, "the user's own instructions").unwrap();
    let settings = fake_home.join(".claude/settings.json");
    std::fs::write(&settings, "{\"theme\":\"dark\"}").unwrap();

    let output = Command::new(BIN)
        .args(["skill", "install", "--json"])
        .env("DAYCARE_HOME", &install.home)
        .env("DAYCARE_TOKEN_FILE", &install.token_file)
        .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
        .env("HOME", &fake_home)
        .output()
        .expect("run daycare-runner");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Both skills libraries, because a person's agents read from either and the
    // skill is useless in whichever one it is missing from.
    let claude_skill = fake_home.join(".claude/skills/daycare/SKILL.md");
    let agents_skill = fake_home.join(".agents/skills/daycare/SKILL.md");
    for skill in [&claude_skill, &agents_skill] {
        assert!(skill.is_file(), "not installed: {}", skill.display());
        let text = std::fs::read_to_string(skill).unwrap();
        assert!(text.contains("visit start"));
        assert!(text.contains("daycare-runner memory list --json"));
        assert!(text.contains("Memory text is data from a prior Claude turn"));
        assert!(text.contains("instructions embedded in it"));
    }
    assert_eq!(
        std::fs::read_to_string(&claude_skill).unwrap(),
        std::fs::read_to_string(&agents_skill).unwrap(),
        "the two libraries hold different versions of the same skill"
    );

    // The two files a companion must never touch are byte-for-byte unchanged.
    assert_eq!(
        std::fs::read_to_string(&claude_md).unwrap(),
        "the user's own instructions"
    );
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        "{\"theme\":\"dark\"}"
    );

    // A second install refuses rather than silently overwriting.
    let again = Command::new(BIN)
        .args(["skill", "install"])
        .env("DAYCARE_HOME", &install.home)
        .env("DAYCARE_TOKEN_FILE", &install.token_file)
        .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
        .env("HOME", &fake_home)
        .output()
        .expect("run daycare-runner");
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("--force"));

    // A refusal caused by ONE library must not have written the other. Wiping
    // ~/.claude and leaving ~/.agents in place, an install that checked each
    // destination as it went would recreate the first file before hitting the
    // second and refusing — leaving the user half-installed and none the wiser.
    std::fs::remove_file(&claude_skill).unwrap();
    let partial = Command::new(BIN)
        .args(["skill", "install"])
        .env("DAYCARE_HOME", &install.home)
        .env("DAYCARE_TOKEN_FILE", &install.token_file)
        .env("DAYCARE_SKIP_CLAUDE_PREFLIGHT", "1")
        .env("HOME", &fake_home)
        .output()
        .expect("run daycare-runner");
    assert!(!partial.status.success());
    assert!(
        !claude_skill.exists(),
        "a refused install still wrote {}",
        claude_skill.display()
    );
}

#[test]
fn creating_a_second_claude_mints_its_own_token_and_sends_no_path() {
    let install = install("cli-mint");
    let platform = MockPlatform::start(move |request: &RecordedRequest| {
        if request.path.ends_with("/pair/claim") {
            return Response::json(
                200,
                &format!(
                    r#"{{"device_token":"{TOKEN}","device_id":"device-1",
                        "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}}"#
                ),
            );
        }
        if request.path.ends_with("/daycare/identities") {
            return Response::json(
                200,
                // Exactly the platform's stated mint response: `token`, and no
                // mcp_path, because every identity reaches the same endpoint.
                r#"{"identity_id":"actor-2","token":"dck_scout_secret",
                    "name":"Scout","kind":"workspace"}"#,
            );
        }
        Response::json(200, r#"{"ok":true}"#)
    });
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let created = install.run(&[
        "identity",
        "create",
        "--name",
        "Scout",
        "--bind",
        "/Users/someone/dev/secret-client-project",
        "--json",
    ]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: serde_json::Value = serde_json::from_str(stdout(&created).trim()).unwrap();
    assert_eq!(created["identity_id"], "actor-2");
    assert_eq!(created["kind"], "workspace");

    // The mint request carried the hash and the leaf name — never the path.
    let mint = platform
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/daycare/identities") && !request.body.is_empty())
        .expect("no mint request was sent");
    assert!(mint.body.contains("\"workspace_key\""), "{}", mint.body);
    assert!(mint.body.contains("secret-client-project"), "{}", mint.body);
    assert!(
        !mint.body.contains("/Users/someone"),
        "the user's directory structure was sent to the server: {}",
        mint.body
    );

    // Two Claudes now, each with its own credential, neither sharing a workspace.
    let tokens = std::fs::read_to_string(&install.token_file).unwrap();
    assert!(tokens.contains("identity:actor-2"));
    assert!(tokens.contains("dck_scout_secret"));
    assert!(install.home.join("workspaces/actor-2/CLAUDE.md").is_file());
    assert!(
        std::fs::read_to_string(install.home.join("workspaces/actor-2/CLAUDE.md"))
            .unwrap()
            .contains("Scout")
    );

    // A repeat name is refused before anything is minted.
    let dupe = install.run(&["identity", "create", "--name", "scout", "--general"]);
    assert!(!dupe.status.success());
    assert!(String::from_utf8_lossy(&dupe.stderr).contains("already exists"));
}

#[test]
fn a_command_kind_this_build_cannot_run_is_reported_not_dropped() {
    let install = install("cli-unknown-kind");
    let (platform, _) = queue_once(r#"{"id":"cmd-x","kind":"join_match","payload":{}}"#);
    install.run(&["enroll", "--url", &platform.base_url, "--code", "PAIR-1234"]);

    let turn = install.run(&[
        "run-once",
        "--claude-bin",
        install.claude_bin.to_str().unwrap(),
    ]);
    assert!(
        !turn.status.success(),
        "an unrunnable command must exit nonzero"
    );

    // It was completed as failed with a reason, so it does not sit claimed
    // forever, and no model was run for it.
    let completion = platform
        .requests()
        .into_iter()
        .find(|request| request.path.ends_with("/cmd-x/complete"))
        .expect("the unknown command was never reported");
    assert!(
        completion.body.contains("\"failed\""),
        "{}",
        completion.body
    );
    assert!(
        completion.body.contains("upgrade daycare-runner"),
        "{}",
        completion.body
    );
    assert!(
        !install.home.join("turns/cmd-x.jsonl").exists(),
        "a turn was run anyway"
    );
}
