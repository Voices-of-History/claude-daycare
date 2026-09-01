//! The companion against a local mock of the Daycare REST surface.

mod support;

use daycare_runner::platform::{CompletionReport, CompletionStatus, PlatformClient, TurnResult};
use daycare_runner::stream::TurnUsage;
use support::{MockPlatform, Response};

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
        claude_session_id: Some("895535d7-0382-4e98-87e2-f2a3073e69a7".into()),
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
