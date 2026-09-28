//! `opencode run --format json` into the normalized `StreamReceipt`.
//!
//! One JSON object per line, each `{type, timestamp, sessionID, part|error}`
//! (`packages/opencode/src/cli/cmd/run.ts`, `emit()`). Shapes were captured
//! from opencode 1.18.33 against a mock LLM and the mock daycare server — see
//! `tests/fixtures/opencode-1.18.33/`.
//!
//! | type          | payload                                                          |
//! |---------------|------------------------------------------------------------------|
//! | `step_start`  | `part.type = "step-start"`                                       |
//! | `tool_use`    | `part.tool`, `part.callID`, `part.state{status,input,output}`    |
//! | `step_finish` | `part.reason`, `part.cost`, `part.tokens{input,output,reasoning,cache{read,write},total}` |
//! | `text`        | `part.text`, emitted once the part is complete                   |
//! | `error`       | `error{name, data{message}}`; the process exits 1                |
//!
//! There is no init event: the tool list is proven before the turn
//! (`opencode debug agent`), and after it by the names in `tool_use`. An MCP
//! tool is named `<server>_<tool>`, so the daycare server's `daycare_look`
//! arrives as `daycare_daycare_look`; it is renamed to `mcp__daycare__daycare_look`
//! so the shared checks read every agent's receipts the same way. Anything
//! else the turn called lands in `foreign_reach`.

use crate::launch::MCP_TOOL_PREFIX;
use crate::stream::{looks_like_invented_tool_call, StreamReceipt, TurnEvent, TurnUsage};
use crate::{Error, Result};
use serde_json::Value;

/// What OpenCode puts in front of every tool the daycare MCP server offers.
pub const OPENCODE_TOOL_PREFIX: &str = "daycare_";

/// The step reason OpenCode reports when the model asked for tools and the
/// loop will continue. A turn whose last step ended this way was cut off.
const CONTINUING_REASON: &str = "tool-calls";

pub fn parse_stream(stream: &str) -> Result<StreamReceipt> {
    let mut session_id: Option<String> = None;
    let mut event_count = 0usize;
    let mut first_timestamp = None;
    let mut last_timestamp = None;
    let mut steps = 0u64;
    let mut stop_reason: Option<String> = None;
    let mut error_subtype: Option<String> = None;
    let mut usage = Tokens::default();
    let mut tool_calls = Vec::new();
    let mut foreign_reach = Vec::new();
    // Text of the step in progress; the last finished step's text is the
    // turn's reply.
    let mut step_text: Vec<String> = Vec::new();
    let mut result_text: Option<String> = None;
    let mut last_type = String::new();

    for (index, line) in stream.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|error| {
            Error::new(format!("line {} is not stream JSON: {error}", index + 1))
        })?;
        event_count += 1;
        if let Some(id) = event.get("sessionID").and_then(Value::as_str) {
            match &session_id {
                None => session_id = Some(id.to_string()),
                // `run` mirrors only its own session. Another id means the
                // stream is not one turn of one session.
                Some(known) if known != id => {
                    return Err(Error::new(format!(
                        "opencode stream switched sessions ({known} then {id})"
                    )))
                }
                Some(_) => {}
            }
        }
        if let Some(timestamp) = event.get("timestamp").and_then(Value::as_i64) {
            first_timestamp.get_or_insert(timestamp);
            last_timestamp = Some(timestamp);
        }
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let part = &event["part"];
        match kind {
            "step_start" => step_text.clear(),
            "text" => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    step_text.push(text.to_string());
                }
            }
            "tool_use" => {
                let name = part.get("tool").and_then(Value::as_str).unwrap_or("");
                let normalized = normalize_tool_name(name);
                if !normalized.starts_with(MCP_TOOL_PREFIX) {
                    foreign_reach.push(name.to_string());
                }
                tool_calls.push(normalized);
            }
            "step_finish" => {
                steps += 1;
                usage.add(part);
                stop_reason = part
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let text = step_text.join("\n");
                result_text = (!text.trim().is_empty()).then_some(text);
                step_text.clear();
            }
            "error" => error_subtype = Some(error_text(&event["error"])),
            _ => {}
        }
        last_type = kind.to_string();
    }

    let session_id =
        session_id.ok_or_else(|| Error::new("opencode stream carried no session id"))?;
    // A turn is over when its last step finished for a reason other than
    // wanting more tools, and nothing errored. A killed turn ends mid-step.
    let finished = last_type == "step_finish"
        && stop_reason
            .as_deref()
            .is_some_and(|reason| reason != CONTINUING_REASON);
    let success = error_subtype.is_none() && finished;
    let error_subtype = error_subtype.or_else(|| {
        (!success).then(|| match stop_reason.as_deref() {
            Some(CONTINUING_REASON) | None => "a turn that ended before its last step".into(),
            Some(reason) => format!("a turn that stopped for {reason}"),
        })
    });
    let invented_tool_calls = result_text
        .as_deref()
        .is_some_and(looks_like_invented_tool_call);
    let events = transcript_events(stream)?;
    Ok(StreamReceipt {
        session_id,
        success,
        result_text,
        duration_ms: match (first_timestamp, last_timestamp) {
            (Some(first), Some(last)) if last >= first => Some((last - first) as u64),
            _ => None,
        },
        num_turns: Some(steps),
        stop_reason,
        error_subtype,
        event_count,
        usage: usage.into_turn_usage(),
        init: None,
        permitted_tool_calls: tool_calls.clone(),
        tool_calls,
        // OpenCode removes a denied tool from the model's list instead of
        // refusing a call to it, so there is nothing to record here.
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
    for (index, line) in stream.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line).map_err(|error| {
            Error::new(format!("line {} is not stream JSON: {error}", index + 1))
        })?;
        let part = &event["part"];
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        events.push(TurnEvent::Said(text.to_string()));
                    }
                }
            }
            "tool_use" => {
                let id = part
                    .get("callID")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let name =
                    normalize_tool_name(part.get("tool").and_then(Value::as_str).unwrap_or(""));
                let state = &part["state"];
                events.push(TurnEvent::Called {
                    id: id.clone(),
                    name,
                    input: state
                        .get("input")
                        .map(|input| input.to_string())
                        .unwrap_or_else(|| "{}".into()),
                });
                let is_error = state.get("status").and_then(Value::as_str) == Some("error");
                let text = if is_error {
                    state.get("error").or_else(|| state.get("output"))
                } else {
                    state.get("output")
                };
                if let Some(text) = text {
                    events.push(TurnEvent::Returned {
                        call_id: id,
                        name: None,
                        is_error,
                        text: match text {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        },
                    });
                }
            }
            "error" => events.push(TurnEvent::EndedAbnormally(error_text(&event["error"]))),
            _ => {}
        }
    }
    Ok(events)
}

