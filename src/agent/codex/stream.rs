//! Parsing the `codex exec --json` JSONL that a Codex turn produces.
//!
//! Shapes checked against codex-cli 0.154 (`tests/fixtures/codex-exec-*.jsonl`
//! were captured from it): `thread.started {thread_id}`, `turn.started`,
//! `item.started|updated|completed {item}`, `turn.completed {usage}`,
//! `turn.failed {error}`, `error {message}`. An item is `{id, type, ..}` with
//! `type` one of `agent_message {text}`, `reasoning {text}`,
//! `mcp_tool_call {server, tool, arguments, result, error, status}`,
//! `command_execution`, `file_change`, `web_search`, `collab_tool_call`,
//! `todo_list`, `error {message}`.
//!
//! The output is the shared `StreamReceipt`. Codex names an MCP tool by
//! `server` + `tool`; the receipt names it `mcp__daycare__<tool>` so the shared
//! world-reach and homecoming checks hold unchanged. Every item that is not
//! words, reasoning, a warning, or a daycare call lands in `foreign_reach`:
//! This remains a backstop even with collaboration disabled before launch.

use crate::launch::{MCP_SERVER, MCP_TOOL_PREFIX};
use crate::stream::{looks_like_invented_tool_call, StreamReceipt, TurnEvent, TurnUsage};
use crate::visit::WEEKLY_WINDOW;
use crate::{Error, Result};
use serde_json::Value;

