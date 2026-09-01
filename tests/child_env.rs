//! Runs in its own test binary because it mutates the process environment.

mod support;

use daycare_runner::launch::SessionMode;
use daycare_runner::turn::{run_turn, TurnPurpose, TurnRequest};
use daycare_runner::workspace::Workspace;
use std::time::Duration;

/// A Daycare turn must run on the user's Claude subscription. If an API key is
/// present in the operator's shell, Claude Code prefers it and the user is
/// billed per token for their character's day out — so the runner strips it.
/// Nesting variables go too: the companion is often started from inside another
/// Claude Code session.
#[test]
fn the_child_never_inherits_api_credentials_or_the_parent_session() {
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-should-not-be-inherited");
    std::env::set_var("ANTHROPIC_AUTH_TOKEN", "should-not-be-inherited");
    std::env::set_var("CLAUDECODE", "1");
    std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");
    std::env::set_var("DAYCARE_DEVICE_TOKEN", "stale-parent-token");
    std::env::set_var("DAYCARE_KEEP_ME", "inherited");

    let dir = support::scratch_dir("child-env");
    let workspace = Workspace::new(dir.join("workspace"));
    workspace
        .scaffold("Pip", "http://127.0.0.1:1/api/daycare/mcp")
        .unwrap();
    let claude_bin =
        support::fake_claude(&dir, &support::fixture_stream_from(&workspace.dir), 0, 0);

    let outcome = run_turn(TurnRequest {
        claude_bin: claude_bin.to_str().unwrap(),
        workspace: &workspace,
        mode: SessionMode::Resume {
            session_id: "18f44c2e-ff64-4e94-a89d-bdbeaa9ab9f7".into(),
        },
        message: "A world turn has been requested for your character Pip.",
        device_token: "dev_token_abc",
        archive_path: &dir.join("turns/cmd-1.jsonl"),
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::World,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap();
    assert!(outcome.succeeded(), "{:?}", outcome.failure);

    let env = support::recorded_env(&dir);
    assert!(
        env.get("ANTHROPIC_API_KEY").is_none(),
        "API key reached the turn"
    );
    assert!(env.get("ANTHROPIC_AUTH_TOKEN").is_none());
    assert!(
        env.get("CLAUDECODE").is_none(),
        "turn ran as a nested session"
    );
    assert!(env.get("CLAUDE_CODE_ENTRYPOINT").is_none());

    // Everything else is inherited, including PATH and the device token.
    assert_eq!(
        env.get("DAYCARE_KEEP_ME").map(String::as_str),
        Some("inherited")
    );
    assert_eq!(
        env.get("DAYCARE_DEVICE_TOKEN").map(String::as_str),
        Some("dev_token_abc")
    );

    // The homecoming saves the visit's memories through the same MCP server,
    // so it carries the device token too — and still nothing API-shaped.
    let private = run_turn(TurnRequest {
        claude_bin: claude_bin.to_str().unwrap(),
        workspace: &workspace,
        mode: SessionMode::Resume {
            session_id: "18f44c2e-ff64-4e94-a89d-bdbeaa9ab9f7".into(),
        },
        message: "Your visit is over and you are on your way home. Look back over the whole visit.",
        device_token: "dev_token_abc",
        archive_path: &dir.join("turns/private.jsonl"),
        timeout: Duration::from_secs(30),
        purpose: TurnPurpose::PrivateHomecoming,
        model: daycare_runner::launch::DEFAULT_TURN_MODEL,
        mcp_settle: Duration::ZERO,
    })
    .unwrap();
    assert!(private.succeeded(), "{:?}", private.failure);
    let private_env = support::recorded_env(&dir);
    assert_eq!(
        private_env.get("DAYCARE_DEVICE_TOKEN").map(String::as_str),
        Some("dev_token_abc")
    );
    assert!(private_env.get("ANTHROPIC_API_KEY").is_none());
    assert!(env.contains_key("PATH"));
}
