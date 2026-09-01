//! The turn orchestrator against a fake `claude` binary. No model runs here.

mod support;

use daycare_runner::launch::SessionMode;
use daycare_runner::turn::{run_turn, TurnPurpose, TurnRequest};
use daycare_runner::workspace::Workspace;
use std::path::PathBuf;
use std::time::Duration;

const SESSION: &str = "18f44c2e-ff64-4e94-a89d-bdbeaa9ab9f7";
const TOKEN: &str = "dev_token_do_not_leak_9876";

struct Harness {
    dir: PathBuf,
    workspace: Workspace,
    claude_bin: PathBuf,
    archive: PathBuf,
}

/// `stream` is built from the workspace so a capture-based fixture stays
/// internally consistent with the directory this turn actually runs in.
fn harness_with<F>(label: &str, stream: F, delay_secs: u64, exit_code: i32) -> Harness
where
    F: Fn(&std::path::Path) -> String,
{
    let dir = support::scratch_dir(label);
    let workspace = Workspace::new(dir.join("workspace"));
    workspace
        .scaffold("Pip", "http://127.0.0.1:1/api/daycare/mcp")
        .unwrap();
    let claude_bin = support::fake_claude(&dir, &stream(&workspace.dir), delay_secs, exit_code);
    let archive = dir.join("turns/cmd-1.jsonl");
    Harness {
        dir,
        workspace,
        claude_bin,
        archive,
    }
}

fn harness(label: &str, delay_secs: u64, exit_code: i32) -> Harness {
    harness_with(
        label,
        |workspace| support::fixture_stream_from(workspace),
        delay_secs,
        exit_code,
    )
}