pub fn parse(stream: &str) -> Result<StreamReceipt> {
    let mut thread_id: Option<String> = None;
    let mut completed = false;
    let mut failed = false;
    let mut error_message: Option<String> = None;
    let mut result_text: Option<String> = None;
    let mut event_count = 0usize;
    let mut usage = TurnUsage::default();
    let mut tool_calls = Vec::new();
    let mut permitted_tool_calls = Vec::new();
    let mut denied_tool_calls = Vec::new();
    let mut foreign_reach = Vec::new();
    let mut invented_tool_calls = false;

    for (index, line) in stream.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|error| {
            Error::new(format!(
                "invalid codex stream JSON on line {}: {error}",
                index + 1
            ))
        })?;
        event_count += 1;
        match event.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                thread_id = string_at(&event, "thread_id");
            }
            Some("turn.completed") => {
                completed = true;
                if let Some(reported) = event.get("usage") {
                    usage = turn_usage(reported);
                }
            }
            Some("turn.failed") => {
                failed = true;
                if let Some(message) = event
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                {
                    error_message = Some(message.to_string());
                }
            }
            // A top-level `error` can be a retry notice ("Reconnecting…")
            // that the turn survives; whether the turn failed is
            // `turn.failed` or a missing `turn.completed`. The message is
            // kept for the failure text.
            Some("error") => {
                if let Some(message) = event.get("message").and_then(Value::as_str) {
                    error_message = Some(message.to_string());
                }
            }
            Some("item.completed") => {
                let Some(item) = event.get("item") else {
                    continue;
                };
                match item.get("type").and_then(Value::as_str) {
                    Some("agent_message") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                if looks_like_invented_tool_call(text) {
                                    invented_tool_calls = true;
                                }
                                result_text = Some(text.to_string());
                            }
                        }
                    }
                    // Thinking and warnings ("Skill descriptions were
                    // shortened…") reach nothing.
                    Some("reasoning") | Some("error") => {}
                    Some("mcp_tool_call") => {
                        let server = item.get("server").and_then(Value::as_str).unwrap_or("");
                        let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
                        if server != MCP_SERVER || is_resource_helper(tool) {
                            foreign_reach.push(format!("mcp server {server:?} ({tool})"));
                            continue;
                        }
                        let name = format!("{MCP_TOOL_PREFIX}{tool}");
                        tool_calls.push(name.clone());
                        let approval_denied = item
                            .get("error")
                            .and_then(|error| error.get("message"))
                            .and_then(Value::as_str)
                            .is_some_and(|message| {
                                message.contains("requires approval, but approval policy is never")
                            });
                        if item.get("status").and_then(Value::as_str) == Some("declined")
                            || approval_denied
                        {
                            denied_tool_calls.push(name);
                            if approval_denied {
                                failed = true;
                                error_message = Some("Daycare MCP call requires approval; check the Codex server approval configuration".into());
                            }
                        } else {
                            permitted_tool_calls.push(name);
                        }
                    }
                    Some(other) => foreign_reach.push(other.to_string()),
                    None => foreign_reach.push("an item with no type".into()),
                }
            }
            // `item.started` / `item.updated` carry partial items; the
            // completed form is the record. A started item that never
            // completed still had reach, though, so it counts.
            Some("item.started") | Some("item.updated") => {
                if let Some(item) = event.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("mcp_tool_call")
                        && (item.get("server").and_then(Value::as_str) != Some(MCP_SERVER)
                            || item
                                .get("tool")
                                .and_then(Value::as_str)
                                .is_some_and(is_resource_helper))
                    {
                        foreign_reach.push(format!(
                            "mcp server {:?} ({:?})",
                            item.get("server"),
                            item.get("tool")
                        ));
                    }
                }
                if let Some(kind) = event
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                {
                    if !matches!(
                        kind,
                        "agent_message" | "reasoning" | "error" | "mcp_tool_call"
                    ) && !foreign_reach.iter().any(|seen| seen == kind)
                    {
                        foreign_reach.push(kind.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    // Codex reports an exhausted account as a failed turn whose message
    // names the limit. Mark it blocked so the visit loop naps instead of
    // counting a failure against a wall.
    if let Some(message) = &error_message {
        let lower = message.to_ascii_lowercase();
        if lower.contains("usage limit") || lower.contains("rate limit") {
            usage.rate_limit_type = Some(WEEKLY_WINDOW.into());
            usage.rate_limit_status = Some("rejected".into());
        }
    }

    let session_id = thread_id.ok_or_else(|| {
        Error::new("codex stream never reported a thread id (no thread.started event)")
    })?;
    let error_subtype = if failed {
        Some(match &error_message {
            Some(message) => format!("turn_failed: {message}"),
            None => "turn_failed".into(),
        })
    } else if !completed {
        Some("turn_never_completed".into())
    } else {
        None
    };
    let events = transcript_events(stream)?;
    Ok(StreamReceipt {
        session_id,
        success: completed && !failed,
        result_text,
        duration_ms: None,
        num_turns: Some(1),
        stop_reason: None,
        error_subtype,
        event_count,
        usage,
        init: None,
        tool_calls,
        permitted_tool_calls,
        denied_tool_calls,
        invented_tool_calls,
        events,
        foreign_reach,
    })
}

/// Codex reports `input_tokens` inclusive of the cached and cache-written
/// part (the rollout's `total_tokens` is `input + output`); the ledger adds
/// input, output, cache read, and cache write the way Claude reports them, so
/// input is stored without the cached parts. `output_tokens` already includes
/// reasoning, which is kept only as a breakdown.
fn turn_usage(reported: &Value) -> TurnUsage {
    let read = |key: &str| reported.get(key).and_then(Value::as_u64);
    let cached = read("cached_input_tokens");
    let written = read("cache_write_input_tokens");
    TurnUsage {
        input_tokens: read("input_tokens").map(|input| {
            input
                .saturating_sub(cached.unwrap_or(0))
                .saturating_sub(written.unwrap_or(0))
        }),
        output_tokens: read("output_tokens"),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: written,
        reasoning_output_tokens: read("reasoning_output_tokens"),
        ..TurnUsage::default()
    }
}

/// The transcript events of one Codex archive, leniently: an archive with no
/// thread id or no completion still renders what it holds.
pub fn transcript_events(stream: &str) -> Result<Vec<TurnEvent>> {
    let mut events = Vec::new();
    for (index, line) in stream.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|error| {
            Error::new(format!("line {} is not stream JSON: {error}", index + 1))
        })?;
        match event.get("type").and_then(Value::as_str) {
            Some("item.completed") => {
                let Some(item) = event.get("item") else {
                    continue;
                };
                match item.get("type").and_then(Value::as_str) {
                    Some("agent_message") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            if !text.trim().is_empty() {
                                events.push(TurnEvent::Said(text.trim().to_string()));
                            }
                        }
                    }
                    Some("mcp_tool_call") => {
                        let server = item.get("server").and_then(Value::as_str).unwrap_or("");
                        let tool = item.get("tool").and_then(Value::as_str).unwrap_or("");
                        let name = if server == MCP_SERVER {
                            format!("{MCP_TOOL_PREFIX}{tool}")
                        } else {
                            format!("mcp__{server}__{tool}")
                        };
                        let id = string_at(item, "id");
                        events.push(TurnEvent::Called {
                            id: id.clone(),
                            name: name.clone(),
                            input: item
                                .get("arguments")
                                .map(|arguments| arguments.to_string())
                                .unwrap_or_else(|| "{}".into()),
                        });
                        let status = item.get("status").and_then(Value::as_str);
                        events.push(TurnEvent::Returned {
                            call_id: id,
                            name: Some(name),
                            is_error: matches!(status, Some("failed") | Some("declined"))
                                || item.get("error").is_some_and(|error| !error.is_null()),
                            text: mcp_result_text(item),
                        });
                    }
                    _ => {}
                }
            }
            Some("turn.failed") => {
                let message = event
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("turn_failed");
                events.push(TurnEvent::EndedAbnormally(message.to_string()));
            }
            _ => {}
        }
    }
    Ok(events)
}

