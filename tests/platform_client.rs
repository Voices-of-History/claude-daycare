//! The companion against a local mock of the Daycare REST surface.

mod support;

use daycare_runner::agent::AgentKind;
use daycare_runner::platform::{
    AgentSession, CompletionReport, CompletionStatus, PlatformClient, TurnResult,
};
use daycare_runner::stream::TurnUsage;
use daycare_runner::visit::Budget;
use support::{MockPlatform, Response};

#[test]
fn visit_start_sends_the_token_cap_with_its_basis() {
    let platform = MockPlatform::start(|_| Response::json(200, r#"{"visit_id":"visit-1"}"#));
    let client = PlatformClient::new(&platform.base_url);
    client
        .start_visit(
            "test-token",
            &Budget::default().or_default_without_meter(),
            None,
            AgentKind::Codex,
            "gpt-5.5",
        )
        .unwrap();
    let sent = platform.requests()[0].json();
    assert_eq!(sent["agent_kind"], "codex");
    assert_eq!(sent["agent_model"], "gpt-5.5");
    assert_eq!(sent["budget_tokens"], 300_000);
    assert_eq!(sent["budget_basis"], "fixed_fallback");
    assert!(sent.get("budget_usage_pct").is_none());
}

#[test]
fn a_weekly_visit_names_its_agent_without_inventing_a_token_cap() {
    let platform = MockPlatform::start(|_| Response::json(200, r#"{"visit_id":"visit-1"}"#));
    let client = PlatformClient::new(&platform.base_url);
    for (agent, model) in [(AgentKind::Claude, "sonnet"), (AgentKind::Codex, "gpt-5.5")] {
        client
            .start_visit(
                "test-token",
                &Budget::default().or_default(),
                Some("Say hello."),
                agent,
                model,
            )
            .unwrap();
        let requests = platform.requests();
        let sent = requests.last().unwrap().json();
        assert_eq!(sent["agent_kind"], agent.as_str());
        assert_eq!(sent["agent_model"], model);
        assert_eq!(sent["budget_usage_pct"], 2.0);
        assert_eq!(sent["instructions"], "Say hello.");
        assert!(sent.get("budget_tokens").is_none());
        assert!(sent.get("budget_basis").is_none());
    }
    let budget = Budget {
        tokens: Some(12345),
        ..Budget::default().or_default()
    };
    client
        .start_visit("test-token", &budget, None, AgentKind::Codex, "gpt-5.5")
        .unwrap();
    let sent = platform.requests().last().unwrap().json();
    assert_eq!(sent["budget_usage_pct"], 2.0);
    assert_eq!(sent["budget_tokens"], 12345);
    assert_eq!(sent["budget_basis"], "fixed_fallback");
}

#[test]
fn codex_completion_posts_an_agent_session_not_a_claude_session() {
    let platform = MockPlatform::start(|_| Response::json(200, r#"{"ok":true}"#));
    let report = CompletionReport {
        status: CompletionStatus::Completed,
        session: Some(AgentSession::new(
            AgentKind::Codex,
            "01900000-0000-7000-8000-000000000001",
        )),
        result: TurnResult {
            result_text: Some("Hello.".into()),
            duration_ms: None,
            usage: None,
            error: None,
            held: false,
        },
    };
    PlatformClient::new(&platform.base_url)
        .complete_command("test-token", "cmd-codex", &report)
        .unwrap();
    let sent = &platform.requests()[0];
    assert_eq!(sent.path, "/api/daycare/commands/cmd-codex/complete");
    assert_eq!(sent.authorization(), Some("Bearer test-token"));
    let body = sent.json();
    assert_eq!(body["agent_kind"], "codex");
    assert_eq!(
        body["agent_session_id"],
        "01900000-0000-7000-8000-000000000001"
    );
    assert!(body.get("claude_session_id").is_none());
}

#[test]
fn claim_sends_the_code_and_returns_the_pairing() {
    let platform = MockPlatform::start(|request| {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/daycare/pair/claim");
        Response::json(
            200,
            r#"{"device_token":"dev_token_abc","device_id":"device-1",
                "actor_id":"actor-1","actor_name":"Pip","mcp_path":"/api/daycare/mcp"}"#,
        )
    });

    let client = PlatformClient::new(&platform.base_url);
    let claim = client.claim_pairing("PAIR-1234", Some("josh-mbp")).unwrap();

    assert_eq!(claim.device_token, "dev_token_abc");
    assert_eq!(claim.actor_name, "Pip");
    assert_eq!(claim.mcp_path, "/api/daycare/mcp");

    let sent = platform.requests()[0].json();
    assert_eq!(sent["code"], "PAIR-1234");
    assert_eq!(sent["device_name"], "josh-mbp");
}

#[test]
fn an_empty_queue_is_no_work_not_an_error() {
    let platform = MockPlatform::start(|_| Response::no_content());
    let client = PlatformClient::new(&platform.base_url);
    assert!(client.next_command("dev_token_abc").unwrap().is_none());

    let sent = &platform.requests()[0];
    assert_eq!(sent.method, "GET");
    assert_eq!(sent.path, "/api/daycare/commands/next");
    assert_eq!(sent.authorization(), Some("Bearer dev_token_abc"));
}

#[test]
fn a_queued_command_is_returned_bare_or_wrapped() {
    let bare = MockPlatform::start(|_| {
        Response::json(
            200,
            r#"{"id":"cmd-1","kind":"world_turn","actor_id":"actor-1"}"#,
        )
    });
    let command = PlatformClient::new(&bare.base_url)
        .next_command("t")
        .unwrap()
        .unwrap();
    assert_eq!(command.id, "cmd-1");
    assert_eq!(command.kind.as_deref(), Some("world_turn"));

    let wrapped = MockPlatform::start(|_| {
        Response::json(
            200,
            r#"{"command":{"id":"cmd-2","prompt":"Take your turn."}}"#,
        )
    });
    let command = PlatformClient::new(&wrapped.base_url)
        .next_command("t")
        .unwrap()
        .unwrap();
    assert_eq!(command.id, "cmd-2");
    assert_eq!(command.prompt.as_deref(), Some("Take your turn."));

    let empty = MockPlatform::start(|_| Response::json(200, r#"{"command":null}"#));
    assert!(PlatformClient::new(&empty.base_url)
        .next_command("t")
        .unwrap()
        .is_none());
}

#[test]
fn completion_posts_the_receipt_to_the_command_path() {
    let platform = MockPlatform::start(|_| Response::json(200, r#"{"ok":true}"#));
    let client = PlatformClient::new(&platform.base_url);

    let report = CompletionReport {
        status: CompletionStatus::Completed,
        session: Some(AgentSession::new(
            AgentKind::Claude,
            "895535d7-0382-4e98-87e2-f2a3073e69a7",
        )),
        result: TurnResult {
            result_text: Some("Greeted Mira by the fountain.".into()),
            duration_ms: Some(2493),
            usage: Some(TurnUsage {
                input_tokens: Some(2),
                output_tokens: Some(78),
                rate_limit_type: Some("five_hour".into()),
                ..TurnUsage::default()
            }),
            error: None,
            held: false,
        },
    };
    client
        .complete_command("dev_token_abc", "cmd-1", &report)
        .unwrap();

    let sent = &platform.requests()[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/api/daycare/commands/cmd-1/complete");
    assert_eq!(sent.authorization(), Some("Bearer dev_token_abc"));

    let body = sent.json();
    assert_eq!(body["status"], "completed");
    assert_eq!(
        body["claude_session_id"],
        "895535d7-0382-4e98-87e2-f2a3073e69a7"
    );
    assert_eq!(body["result"]["duration_ms"], 2493);
    assert_eq!(body["result"]["usage"]["output_tokens"], 78);
    assert_eq!(body["result"]["usage"]["rate_limit_type"], "five_hour");
}

/// A 401 says what happened AND what most likely caused it.
///
/// Verified live on 2026-08-06: when an identity is re-paired to another
/// machine the server rotates its token, and this machine's next poll answers
/// `401 {"error":"Invalid or revoked device token"}` — which names a *device*
/// token even though what died was the identity's, and names nothing the user
/// did. Re-pairing is a thing the user chose, minutes earlier, on another
/// computer; the one message they see about it should connect the two.
#[test]
fn a_rejected_credential_surfaces_the_status_without_echoing_the_token() {
    let platform = MockPlatform::start(|_| Response::json(401, r#"{"error":"unknown device"}"#));
    let error = PlatformClient::new(&platform.base_url)
        .next_command("dev_token_abc")
        .unwrap_err();
    let message = error.message();
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("unknown device"), "{message}");
    assert!(
        !message.contains("dev_token_abc"),
        "token leaked: {message}"
    );
    assert!(
        message.contains("another computer"),
        "a 401 that does not mention re-pairing leaves the user with the \
         server's word 'device token' and no way to connect it to what they \
         actually did: {message}"
    );
    assert!(
        message.contains("Pair again"),
        "the message names the cause but not the fix: {message}"
    );
}

/// A 409 at enroll is not a 404, and the difference is what the user should do.
///
/// 404 means the code was wrong or used — check the code. 409 means the code
/// was perfectly good and the Claude it pointed at has been retired, so
/// re-reading the code accomplishes nothing. Collapsing them sends the user
/// back to a screen that cannot help.
#[test]
fn a_retired_claude_is_reported_as_something_other_than_a_bad_code() {
    let platform = MockPlatform::start(|_| {
        Response::json(
            409,
            r#"{"error":"That Claude has been retired and cannot be re-paired"}"#,
        )
    });
    let error = PlatformClient::new(&platform.base_url)
        .claim_pairing("PAIR-1234", Some("test-mac"))
        .unwrap_err();
    let message = error.message();
    assert!(message.contains("409"), "{message}");
    assert!(
        message.contains("retired"),
        "the server's reason was dropped, leaving the user to re-check a code \
         that was never the problem: {message}"
    );
}
