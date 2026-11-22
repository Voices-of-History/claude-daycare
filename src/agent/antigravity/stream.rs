//! `agy --output-format stream-json` into the normalized `StreamReceipt`.
//!
//! One JSON object per line, each `{event, ...}`. Shapes were captured from
//! Antigravity CLI 1.5.13 (`tests/fixtures/antigravity-1.5.13/`) and match
//! Google's headless-mode page:
//!
//! | event         | payload                                                                 |
//! |---------------|-------------------------------------------------------------------------|
//! | `init`        | `conversation_id`, `init{model, cwd, agent, permission_mode, tools}`    |
//! | `step_update` | `step_update{conversation_id, step_index, state, step_type, ...}`       |
//! | `result`      | `result{conversation_id, status, response, error, num_turns, usage, ...}` |
//!
//! A `step_update` is reported as it changes: `state` is `ACTIVE`, then
//! `DONE` or `ERROR`. `step_type` is `user_input`, `agent_response` (with
//! `text_delta` fragments and, when done, `usage`), `tool` (with `tool_name`
//! and `tool_info{name, parameters, output}`) or `finish` (the built-in tool
//! that ends a turn). A daycare MCP call arrives as the built-in
//! `call_mcp_tool` with `parameters{ServerName, ToolName, Arguments}`; it is
//! renamed `mcp__daycare__<ToolName>` so the shared checks read every agent's
//! receipts the same way. Anything else the turn called lands in
//! `foreign_reach`, except `finish` and the custom agent's other built-ins that
//! reach nothing (`HARMLESS_BUILTINS`).
//!
//! `init.tools` lists Antigravity's global tool set, not the custom agent's,
//! so it proves nothing about the seal. What `init` does prove is which agent
//! ran (`init.agent`) and under which permission mode: a turn that ran as any
//! agent but `daycare`, or not in `strict` mode, is reported as foreign reach.