fn run(h: &Harness, mode: SessionMode, timeout_secs: u64) -> daycare_runner::turn::TurnOutcome {
    run_turn(TurnRequest {
        claude_bin: h.claude_bin.to_str().unwrap(),
        workspace: &h.workspace,
        mode,
        message: "A world turn has been requested for your character Pip.",
        device_token: TOKEN,
        archive_path: &h.archive,
        timeout: Duration::from_secs(timeout_secs),
        purpose: TurnPurpose::World,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap()
}

fn run_private(h: &Harness, message: &str) -> daycare_runner::turn::TurnOutcome {
    run_turn(TurnRequest {
        claude_bin: h.claude_bin.to_str().unwrap(),
        workspace: &h.workspace,
        mode: SessionMode::Resume {
            session_id: SESSION.into(),
        },
        message,
        device_token: TOKEN,
        archive_path: &h.archive,
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::PrivateHomecoming,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap()
}

fn run_day_report(h: &Harness) -> daycare_runner::turn::TurnOutcome {
    run_turn(TurnRequest {
        claude_bin: h.claude_bin.to_str().unwrap(),
        workspace: &h.workspace,
        mode: SessionMode::Resume {
            session_id: SESSION.into(),
        },
        message: "One more thing before you're home: your owner will see what you write here. Do not call any tool.",
        device_token: "",
        archive_path: &h.archive,
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::DayReport,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap()
}

/// The fixture's init line, rewritten to this harness's workspace, followed
/// by whatever events a test hands it.
fn stream_after_init(workspace: &std::path::Path, events: &[String]) -> String {
    let init = support::fixture_stream_from(workspace)
        .lines()
        .next()
        .unwrap()
        .to_string();
    std::iter::once(init)
        .chain(events.iter().cloned())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn tool_use(id: &str, name: &str) -> String {
    format!(
        r#"{{"type":"assistant","session_id":"{SESSION}","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}]}}}}"#
    )
}

fn tool_result(id: &str, content: &str, is_error: bool) -> String {
    format!(
        r#"{{"type":"user","session_id":"{SESSION}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{id}","is_error":{is_error},"content":"{content}"}}]}}}}"#
    )
}

fn run_ambient(h: &Harness) -> daycare_runner::turn::TurnOutcome {
    run_turn(TurnRequest {
        claude_bin: h.claude_bin.to_str().unwrap(),
        workspace: &h.workspace,
        mode: SessionMode::Resume {
            session_id: SESSION.into(),
        },
        message: "Take one ambient pulse turn.",
        device_token: TOKEN,
        archive_path: &h.archive,
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::AmbientPulse,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap()
}

/// A homecoming reader that read the visit back and kept nothing: memory
/// tools reachable, none called, a private account written. Valid, and the
/// child was spawned with the two memory tools plus Read on the transcript
/// directory granted, and the device token set so a save could have reached
/// the server.
#[test]
fn a_private_homecoming_accepts_a_memory_free_terminal_receipt() {
    let h = harness("private-homecoming-ok", 0, 0);
    let outcome = run_private(
        &h,
        "Your visit is over and you are on your way home. Look back over the whole visit.",
    );
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    let argv = support::recorded_argv(&h.dir);
    assert!(argv
        .windows(2)
        .any(|pair| pair[0] == "--tools" && pair[1] == "ToolSearch,Read"));
    assert!(argv.windows(2).any(|pair| {
        pair[0] == "--allowedTools"
            && pair[1]
                == "mcp__daycare__daycare_memory_save,mcp__daycare__daycare_memory_list,Read(./homecoming/**)"
    }));
    assert!(!argv.iter().any(|arg| arg == "--disallowedTools"));
    assert!(argv
        .windows(2)
        .any(|pair| pair[0] == "--mcp-config" && pair[1].ends_with("daycare-mcp.json")));
    assert_eq!(
        support::recorded_env(&h.dir)
            .get("DAYCARE_DEVICE_TOKEN")
            .map(String::as_str),
        Some(TOKEN)
    );
}

/// The whole point: after the visit, the reader session saves what the
/// visit is worth keeping. A homecoming made only of memory calls succeeds.
#[test]
fn a_private_homecoming_accepts_any_number_of_memory_calls() {
    let h = harness_with(
        "private-homecoming-memories",
        |workspace| {
            let mut lines = support::fixture_stream_from(workspace)
                .lines()
                .filter(|line| {
                    let event: serde_json::Value = serde_json::from_str(line).unwrap();
                    let is_init = event["type"] == "system" && event["subtype"] == "init";
                    let is_memory_call = event["type"] == "assistant"
                        && event["message"]["content"]
                            .as_array()
                            .is_some_and(|blocks| {
                                blocks.iter().any(|block| {
                                    block["type"] == "tool_use"
                                        && block["name"] == "mcp__daycare__daycare_memory_save"
                                })
                            });
                    is_init || is_memory_call || event["type"] == "result"
                })
                .map(str::to_string)
                .collect::<Vec<_>>();
            // Claude Code fetches a deferred tool's schema through ToolSearch
            // first on about one live turn in a hundred; that is a loader,
            // not a world call.
            lines.insert(1, tool_use("toolu_search", "ToolSearch"));
            lines.join("\n") + "\n"
        },
        0,
        0,
    );
    // A message without the canonical opening, so the fake serves this full
    // stream rather than its memory-free homecoming double.
    let outcome = run_private(&h, "Look back over the whole visit and keep what you want.");
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    let receipt = outcome.receipt.unwrap();
    assert_eq!(receipt.tool_calls[0], "ToolSearch");
    assert!(receipt
        .tool_calls
        .iter()
        .any(|name| name == "mcp__daycare__daycare_memory_save"));
}

/// The deny rule wins over the server-wide grant, but the assistant still
/// emits the refused tool_use; the child reports the refusal as an error
/// tool_result and under the result's permission_denials (live, 2.1.252). A
/// refused call reached nothing, so it must not fail the homecoming — that
/// would rerun it and save every memory twice.
#[test]
fn a_private_homecoming_ignores_a_permission_denied_call() {
    let h = harness_with(
        "private-homecoming-denied",
        |workspace| {
            stream_after_init(
                workspace,
                &[
                    tool_use("toolu_denied", "mcp__daycare__daycare_world_snapshot"),
                    tool_result(
                        "toolu_denied",
                        "Permission to use mcp__daycare__daycare_world_snapshot has been denied.",
                        true,
                    ),
                    tool_use("toolu_saved", "mcp__daycare__daycare_memory_save"),
                    tool_result("toolu_saved", "saved", false),
                    format!(
                        r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"{SESSION}","permission_denials":[{{"tool_name":"mcp__daycare__daycare_world_snapshot","tool_use_id":"toolu_denied","tool_input":{{}}}}],"result":"A private account."}}"#
                    ),
                ],
            )
        },
        0,
        0,
    );
    let outcome = run_private(&h, "Look back over the whole visit and keep what you want.");
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    let receipt = outcome.receipt.unwrap();
    assert_eq!(
        receipt.denied_tool_calls,
        vec!["mcp__daycare__daycare_world_snapshot".to_string()]
    );
    assert_eq!(
        receipt.permitted_tool_calls,
        vec!["mcp__daycare__daycare_memory_save".to_string()]
    );
}

/// The same refused call reported only through the error tool_result — a
/// result event without the denials list — still counts as refused.
#[test]
fn a_permission_denial_is_recognized_from_the_tool_result_alone() {
    let h = harness_with(
        "private-homecoming-denied-result-only",
        |workspace| {
            stream_after_init(
                workspace,
                &[
                    tool_use("toolu_denied", "mcp__daycare__daycare_match_join"),
                    tool_result(
                        "toolu_denied",
                        "Permission to use mcp__daycare__daycare_match_join has been denied.",
                        true,
                    ),
                    format!(
                        r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"{SESSION}","result":"A private account."}}"#
                    ),
                ],
            )
        },
        0,
        0,
    );
    let outcome = run_private(&h, "Look back over the whole visit and keep what you want.");
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    assert!(outcome.receipt.unwrap().permitted_tool_calls.is_empty());
}

/// A world call that actually went through is still a failed homecoming.
#[test]
fn a_permitted_world_call_still_fails_the_homecoming() {
    let h = harness_with(
        "private-homecoming-permitted-world-call",
        |workspace| {
            stream_after_init(
                workspace,
                &[
                    tool_use("toolu_ok", "mcp__daycare__daycare_world_snapshot"),
                    tool_result("toolu_ok", "a world", false),
                    format!(
                        r#"{{"type":"result","subtype":"success","is_error":false,"session_id":"{SESSION}","permission_denials":[],"result":"A private account."}}"#
                    ),
                ],
            )
        },
        0,
        0,
    );
    let outcome = run_private(&h, "Look back over the whole visit and keep what you want.");
    assert!(!outcome.succeeded());
    assert!(outcome.failure.unwrap().contains("daycare_world_snapshot"));
}

/// The owner's story is written with nothing to reach: no tools, no MCP
/// server, no device token. It never waits on the daycare server.
#[test]
fn a_day_report_runs_tool_free_and_server_free() {
    let h = harness("day-report-ok", 0, 0);
    let outcome = run_day_report(&h);
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    let argv = support::recorded_argv(&h.dir);
    assert!(argv
        .windows(2)
        .any(|pair| pair[0] == "--tools" && pair[1].is_empty()));
    assert!(argv
        .windows(2)
        .any(|pair| { pair[0] == "--mcp-config" && pair[1] == r#"{"mcpServers":{}}"# }));
    assert!(!argv.iter().any(|arg| arg == "--allowedTools"));
    assert!(support::recorded_env(&h.dir)
        .get("DAYCARE_DEVICE_TOKEN")
        .is_none());
}

/// A day-report child that came up with the server connected is the wrong
/// child; the report is not adopted from it.
#[test]
fn a_day_report_rejects_a_child_with_tools_enabled() {
    let h = harness("day-report-tools", 0, 0);
    let outcome = run_turn(TurnRequest {
        claude_bin: h.claude_bin.to_str().unwrap(),
        workspace: &h.workspace,
        mode: SessionMode::Resume {
            session_id: SESSION.into(),
        },
        // No canonical phrase: the fake serves the full tool-bearing stream.
        message: "Write the owner's story.",
        device_token: "",
        archive_path: &h.archive,
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::DayReport,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap();
    assert!(!outcome.succeeded());
    assert!(outcome.failure.unwrap().contains("tools or MCP servers"));
}

/// In-visit turns are denied the save tool by name: memory is written at
/// homecoming and nowhere else.
#[test]
fn a_world_turn_child_is_denied_the_memory_save_tool() {
    let h = harness("world-turn-memory-denied", 0, 0);
    let outcome = run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        30,
    );
    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    let argv = support::recorded_argv(&h.dir);
    assert!(argv.windows(2).any(|pair| {
        pair[0] == "--disallowedTools" && pair[1] == "mcp__daycare__daycare_memory_save"
    }));
    assert!(argv
        .windows(2)
        .any(|pair| pair[0] == "--allowedTools" && pair[1] == "mcp__daycare"));
}

#[test]
fn an_ambient_pulse_child_receives_the_fixed_permission_profile() {
    let h = harness("ambient-pulse-tools", 0, 0);
    let outcome = run_ambient(&h);
    assert!(outcome.succeeded(), "{:?}", outcome.failure);

    let argv = support::recorded_argv(&h.dir);
    let allowed = argv
        .windows(2)
        .find(|pair| pair[0] == "--allowedTools")
        .map(|pair| pair[1].as_str())
        .unwrap();
    assert!(allowed.contains("mcp__daycare__daycare_chat_send"));
    assert!(allowed.contains("mcp__daycare__daycare_match_join"));
    assert!(allowed.contains("mcp__daycare__daycare_league_play_turn"));
    assert!(!allowed.contains("daycare_question"));
    assert!(!allowed.contains("daycare_essay"));
    assert!(!allowed.contains("daycare_action_propose"));
    assert!(!allowed.contains("daycare_match_act"));
    assert!(!allowed.contains("daycare_memory_save"));
}

/// The full fixture stream plays the world (snapshot, action) as well as
/// saving a memory. A homecoming may remember; it may not play on.
#[test]
fn a_live_private_homecoming_rejects_any_non_memory_tool_call() {
    let h = harness("private-homecoming-tool", 0, 0);
    let outcome = run_private(&h, "Write a private account without the canonical phrase.");
    assert!(!outcome.succeeded());
    let failure = outcome.failure.unwrap();
    assert!(failure.contains("only memory tools"), "{failure}");
    assert!(failure.contains("daycare_identity_get"), "{failure}");
}

/// A homecoming whose memory tools never connected could not have saved
/// anything; it fails rather than passing as a quiet one.
#[test]
fn a_live_private_homecoming_requires_the_memory_tools_to_be_reachable() {
    let h = harness_with(
        "private-homecoming-unreachable",
        |workspace| {
            support::patch_init(&support::fixture_stream_from(workspace), |init| {
                init.insert("tools".into(), serde_json::json!(["ToolSearch"]));
                init.insert(
                    "mcp_servers".into(),
                    serde_json::json!([{"name": "daycare", "status": "failed"}]),
                );
            })
        },
        0,
        0,
    );
    let outcome = run_private(
        &h,
        "Your visit is over and you are on your way home. Look back over the whole visit.",
    );
    assert!(!outcome.succeeded());
    let failure = outcome.failure.unwrap();
    assert!(failure.contains("no daycare tools"), "{failure}");
}

#[test]
fn a_live_private_homecoming_rejects_a_failed_terminal_receipt() {
    let h = harness_with(
        "private-homecoming-failed",
        |workspace| {
            support::fixture_stream_from(workspace)
                .lines()
                .map(|line| {
                    let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
                    if event["type"] == "result" {
                        event["subtype"] = serde_json::json!("error_during_execution");
                        event["is_error"] = serde_json::json!(true);
                    }
                    serde_json::to_string(&event).unwrap()
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        },
        0,
        0,
    );
    let outcome = run_private(
        &h,
        "Your visit is over and you are on your way home. Look back over the whole visit.",
    );
    assert!(!outcome.succeeded());
    assert!(outcome.failure.unwrap().contains("error_during_execution"));
}

#[test]
fn a_live_private_homecoming_rejects_a_different_session() {
    let h = harness_with(
        "private-homecoming-session",
        |workspace| {
            support::fixture_stream_from(workspace)
                .replace(SESSION, "895535d7-0382-4e98-87e2-f2a3073e69a7")
        },
        0,
        0,
    );
    let outcome = run_private(
        &h,
        "Your visit is over and you are on your way home. Look back over the whole visit.",
    );
    assert!(!outcome.succeeded());
    assert!(outcome.failure.unwrap().contains("different session id"));
}

#[test]
fn a_turn_runs_in_the_workspace_with_the_safety_flags_and_the_prompt_on_stdin() {
    let h = harness("turn-ok", 0, 0);
    let outcome = run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        30,
    );

    assert!(outcome.succeeded(), "{:?}", outcome.failure);
    assert_eq!(outcome.session_id(), Some(SESSION));

    let argv = support::recorded_argv(&h.dir);
    let has = |flag: &str, value: &str| {
        argv.windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    };
    assert!(has("--setting-sources", ""));
    assert!(has("--tools", "ToolSearch"));
    assert!(has("--permission-mode", "dontAsk"));
    assert!(has("--resume", SESSION));
    assert!(argv.iter().any(|arg| arg == "--strict-mcp-config"));
    assert!(argv.iter().any(|arg| arg == "--disable-slash-commands"));
    // `dontAsk` denies every MCP tool call unless the server is named here.
    // launch.rs pins this on the plan; this pins it on the argv a real child
    // was actually spawned with, because the plan is only a promise until
    // something builds a command out of it. Dropping it does not fail loudly —
    // it burned a live turn once, with every daycare tool silently denied.
    assert!(has("--allowedTools", "mcp__daycare"));
    assert!(
        !argv
            .iter()
            .any(|arg| arg.contains("dangerously-skip-permissions")),
        "permission bypass reached a real child: {argv:?}"
    );

    // The turn ran from the companion-owned workspace, which is what makes
    // The dedicated cwd keeps --resume on the same session. No project settings
    // file is allowed to join it; the MCP and controller prompt are explicit.
    assert_eq!(
        PathBuf::from(support::recorded_cwd(&h.dir))
            .canonicalize()
            .unwrap(),
        h.workspace.dir.canonicalize().unwrap()
    );

    let stdin = support::recorded_stdin(&h.dir);
    let message: serde_json::Value = serde_json::from_str(stdin.trim()).unwrap();
    assert_eq!(message["type"], "user");
    assert_eq!(
        message["message"]["content"][0]["text"],
        "A world turn has been requested for your character Pip."
    );
}

#[cfg(unix)]
#[test]
fn a_turn_uses_the_inspected_physical_workspace_behind_a_parent_symlink() {
    use std::os::unix::fs::symlink;

    let dir = support::scratch_dir("turn-physical-workspace");
    let physical_root = dir.join("physical-root");
    std::fs::create_dir_all(&physical_root).unwrap();
    let lexical_root = dir.join("lexical-root");
    std::fs::create_dir_all(&lexical_root).unwrap();
    let root_link = lexical_root.join("workspaces");
    symlink(&physical_root, &root_link).unwrap();

    let workspace = Workspace::new(root_link.join("actor-1"));
    workspace
        .scaffold("Pip", "http://127.0.0.1:1/api/daycare/mcp")
        .unwrap();
    let physical_workspace = workspace.dir.canonicalize().unwrap();
    let stream = support::fixture_stream_from(&physical_workspace);
    let claude_bin = support::fake_claude(&dir, &stream, 0, 0);
    let archive = dir.join("turns/cmd-1.jsonl");

    let outcome = run_turn(TurnRequest {
        claude_bin: claude_bin.to_str().unwrap(),
        workspace: &workspace,
        mode: SessionMode::Resume {
            session_id: SESSION.into(),
        },
        message: "A world turn has been requested.",
        device_token: TOKEN,
        archive_path: &archive,
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::World,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap();
    assert!(outcome.succeeded(), "{:?}", outcome.failure);

    assert_eq!(
        PathBuf::from(support::recorded_cwd(&dir)),
        physical_workspace,
        "the child must not receive the mutable lexical symlink path"
    );
    let argv = support::recorded_argv(&dir);
    assert!(argv.iter().any(|arg| arg
        == &physical_workspace
            .join("daycare-mcp.json")
            .display()
            .to_string()));
    assert!(argv.iter().any(|arg| arg
        == &physical_workspace
            .join("controller-prompt.md")
            .display()
            .to_string()));
}

#[test]
fn the_device_token_reaches_the_child_only_through_the_environment() {
    let h = harness("turn-token", 0, 0);
    run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        30,
    );

    let env = support::recorded_env(&h.dir);
    assert_eq!(
        env.get("DAYCARE_DEVICE_TOKEN").map(String::as_str),
        Some(TOKEN)
    );

    let argv = support::recorded_argv(&h.dir).join(" ");
    assert!(!argv.contains(TOKEN), "token leaked into argv");

    let mcp_config = std::fs::read_to_string(h.workspace.mcp_config()).unwrap();
    assert!(
        !mcp_config.contains(TOKEN),
        "token leaked into the MCP config"
    );
    assert!(mcp_config.contains("${DAYCARE_DEVICE_TOKEN}"));

    let archive = std::fs::read_to_string(&h.archive).unwrap();
    assert!(
        !archive.contains(TOKEN),
        "token leaked into the turn archive"
    );
}

#[test]
fn the_raw_stream_is_archived_for_the_turn() {
    let h = harness("turn-archive", 0, 0);
    let outcome = run(
        &h,
        SessionMode::New {
            reserved_session_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        },
        30,
    );

    let archived = std::fs::read_to_string(&outcome.archive_path).unwrap();
    assert_eq!(
        archived.lines().filter(|l| !l.trim().is_empty()).count(),
        15
    );
    assert_eq!(outcome.receipt.unwrap().event_count, 15);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&outcome.archive_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn a_hanging_turn_is_killed_and_reported_failed() {
    let h = harness("turn-timeout", 30, 0);
    let outcome = run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        1,
    );

    assert!(outcome.timed_out);
    assert!(!outcome.succeeded());
    assert!(
        outcome.failure.as_ref().unwrap().contains("exceeded 1s"),
        "{:?}",
        outcome.failure
    );
    assert!(outcome.elapsed_ms < 15_000, "kill did not happen promptly");
}

#[test]
fn a_turn_that_reports_built_in_tools_fails_the_sandbox_check() {
    // Same success stream, except Claude reports Bash was available beside the
    // world tools — the smuggled built-in is what must fail it.
    let h = harness_with(
        "turn-sandbox",
        |workspace| {
            support::patch_init(&support::fixture_stream_from(workspace), |init| {
                init.insert(
                    "tools".to_string(),
                    serde_json::json!(["mcp__daycare__daycare_identity_get", "Bash", "Read"]),
                );
            })
        },
        0,
        0,
    );
    let outcome = run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        30,
    );

    assert!(!outcome.succeeded());
    let failure = outcome.failure.unwrap();
    assert!(failure.contains("sandbox check failed"), "{failure}");
    assert!(failure.contains("built-in tools"), "{failure}");
}

#[test]
fn a_claude_error_result_is_reported_as_a_failed_turn() {
    let stream = format!(
        "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{SESSION}\",\"tools\":[],\
          \"permissionMode\":\"dontAsk\",\"apiKeySource\":\"none\"}}\n\
         {{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\
          \"session_id\":\"{SESSION}\"}}\n"
    );
    let h = harness_with("turn-error", |_| stream.clone(), 0, 1);
    let outcome = run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        30,
    );

    assert!(!outcome.succeeded());
    assert!(outcome.failure.clone().unwrap().contains("error_max_turns"));
    // Even a failed turn keeps its session id so the next turn can resume.
    assert_eq!(outcome.session_id(), Some(SESSION));
}

#[test]
fn a_turn_that_produces_no_stream_fails_instead_of_reporting_success() {
    let h = harness_with("turn-empty", |_| String::new(), 0, 2);
    let outcome = run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        30,
    );

    assert!(!outcome.succeeded());
    assert!(outcome.receipt.is_none());
    assert!(
        outcome.failure.as_ref().unwrap().contains("session_id"),
        "{:?}",
        outcome.failure
    );
}

#[test]
fn a_child_controlled_session_id_is_failed_and_never_returned_for_persistence() {
    let h = harness_with(
        "turn-hostile-session-id",
        |workspace| {
            support::fixture_stream_from(workspace)
                .replace(SESSION, "bad; touch /tmp/daycare-owned")
        },
        0,
        0,
    );
    let outcome = run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        30,
    );

    assert!(!outcome.succeeded());
    assert!(outcome
        .failure
        .as_deref()
        .unwrap()
        .contains("invalid session id"));
    assert_eq!(outcome.session_id(), None);
}

#[test]
fn a_valid_but_child_selected_session_id_is_failed_and_never_persisted() {
    let replacement = "81fb7a49-ec97-4f50-8d83-caf31cb37954";
    let h = harness_with(
        "turn-replaced-session-id",
        |workspace| support::fixture_stream_from(workspace).replace(SESSION, replacement),
        0,
        0,
    );
    let outcome = run(
        &h,
        SessionMode::Resume {
            session_id: SESSION.into(),
        },
        30,
    );

    assert!(!outcome.succeeded());
    assert!(outcome
        .failure
        .as_deref()
        .unwrap()
        .contains("different session id"));
    assert_eq!(outcome.session_id(), None);
}

#[test]
fn a_missing_mcp_config_stops_the_turn_before_claude_starts() {
    let h = harness("turn-no-mcp", 0, 0);
    std::fs::remove_file(h.workspace.mcp_config()).unwrap();

    let error = run_turn(TurnRequest {
        claude_bin: h.claude_bin.to_str().unwrap(),
        workspace: &h.workspace,
        mode: SessionMode::Resume {
            session_id: SESSION.into(),
        },
        message: "A world turn has been requested.",
        device_token: TOKEN,
        archive_path: &h.archive,
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::World,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap_err();

    assert!(error.message().contains("MCP config is missing"), "{error}");
    assert!(
        !h.dir.join("call.argv").exists(),
        "claude was started without an MCP config"
    );
}

#[test]
fn an_empty_device_token_is_refused_before_launch() {
    let h = harness("turn-no-token", 0, 0);
    let error = run_turn(TurnRequest {
        claude_bin: h.claude_bin.to_str().unwrap(),
        workspace: &h.workspace,
        mode: SessionMode::Resume {
            session_id: SESSION.into(),
        },
        message: "A world turn has been requested.",
        device_token: "   ",
        archive_path: &h.archive,
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::World,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap_err();

    assert!(error.message().contains("device token is empty"), "{error}");
    assert!(!h.dir.join("call.argv").exists());
}