/// Text of an MCP tool result: the MCP `content` text parts, else the error.
fn mcp_result_text(item: &Value) -> String {
    if let Some(result) = item.get("result").filter(|result| !result.is_null()) {
        return match result {
            Value::String(text) => text.clone(),
            _ => result
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| result.to_string()),
        };
    }
    match item.get("error") {
        Some(Value::Object(error)) => error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Value::Object(error.clone()).to_string()),
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

// Codex's built-in resource helpers emit mcp_tool_call with the requested
// server's name, even though they are not tools advertised by that server.
fn is_resource_helper(tool: &str) -> bool {
    matches!(
        tool,
        "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource"
    )
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_captured_world_turn_preserves_tools_results_and_usage() {
        let receipt = parse(include_str!(
            "../../../tests/fixtures/codex-exec-world.jsonl"
        ))
        .unwrap();
        assert!(receipt.success);
        assert!(receipt.foreign_reach.is_empty());
        assert!(receipt.denied_tool_calls.is_empty());
        assert_eq!(
            receipt.tool_calls,
            [
                "mcp__daycare__daycare_identity_get",
                "mcp__daycare__daycare_world_snapshot",
                "mcp__daycare__daycare_action_propose"
            ]
        );
        assert_eq!(receipt.tool_calls, receipt.permitted_tool_calls);
        assert_eq!(receipt.events.len(), 7);
        assert!(
            matches!(&receipt.events[5], TurnEvent::Returned { text, is_error: false, .. } if text.contains("\"accepted\": true"))
        );
        assert!(receipt.result_text.unwrap().contains("Mira looked up"));
        assert_eq!(receipt.usage.input_tokens, Some(9577));
        assert_eq!(receipt.usage.cache_read_input_tokens, Some(24576));
        assert_eq!(receipt.usage.output_tokens, Some(282));
    }

    #[test]
    fn a_captured_approval_refusal_is_not_a_successful_visit() {
        let receipt = parse(include_str!(
            "../../../tests/fixtures/codex-exec-approval-denied.jsonl"
        ))
        .unwrap();
        assert!(!receipt.success);
        assert!(receipt.permitted_tool_calls.is_empty());
        assert_eq!(
            receipt.denied_tool_calls,
            ["mcp__daycare__daycare_identity_get"]
        );
        assert!(receipt
            .events
            .iter()
            .any(|event| matches!(event, TurnEvent::Returned { is_error: true, .. })));
    }

    #[test]
    fn foreign_reach_is_rejected_even_when_the_item_never_completes() {
        for event_type in ["item.started", "item.updated", "item.completed"] {
            for item in [
                json!({"type":"mcp_tool_call", "server":"foreign", "tool":"read"}),
                json!({"type":"mcp_tool_call", "server":"daycare", "tool":"read_mcp_resource"}),
                json!({"type":"mcp_tool_call", "server":"daycare", "tool":"list_mcp_resources"}),
                json!({"type":"mcp_tool_call", "server":"daycare", "tool":"list_mcp_resource_templates"}),
                json!({"type":"command_execution", "command":"cat ~/.ssh/id_rsa"}),
                json!({"type":"collab_tool_call", "tool":"spawn_agent"}),
                json!({"type":"file_change"}),
                json!({"type":"web_search"}),
            ] {
                let stream = format!(
                    "{}\n{}\n{}",
                    json!({"type":"thread.started","thread_id":"test"}),
                    json!({"type":event_type,"item":item}),
                    json!({"type":"turn.completed","usage":{}})
                );
                assert!(
                    !parse(&stream).unwrap().foreign_reach.is_empty(),
                    "{stream}"
                );
            }
        }
    }

    #[test]
    fn cached_and_reasoning_tokens_are_not_double_counted() {
        let usage = turn_usage(
            &json!({"input_tokens":100, "cached_input_tokens":60, "cache_write_input_tokens":10, "output_tokens":20, "reasoning_output_tokens":15}),
        );
        assert_eq!(usage.input_tokens, Some(30));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cache_read_input_tokens, Some(60));
        assert_eq!(usage.cache_creation_input_tokens, Some(10));
        assert_eq!(usage.reasoning_output_tokens, Some(15));
    }
}