use crate::launch::{MCP_SERVER, MCP_TOOL_PREFIX};
use crate::stream::{looks_like_invented_tool_call, StreamReceipt, TurnEvent, TurnUsage};
use crate::{Error, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// The custom agent every turn runs as.
pub const AGENT_NAME: &str = "daycare";

/// The permission mode the seal's settings pin.
pub const PERMISSION_MODE: &str = "strict";

/// The built-in tool every MCP call goes through.
pub const MCP_CALL_TOOL: &str = "call_mcp_tool";

/// The built-in tool that ends a turn. It reaches nothing.
const FINISH_TOOL: &str = "finish";

/// The built-in tools the custom agent still has beside `finish` and
/// `call_mcp_tool` (checked on 1.5.13; see `docs/ANTIGRAVITY.md`). A model may
/// call them, and none touches the shell, files or network: `manage_task`
/// keeps the turn's own task list, and `list_resources` and `read_resource`
/// only see resources the one configured MCP server offers. A call to one is
/// not reach, and, like `finish`, it is not a tool call the checks count.
/// Everything else that is not a daycare MCP call stays foreign reach.
const HARMLESS_BUILTINS: [&str; 3] = ["manage_task", "list_resources", "read_resource"];

pub fn parse_stream(stream: &str) -> Result<StreamReceipt> {
    let mut session_id: Option<String> = None;
    let mut event_count = 0usize;
    let mut init_seen = false;
    let mut steps: BTreeMap<u64, String> = BTreeMap::new();
    let mut last_reply: Option<String> = None;
    let mut step_usage = Tokens::default();
    let mut result: Option<Value> = None;
    let mut tool_calls = Vec::new();
    let mut permitted_tool_calls = Vec::new();
    let mut foreign_reach = Vec::new();

    for (index, line) in stream.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|error| {
            Error::new(format!("line {} is not stream JSON: {error}", index + 1))
        })?;
        event_count += 1;
        if let Some(id) = conversation_id(&event) {
            match &session_id {
                None => session_id = Some(id.to_string()),
                Some(known) if known != id => {
                    return Err(Error::new(format!(
                        "agy stream switched conversations ({known} then {id})"
                    )))
                }
                Some(_) => {}
            }
        }
        match event.get("event").and_then(Value::as_str).unwrap_or("") {
            "init" => {
                init_seen = true;
                let init = &event["init"];
                let agent = init.get("agent").and_then(Value::as_str).unwrap_or("");
                if agent != AGENT_NAME {
                    foreign_reach.push(format!("the {agent:?} agent instead of {AGENT_NAME:?}"));
                }
                let mode = init
                    .get("permission_mode")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if mode != PERMISSION_MODE {
                    foreign_reach.push(format!("permission mode {mode:?}"));
                }
            }
            "step_update" => {
                let step = &event["step_update"];
                let state = step.get("state").and_then(Value::as_str).unwrap_or("");
                match step.get("step_type").and_then(Value::as_str).unwrap_or("") {
                    "agent_response" => {
                        let key = step_index(step);
                        if let Some(delta) = step.get("text_delta").and_then(Value::as_str) {
                            steps.entry(key).or_default().push_str(delta);
                        }
                        if state == "DONE" {
                            step_usage.add(&step["usage"]);
                            let text = steps.get(&key).map(|text| text.trim()).unwrap_or("");
                            if !text.is_empty() {
                                last_reply = Some(text.to_string());
                            }
                        }
                    }
                    "tool" if state == "DONE" || state == "ERROR" => {
                        let Some(call) = ToolCall::from_step(step) else {
                            continue;
                        };
                        if call.is_harmless() {
                            continue;
                        }
                        if !call.name.starts_with(MCP_TOOL_PREFIX) {
                            foreign_reach.push(call.reported.clone());
                        }
                        if state == "DONE" {
                            permitted_tool_calls.push(call.name.clone());
                        }
                        tool_calls.push(call.name);
                    }
                    _ => {}
                }
            }
            "result" => result = Some(event["result"].clone()),
            _ => {}
        }
    }

    let session_id = session_id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::new("agy stream carried no conversation id"))?;
    if !init_seen && !tool_calls.is_empty() {
        foreign_reach.push("a turn with no init event naming its agent".into());
    }
    let status = result
        .as_ref()
        .and_then(|result| result.get("status"))
        .and_then(Value::as_str);
    let reported_error = result
        .as_ref()
        .and_then(|result| result.get("error"))
        .and_then(Value::as_str)
        .filter(|error| !error.trim().is_empty())
        .map(str::to_string);
    let success = status == Some("SUCCESS") && reported_error.is_none();
    let error_subtype = if success {
        None
    } else {
        Some(match (status, reported_error) {
            (_, Some(error)) => error,
            (Some(status), None) => format!("a turn that ended with status {status}"),
            (None, None) => "a turn that ended before its result".into(),
        })
    };
    let result_text = last_reply.or_else(|| {
        result
            .as_ref()
            .and_then(|result| result.get("response"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    });
    // The result's usage covers the whole conversation so far: on a resumed
    // turn (agy 1.5.13) it repeats every earlier turn. The finished response
    // steps carry this turn's own usage, so they are the count; the result is
    // only a fallback for a stream with no step usage, where overcounting
    // stops a visit early instead of letting it run past its cap.
    let usage = if step_usage.is_empty() {
        let mut reported = Tokens::default();
        if let Some(usage) = result.as_ref().map(|result| &result["usage"]) {
            reported.add(usage);
        }
        reported
    } else {
        step_usage
    };
    let invented_tool_calls = result_text
        .as_deref()
        .is_some_and(looks_like_invented_tool_call);
    let events = transcript_events(stream)?;
    Ok(StreamReceipt {
        session_id,
        success,
        result_text,
        duration_ms: result
            .as_ref()
            .and_then(|result| number(&result["duration_seconds"]))
            .map(|seconds| (seconds * 1000.0).round() as u64),
        num_turns: result
            .as_ref()
            .and_then(|result| number(&result["num_turns"]))
            .map(|turns| turns as u64),
        stop_reason: result
            .as_ref()
            .and_then(|result| result.get("finished_reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        error_subtype,
        event_count,
        usage: usage.into_turn_usage(),
        init: None,
        tool_calls,
        permitted_tool_calls,
        // A call Antigravity refuses is reported as an `ERROR` tool step, the
        // same as one that failed for another reason; there is no separate
        // denial to record.
        denied_tool_calls: Vec::new(),
        invented_tool_calls,
        events,
        foreign_reach,
    })
}

/// The transcript events of one archived turn, leniently: a partial archive
/// still renders what it holds.
pub fn transcript_events(stream: &str) -> Result<Vec<TurnEvent>> {
    let mut events = Vec::new();
    let mut texts: BTreeMap<u64, String> = BTreeMap::new();
    for (index, line) in stream.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|error| {
            Error::new(format!("line {} is not stream JSON: {error}", index + 1))
        })?;
        match event.get("event").and_then(Value::as_str).unwrap_or("") {
            "step_update" => {
                let step = &event["step_update"];
                let state = step.get("state").and_then(Value::as_str).unwrap_or("");
                match step.get("step_type").and_then(Value::as_str).unwrap_or("") {
                    "agent_response" => {
                        let key = step_index(step);
                        if let Some(delta) = step.get("text_delta").and_then(Value::as_str) {
                            texts.entry(key).or_default().push_str(delta);
                        }
                        if state == "DONE" {
                            if let Some(text) = texts.remove(&key) {
                                if !text.trim().is_empty() {
                                    events.push(TurnEvent::Said(text.trim().to_string()));
                                }
                            }
                        }
                    }
                    "tool" if state == "DONE" || state == "ERROR" => {
                        let Some(call) = ToolCall::from_step(step) else {
                            continue;
                        };
                        if call.is_finish() {
                            continue;
                        }
                        let id = Some(format!("step-{}", step_index(step)));
                        events.push(TurnEvent::Called {
                            id: id.clone(),
                            name: call.name,
                            input: call.input,
                        });
                        let info = &step["tool_info"];
                        let text = info
                            .get("output")
                            .filter(|output| !output.is_null())
                            .or_else(|| info.get("error").filter(|error| !error.is_null()))
                            .or_else(|| step.get("error"))
                            .map(|value| match value {
                                Value::String(text) => text.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default();
                        events.push(TurnEvent::Returned {
                            call_id: id,
                            name: None,
                            is_error: state == "ERROR",
                            text,
                        });
                    }
                    _ => {}
                }
            }
            "result" => {
                let result = &event["result"];
                if result.get("status").and_then(Value::as_str) != Some("SUCCESS") {
                    let error = result
                        .get("error")
                        .and_then(Value::as_str)
                        .filter(|error| !error.trim().is_empty())
                        .unwrap_or("the turn ended with an error");
                    events.push(TurnEvent::EndedAbnormally(error.to_string()));
                }
            }
            _ => {}
        }
    }
    Ok(events)
}

/// The tokens one line of the stream spent, counted as the ledger counts
/// them. A finished `agent_response` step carries its own usage. Used while
/// the turn runs, so the runner can stop a turn that passes the visit's token
/// cap mid-turn.
pub fn tokens_in_line(line: &str) -> u64 {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return 0;
    };
    if event.get("event").and_then(Value::as_str) != Some("step_update") {
        return 0;
    }
    let step = &event["step_update"];
    if step.get("step_type").and_then(Value::as_str) != Some("agent_response")
        || step.get("state").and_then(Value::as_str) != Some("DONE")
    {
        return 0;
    }
    let mut tokens = Tokens::default();
    tokens.add(&step["usage"]);
    tokens.counted()
}

/// One finished tool step, normalized.
struct ToolCall {
    /// `mcp__daycare__<tool>` for a daycare call, else what the agent reported.
    name: String,
    /// How the agent reported it, for failure text.
    reported: String,
    /// The call's arguments as JSON text.
    input: String,
}

impl ToolCall {
    fn from_step(step: &Value) -> Option<Self> {
        let info = &step["tool_info"];
        let tool = step
            .get("tool_name")
            .and_then(Value::as_str)
            .or_else(|| info.get("name").and_then(Value::as_str))?;
        let parameters = &info["parameters"];
        if tool == MCP_CALL_TOOL {
            let server = parameters
                .get("ServerName")
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = parameters
                .get("ToolName")
                .and_then(Value::as_str)
                .unwrap_or("");
            let input = parameters
                .get("Arguments")
                .map(Value::to_string)
                .unwrap_or_else(|| "{}".into());
            let reported = format!("{MCP_CALL_TOOL} {server}/{name}");
            let name = if server == MCP_SERVER && !name.is_empty() {
                format!("{MCP_TOOL_PREFIX}{name}")
            } else {
                reported.clone()
            };
            return Some(ToolCall {
                name,
                reported,
                input,
            });
        }
        Some(ToolCall {
            name: tool.to_string(),
            reported: tool.to_string(),
            input: if parameters.is_null() {
                "{}".into()
            } else {
                parameters.to_string()
            },
        })
    }

    fn is_finish(&self) -> bool {
        self.name == FINISH_TOOL
    }

    /// `finish` or one of the built-ins that reach nothing. Only a call the
    /// agent reported by its own name counts: a `call_mcp_tool` to another
    /// server is renamed with its server and stays foreign.
    fn is_harmless(&self) -> bool {
        self.is_finish() || HARMLESS_BUILTINS.contains(&self.name.as_str())
    }
}

fn conversation_id(event: &Value) -> Option<&str> {
    [
        event.get("conversation_id"),
        event["step_update"].get("conversation_id"),
        event["result"].get("conversation_id"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|id| !id.is_empty())
}

fn step_index(step: &Value) -> u64 {
    number(&step["step_index"])
        .map(|index| index as u64)
        .unwrap_or(0)
}

/// Antigravity writes some numbers as JSON numbers and some as strings.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

#[derive(Default)]
struct Tokens {
    input: Option<u64>,
    output: Option<u64>,
    thinking: Option<u64>,
    cache_read: Option<u64>,
}

impl Tokens {
    fn add(&mut self, usage: &Value) {
        fn add_to(slot: &mut Option<u64>, value: &Value) {
            if let Some(value) = number(value) {
                *slot = Some(slot.unwrap_or(0) + value.max(0.0) as u64);
            }
        }
        add_to(&mut self.input, &usage["input_tokens"]);
        add_to(&mut self.output, &usage["output_tokens"]);
        add_to(&mut self.thinking, &usage["thinking_tokens"]);
        add_to(&mut self.cache_read, &usage["cache_read_tokens"]);
    }

    /// Everything the visit ledger counts: input, output, thinking and cached
    /// input. agy reports cache reads apart from `input_tokens` (a captured
    /// turn had 25,375 input and 40,253 cache reads), and the ledger counts
    /// them, so the live cap has to as well.
    fn counted(&self) -> u64 {
        self.input.unwrap_or(0)
            + self.output.unwrap_or(0)
            + self.thinking.unwrap_or(0)
            + self.cache_read.unwrap_or(0)
    }

    fn is_empty(&self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.thinking.is_none()
            && self.cache_read.is_none()
    }

    fn into_turn_usage(self) -> TurnUsage {
        let output = match (self.output, self.thinking) {
            (None, None) => None,
            (output, thinking) => Some(output.unwrap_or(0) + thinking.unwrap_or(0)),
        };
        TurnUsage {
            input_tokens: self.input,
            output_tokens: output,
            cache_read_input_tokens: self.cache_read,
            reasoning_output_tokens: self.thinking,
            ..TurnUsage::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/antigravity-1.5.13")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn a_world_turn_names_daycare_calls_and_reaches_nothing_else() {
        let receipt = parse_stream(&fixture("world.jsonl")).unwrap();
        assert!(receipt.success, "{:?}", receipt.error_subtype);
        assert_eq!(receipt.session_id, "c0ffee00-0000-4000-8000-000000000001");
        assert_eq!(
            receipt.tool_calls,
            [
                "mcp__daycare__daycare_identity_get",
                "mcp__daycare__daycare_world_snapshot"
            ]
        );
        assert_eq!(receipt.permitted_tool_calls, receipt.tool_calls);
        assert!(
            receipt.foreign_reach.is_empty(),
            "{:?}",
            receipt.foreign_reach
        );
        assert_eq!(
            receipt.result_text.as_deref(),
            Some("I looked around the courtyard.")
        );
        assert_eq!(receipt.num_turns, Some(1));
        assert_eq!(receipt.usage.input_tokens, Some(9100));
        assert_eq!(receipt.usage.output_tokens, Some(120));
        assert_eq!(receipt.usage.cache_read_input_tokens, Some(4000));
        let said: Vec<_> = receipt
            .events
            .iter()
            .filter(|event| matches!(event, TurnEvent::Said(_)))
            .collect();
        assert_eq!(said.len(), 1);
        assert!(receipt.events.iter().any(|event| matches!(
            event,
            TurnEvent::Returned { is_error: false, text, .. } if text.contains("Pip")
        )));
    }

    #[test]
    fn a_shell_call_another_server_or_another_agent_is_foreign_reach() {
        let world = fixture("world.jsonl");
        let shell = world.replace(
            r#""tool_name": "call_mcp_tool", "tool_info": {"name": "call_mcp_tool", "parameters": {"ServerName": "daycare", "ToolName": "daycare_world_snapshot", "Arguments": {}}"#,
            r#""tool_name": "run_command", "tool_info": {"name": "run_command", "parameters": {"CommandLine": "ls ~"}"#,
        );
        assert_ne!(shell, world);
        let receipt = parse_stream(&shell).unwrap();
        assert_eq!(receipt.foreign_reach, ["run_command"]);

        let other = world.replace(r#""ServerName": "daycare""#, r#""ServerName": "github""#);
        let receipt = parse_stream(&other).unwrap();
        assert_eq!(receipt.foreign_reach.len(), 2);
        assert!(receipt.foreign_reach[0].contains("github/"));

        let agent = world.replace(r#""agent": "daycare""#, r#""agent": "default""#);
        let receipt = parse_stream(&agent).unwrap();
        assert!(receipt.foreign_reach[0].contains("\"default\" agent"));

        let mode = world.replace(
            r#""permission_mode": "strict""#,
            r#""permission_mode": "default""#,
        );
        assert!(parse_stream(&mode).unwrap().foreign_reach[0].contains("permission mode"));
    }

    /// The world fixture with one finished tool step of `tool` inserted just
    /// before the result.
    fn world_with_step(tool: &str, state: &str) -> String {
        let step = format!(
            r#"{{"event": "step_update", "step_update": {{"conversation_id": "c0ffee00-0000-4000-8000-000000000001", "step_index": 9, "state": "{state}", "step_type": "tool", "tool_name": "{tool}", "tool_info": {{"name": "{tool}", "parameters": {{}}, "output": "ok"}}}}}}"#
        );
        let mut lines: Vec<String> = fixture("world.jsonl").lines().map(str::to_string).collect();
        let result = lines
            .iter()
            .position(|line| line.contains(r#""event": "result""#))
            .unwrap();
        lines.insert(result, step);
        lines.join("\n")
    }

    #[test]
    fn the_harmless_built_ins_are_not_foreign_reach() {
        for tool in HARMLESS_BUILTINS {
            for state in ["DONE", "ERROR"] {
                let receipt = parse_stream(&world_with_step(tool, state)).unwrap();
                assert!(
                    receipt.foreign_reach.is_empty(),
                    "{tool} {state}: {:?}",
                    receipt.foreign_reach
                );
                // Like `finish`, they are not counted as tool calls, so the
                // purpose checks still see only daycare calls.
                assert_eq!(
                    receipt.tool_calls,
                    [
                        "mcp__daycare__daycare_identity_get",
                        "mcp__daycare__daycare_world_snapshot"
                    ],
                    "{tool} {state}"
                );
                assert!(receipt.success);
            }
        }
    }

    #[test]
    fn shell_file_and_network_tools_stay_foreign_reach() {
        for tool in [
            "command",
            "run_command",
            "read_file",
            "write_file",
            "read_url",
            "execute_url",
            "download_url",
            "search_web",
            "manage_task_extra",
        ] {
            let receipt = parse_stream(&world_with_step(tool, "DONE")).unwrap();
            assert_eq!(receipt.foreign_reach, [tool], "{tool}");
        }
        // Naming a harmless built-in on another MCP server does not borrow its
        // pass.
        let world = fixture("world.jsonl")
            .replace(r#""ServerName": "daycare""#, r#""ServerName": "github""#)
            .replace(
                r#""ToolName": "daycare_world_snapshot""#,
                r#""ToolName": "manage_task""#,
            );
        let receipt = parse_stream(&world).unwrap();
        assert!(receipt
            .foreign_reach
            .iter()
            .any(|reach| reach.contains("github/manage_task")));
    }

    #[test]
    fn an_error_result_fails_the_turn_with_its_reason() {
        let receipt = parse_stream(&fixture("error.jsonl")).unwrap();
        assert!(!receipt.success);
        assert!(receipt.error_subtype.unwrap().contains("authentication"));
        assert!(matches!(
            receipt.events.last(),
            Some(TurnEvent::EndedAbnormally(text)) if text.contains("authentication")
        ));
    }

    #[test]
    fn a_turn_killed_before_its_result_is_not_a_success() {
        let world = fixture("world.jsonl");
        let cut: String = world
            .lines()
            .filter(|line| !line.contains(r#""event": "result""#))
            .map(|line| format!("{line}\n"))
            .collect();
        let receipt = parse_stream(&cut).unwrap();
        assert!(!receipt.success);
        assert_eq!(
            receipt.error_subtype.as_deref(),
            Some("a turn that ended before its result")
        );
        // Without a result, usage comes from the finished steps.
        assert_eq!(receipt.usage.input_tokens, Some(9100));
    }

    #[test]
    fn only_finished_response_steps_count_tokens_live() {
        let world = fixture("world.jsonl");
        let total: u64 = world.lines().map(tokens_in_line).sum();
        assert_eq!(total, 9100 + 120 + 4000);
        assert_eq!(tokens_in_line("not json"), 0);
    }

    #[test]
    fn the_live_count_matches_what_the_ledger_records() {
        let world = fixture("world.jsonl");
        let live: u64 = world.lines().map(tokens_in_line).sum();
        let usage = parse_stream(&world).unwrap().usage;
        // VisitLedger::record_turn counts input + output + cache reads, with
        // thinking already inside output.
        let ledger = usage.input_tokens.unwrap_or(0)
            + usage.output_tokens.unwrap_or(0)
            + usage.cache_read_input_tokens.unwrap_or(0);
        assert_eq!(live, ledger);
    }

    #[test]
    fn a_resumed_turn_counts_its_own_steps_not_the_conversation_total() {
        // On a resumed conversation agy 1.5.13's result repeats the earlier
        // turns' usage. Seen live: turn two's result said 33,627 input when
        // its own steps used 8,252. Model that with a result far above the
        // steps.
        let world = fixture("world.jsonl");
        let resumed = world.replace(
            r#""usage": {"input_tokens": 9100, "output_tokens": 120, "thinking_tokens": 0, "cache_read_tokens": 4000, "total_tokens": 9220}"#,
            r#""usage": {"input_tokens": 34100, "output_tokens": 620, "thinking_tokens": 300, "cache_read_tokens": 44000, "total_tokens": 34720}"#,
        );
        assert_ne!(resumed, world);
        let usage = parse_stream(&resumed).unwrap().usage;
        assert_eq!(usage.input_tokens, Some(9100));
        assert_eq!(usage.output_tokens, Some(120));
        assert_eq!(usage.cache_read_input_tokens, Some(4000));

        // With no step usage at all, the result is still counted.
        let result_only: String = world
            .lines()
            .map(|line| match line.find(r#", "usage": {"#) {
                Some(start) if line.contains(r#""step_type": "agent_response""#) => {
                    let end = line[start..].find('}').unwrap() + start + 1;
                    format!("{}{}\n", &line[..start], &line[end..])
                }
                _ => format!("{line}\n"),
            })
            .collect();
        assert!(!result_only.contains(r#""usage": {"input_tokens": 3000"#));
        let usage = parse_stream(&result_only).unwrap().usage;
        assert_eq!(usage.input_tokens, Some(9100));
    }

    #[test]
    fn a_switched_conversation_is_rejected() {
        let world = fixture("world.jsonl");
        let mut lines: Vec<String> = world.lines().map(str::to_string).collect();
        let last = lines.len() - 1;
        lines[last] = lines[last].replace(
            "c0ffee00-0000-4000-8000-000000000001",
            "c0ffee00-0000-4000-8000-000000000002",
        );
        assert!(parse_stream(&lines.join("\n"))
            .unwrap_err()
            .to_string()
            .contains("switched conversations"));
    }
}