/// The tokens one line of the stream spent, counted as the ledger counts
/// them. Only `step_finish` carries usage. Used while the turn runs, so the
/// runner can stop a turn that passes the visit's token cap mid-turn.
pub fn tokens_in_line(line: &str) -> u64 {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return 0;
    };
    if event.get("type").and_then(Value::as_str) != Some("step_finish") {
        return 0;
    }
    let mut tokens = Tokens::default();
    tokens.add(&event["part"]);
    tokens.counted()
}

/// `daycare_daycare_look` → `mcp__daycare__daycare_look`. A name without the
/// server's prefix is not the daycare server's and is kept as reported.
pub fn normalize_tool_name(name: &str) -> String {
    match name.strip_prefix(OPENCODE_TOOL_PREFIX) {
        Some(tool) if !tool.is_empty() => format!("{MCP_TOOL_PREFIX}{tool}"),
        _ => name.to_string(),
    }
}

fn error_text(error: &Value) -> String {
    let name = error.get("name").and_then(Value::as_str).unwrap_or("error");
    match error
        .pointer("/data/message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
    {
        Some(message) => format!("{name}: {message}"),
        None => name.to_string(),
    }
}

/// Usage summed over a turn's steps. OpenCode reports `output` without the
/// reasoning tokens (captured: total 1050 = input 700 + output 30 +
/// reasoning 20 + cache read 300), while `TurnUsage.output_tokens` includes
/// them, so they are added back.
#[derive(Default)]
struct Tokens {
    seen: bool,
    input: u64,
    output: u64,
    reasoning: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
}

impl Tokens {
    fn add(&mut self, part: &Value) {
        let tokens = &part["tokens"];
        if tokens.is_object() {
            self.seen = true;
            let field =
                |pointer: &str| tokens.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
            let components = field("/input")
                + field("/output")
                + field("/reasoning")
                + field("/cache/read")
                + field("/cache/write");
            if components == 0 {
                // A build that reports only the total still gets counted.
                self.input += field("/total");
            } else {
                self.input += field("/input");
                self.output += field("/output");
                self.reasoning += field("/reasoning");
                self.cache_read += field("/cache/read");
                self.cache_write += field("/cache/write");
            }
        }
        if let Some(cost) = part.get("cost").and_then(Value::as_f64) {
            if cost.is_finite() && cost > 0.0 {
                self.cost += cost;
            }
        }
    }

    fn counted(&self) -> u64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }

    fn into_turn_usage(self) -> TurnUsage {
        if !self.seen {
            return TurnUsage::default();
        }
        TurnUsage {
            input_tokens: Some(self.input),
            output_tokens: Some(self.output + self.reasoning),
            cache_read_input_tokens: Some(self.cache_read),
            cache_creation_input_tokens: Some(self.cache_write),
            // Zero means "unknown" for subscription logins: OpenCode forces
            // the cost of ChatGPT-OAuth models to 0.
            total_cost_usd: (self.cost > 0.0).then_some(self.cost),
            reasoning_output_tokens: (self.reasoning > 0).then_some(self.reasoning),
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
                .join("tests/fixtures/opencode-1.18.33")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn a_world_turn_yields_a_receipt_with_normalized_daycare_calls() {
        let receipt = parse_stream(&fixture("world.jsonl")).unwrap();
        assert_eq!(receipt.session_id, "ses_f173d91f7ffebhfg3cOJwHnscV");
        assert!(receipt.success);
        assert_eq!(receipt.stop_reason.as_deref(), Some("stop"));
        assert_eq!(receipt.num_turns, Some(2));
        assert_eq!(receipt.event_count, 6);
        assert_eq!(
            receipt.tool_calls,
            vec!["mcp__daycare__daycare_identity_get".to_string()]
        );
        assert!(receipt.foreign_reach.is_empty());
        assert_eq!(
            receipt.result_text.as_deref(),
            Some("I looked around the courtyard.")
        );
        assert!(receipt.init.is_none());
    }

    #[test]
    fn usage_sums_every_step_and_puts_reasoning_inside_output() {
        let usage = parse_stream(&fixture("world.jsonl")).unwrap().usage;
        assert_eq!(usage.input_tokens, Some(1400));
        assert_eq!(usage.output_tokens, Some(100));
        assert_eq!(usage.reasoning_output_tokens, Some(40));
        assert_eq!(usage.cache_read_input_tokens, Some(600));
        assert_eq!(usage.cache_creation_input_tokens, Some(0));
        // Cost 0 is what OpenCode reports for subscription models: unknown.
        assert_eq!(usage.total_cost_usd, None);
    }

    #[test]
    fn the_live_counter_matches_what_the_ledger_will_count() {
        let stream = fixture("world.jsonl");
        let live: u64 = stream.lines().map(tokens_in_line).sum();
        let usage = parse_stream(&stream).unwrap().usage;
        let ledger = usage.input_tokens.unwrap()
            + usage.output_tokens.unwrap()
            + usage.cache_read_input_tokens.unwrap()
            + usage.cache_creation_input_tokens.unwrap();
        assert_eq!(live, 2100);
        assert_eq!(live, ledger);
    }

    #[test]
    fn a_homecoming_calls_only_memory_tools() {
        let receipt = parse_stream(&fixture("homecoming.jsonl")).unwrap();
        assert!(receipt.success);
        assert_eq!(
            receipt.tool_calls,
            vec!["mcp__daycare__daycare_memory_list".to_string()]
        );
    }

    #[test]
    fn a_day_report_calls_nothing() {
        let receipt = parse_stream(&fixture("day-report.jsonl")).unwrap();
        assert!(receipt.success);
        assert!(receipt.tool_calls.is_empty());
        assert!(receipt.result_text.is_some());
    }

    #[test]
    fn an_error_event_fails_the_turn_by_name() {
        let receipt = parse_stream(&fixture("error.jsonl")).unwrap();
        assert!(!receipt.success);
        assert_eq!(
            receipt.error_subtype.as_deref(),
            Some("UnknownError: Unexpected server error. Check server logs for details.")
        );
        assert!(receipt.usage.is_empty());
    }

    #[test]
    fn a_turn_cut_off_mid_step_is_not_a_success_but_keeps_its_usage() {
        let stream: String = fixture("world.jsonl")
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join("\n");
        let receipt = parse_stream(&stream).unwrap();
        assert!(!receipt.success);
        assert_eq!(receipt.stop_reason.as_deref(), Some("tool-calls"));
        assert_eq!(receipt.usage.input_tokens, Some(700));
    }

    #[test]
    fn a_built_in_or_foreign_tool_is_foreign_reach() {
        let stream = r#"{"type":"step_start","timestamp":1,"sessionID":"ses_a","part":{}}
{"type":"tool_use","timestamp":2,"sessionID":"ses_a","part":{"tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"ls"},"output":"x"}}}
{"type":"tool_use","timestamp":2,"sessionID":"ses_a","part":{"tool":"github_search","callID":"c2","state":{"status":"completed","input":{},"output":"x"}}}
{"type":"step_finish","timestamp":3,"sessionID":"ses_a","part":{"reason":"stop","tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0}}"#;
        let receipt = parse_stream(stream).unwrap();
        assert_eq!(receipt.foreign_reach, vec!["bash", "github_search"]);
    }

    #[test]
    fn a_stream_that_switches_sessions_is_refused() {
        let stream = r#"{"type":"step_start","timestamp":1,"sessionID":"ses_a","part":{}}
{"type":"step_start","timestamp":1,"sessionID":"ses_b","part":{}}"#;
        assert!(parse_stream(stream).is_err());
    }

    #[test]
    fn a_stream_with_no_session_is_refused() {
        assert!(parse_stream("").is_err());
    }

    #[test]
    fn the_transcript_pairs_calls_with_their_results() {
        let events = transcript_events(&fixture("world.jsonl")).unwrap();
        assert!(matches!(
            &events[0],
            TurnEvent::Called { name, id: Some(id), .. }
                if name == "mcp__daycare__daycare_identity_get" && id == "call_0"
        ));
        assert!(matches!(
            &events[1],
            TurnEvent::Returned { call_id: Some(id), is_error: false, .. } if id == "call_0"
        ));
        assert!(matches!(&events[2], TurnEvent::Said(text) if text.contains("courtyard")));
    }

    #[test]
    fn a_corrupt_line_is_named() {
        let error = transcript_events("{}\nnot json").unwrap_err();
        assert!(error.to_string().contains("line 2 is not stream JSON"));
    }
}
