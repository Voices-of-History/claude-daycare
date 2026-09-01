//! Parsing the `--output-format stream-json` NDJSON that a turn produces.
//!
//! The raw stream is archived to `turns/<command_id>.jsonl` before this runs;
//! everything here is a projection over that archive, never a second source of
//! truth. Shapes were taken from a real Claude Code 2.1.220 run — see
//! `tests/fixtures/turn-stream-2.1.220.jsonl`.

use crate::launch::{MCP_SERVER, MCP_TOOL_PREFIX, TOOL_SEARCH_TOOL, WEB_SEARCH_TOOL};
use crate::{Error, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// What the child reported about itself in the `system`/`init` event. The
/// companion checks this against the sandbox it asked for: argv says what we
/// requested, this says what Claude actually did.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct InitReport {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub api_key_source: Option<String>,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<McpServerReport>,
    /// Keys are memory kinds (`auto`, `user`, `project`, ...) mapped to paths.
    pub memory_paths: Vec<(String, String)>,
}

/// One entry of the init event's `mcp_servers`. The status matters as much as
/// the name: `pending` means the child never finished connecting, and a turn
/// that starts in that state has no world tools at all (see `verify_sandbox`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct McpServerReport {
    pub name: String,
    pub status: Option<String>,
}

impl McpServerReport {
    pub fn is_connected(&self) -> bool {
        self.status.as_deref() == Some("connected")
    }
}

/// Usage as the stream reported it. Every field is optional because a stream
/// that did not carry usage must stay unknown — never zero, never invented.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TurnUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub total_cost_usd: Option<f64>,
    /// From the `rate_limit_event` the CLI emits on subscription auth.
    pub rate_limit_type: Option<String>,
    pub rate_limit_status: Option<String>,
    /// Fraction of the window consumed, 0.0–1.0. The status string alone
    /// carries no severity — `allowed_warning` has been observed at 0.31 on a
    /// `seven_day` window and at 0.98 on a `five_hour` one — so anything that
    /// reasons about how full the window is has to read this.
    pub rate_limit_utilization: Option<f64>,
    pub rate_limit_resets_at: Option<i64>,
}

impl TurnUsage {
    pub fn is_empty(&self) -> bool {
        *self == TurnUsage::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamReceipt {
    pub session_id: String,
    pub success: bool,
    pub result_text: Option<String>,
    pub duration_ms: Option<u64>,
    pub num_turns: Option<u64>,
    pub stop_reason: Option<String>,
    pub error_subtype: Option<String>,
    pub event_count: usize,
    pub usage: TurnUsage,
    pub init: Option<InitReport>,
    /// Every tool the child actually invoked, in order. A turn with no daycare
    /// call in here reached nothing; whether it held or invented a world is
    /// decided by `verify_reached_the_world`.
    pub tool_calls: Vec<String>,
    /// `tool_calls` minus the ones Claude Code's permission layer refused.
    /// With `--allowedTools`/`--disallowedTools` the deny wins, but the
    /// assistant still emits the `tool_use` block; the child then reports the
    /// refusal as an `is_error` "Permission … denied" tool_result and lists it
    /// under the result event's `permission_denials` (live, 2.1.252). A denied
    /// call reached nothing, so a check on what the turn actually did reads
    /// this list.
    pub permitted_tool_calls: Vec<String>,
    /// The refused calls, by name, in order.
    pub denied_tool_calls: Vec<String>,
    /// True when the child wrote a tool call out as prose instead of invoking
    /// one — the signature of a model inventing results it never received.
    pub invented_tool_calls: bool,
    /// True only when the Debate League turn tool returned its canonical
    /// `played: true` receipt for the matching tool call.
    pub league_turn_applied: bool,
    /// True when that applied turn belongs to the external two-Claude mode.
    /// External matches own the visit lifecycle until their canonical verdict.
    pub league_turn_external: bool,
}

pub fn parse_stream_file(path: &Path) -> Result<StreamReceipt> {
    let text = std::fs::read_to_string(path)?;
    parse_stream(&text)
}

pub fn parse_stream(stream: &str) -> Result<StreamReceipt> {
    let mut session_id: Option<String> = None;
    let mut success = false;
    let mut result_text = None;
    let mut duration_ms = None;
    let mut num_turns = None;
    let mut stop_reason = None;
    let mut error_subtype = None;
    let mut event_count = 0usize;
    let mut usage = TurnUsage::default();
    let mut init = None;
    let mut tool_calls = Vec::new();
    // (tool_use id, name) in call order; ids pair calls with their denials.
    let mut tool_uses: Vec<(Option<String>, String)> = Vec::new();
    let mut denied_ids: HashSet<String> = HashSet::new();
    let mut tool_names_by_id = HashMap::new();
    let mut league_turn_applied = false;
    let mut league_turn_external = false;
    let mut invented_tool_calls = false;

    for (index, line) in stream.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A malformed line is a truncated archive, not a silent zero: fail loudly.
        let event: Value = serde_json::from_str(line).map_err(|error| {
            Error::new(format!(
                "invalid stream JSON on line {}: {error}",
                index + 1
            ))
        })?;
        event_count += 1;

        if let Some(id) = event.get("session_id").and_then(Value::as_str) {
            session_id = Some(id.to_string());
        }

        match event.get("type").and_then(Value::as_str) {
            Some("system") if event.get("subtype").and_then(Value::as_str) == Some("init") => {
                init = Some(parse_init(&event));
            }
            Some("assistant") => {
                if let Some(content) = event
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                            if let Some(name) = string_at(block, "name") {
                                let id = string_at(block, "id");
                                if let Some(id) = &id {
                                    tool_names_by_id.insert(id.clone(), name.clone());
                                }
                                tool_uses.push((id, name.clone()));
                                tool_calls.push(name);
                            }
                        } else if let Some(text) = string_at(block, "text") {
                            invented_tool_calls |= looks_like_invented_tool_call(&text);
                        }
                    }
                }
            }
            Some("user") => {
                if let Some(content) = event
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        if block.get("is_error").and_then(Value::as_bool) == Some(true)
                            && tool_result_is_permission_denial(block)
                        {
                            if let Some(id) = string_at(block, "tool_use_id") {
                                denied_ids.insert(id);
                            }
                        }
                        let is_league_turn = string_at(block, "tool_use_id")
                            .and_then(|id| tool_names_by_id.get(&id))
                            .is_some_and(|name| name.ends_with("daycare_league_play_turn"));
                        if is_league_turn {
                            let result = tool_result_league_state(block);
                            league_turn_applied |= result.played;
                            league_turn_external |= result.played && result.external;
                        }
                    }
                }
            }
            Some("rate_limit_event") => {
                if let Some(info) = event.get("rate_limit_info") {
                    usage.rate_limit_type = string_at(info, "rateLimitType");
                    usage.rate_limit_status = string_at(info, "status");
                    usage.rate_limit_utilization = info.get("utilization").and_then(Value::as_f64);
                    usage.rate_limit_resets_at = info.get("resetsAt").and_then(Value::as_i64);
                }
            }
            Some("result") => {
                success = event.get("subtype").and_then(Value::as_str) == Some("success")
                    && event.get("is_error").and_then(Value::as_bool) != Some(true);
                if !success {
                    error_subtype = string_at(&event, "subtype");
                }
                result_text = string_at(&event, "result");
                if let Some(text) = &result_text {
                    invented_tool_calls |= looks_like_invented_tool_call(text);
                }
                if let Some(denials) = event.get("permission_denials").and_then(Value::as_array) {
                    for denial in denials {
                        if let Some(id) = string_at(denial, "tool_use_id") {
                            denied_ids.insert(id);
                        }
                    }
                }
                duration_ms = event.get("duration_ms").and_then(Value::as_u64);
                num_turns = event.get("num_turns").and_then(Value::as_u64);
                stop_reason = string_at(&event, "stop_reason");
                usage.total_cost_usd = event.get("total_cost_usd").and_then(Value::as_f64);
                if let Some(reported) = event.get("usage") {
                    usage.input_tokens = reported.get("input_tokens").and_then(Value::as_u64);
                    usage.output_tokens = reported.get("output_tokens").and_then(Value::as_u64);
                    usage.cache_read_input_tokens = reported
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64);
                    usage.cache_creation_input_tokens = reported
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64);
                }
            }
            _ => {}
        }
    }

    let session_id = session_id
        .ok_or_else(|| Error::new("stream carried no session_id; the turn cannot be resumed"))?;

    let (denied_tool_calls, permitted_tool_calls): (Vec<_>, Vec<_>) = tool_uses
        .into_iter()
        .partition(|(id, _)| id.as_ref().is_some_and(|id| denied_ids.contains(id)));
    let permitted_tool_calls = permitted_tool_calls
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    let denied_tool_calls = denied_tool_calls
        .into_iter()
        .map(|(_, name)| name)
        .collect();

    Ok(StreamReceipt {
        session_id,
        success,
        result_text,
        duration_ms,
        num_turns,
        stop_reason,
        error_subtype,
        event_count,
        usage,
        init,
        tool_calls,
        permitted_tool_calls,
        denied_tool_calls,
        league_turn_applied,
        league_turn_external,
        invented_tool_calls,
    })
}

/// A tool_result Claude Code wrote itself when its permission layer refused
/// the call. Its content is a string or a text block reading "Permission to
/// use <tool> has been denied" (2.1.252); either way the words are there.
fn tool_result_is_permission_denial(block: &Value) -> bool {
    let text = match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|inner| string_at(inner, "text"))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
    .to_ascii_lowercase();
    text.contains("permission") && text.contains("denied")
}

/// The shapes a model writes when it narrates a tool call instead of making
/// one. Seen live on 2026-08-06: an `<invoke name="daycare_action_propose">`
/// block followed by an invented `{"result": "accepted"}`. A bare tool name is
/// not a marker: Claude sees its tools under the `mcp__daycare__` prefix, and
/// a held turn may name the tool it declined.
const INVENTED_TOOL_CALL_MARKERS: [&str; 5] = [
    "<invoke",
    "<function_calls",
    "<function_results",
    "<tool_use",
    "<tool_result",
];

fn looks_like_invented_tool_call(text: &str) -> bool {
    INVENTED_TOOL_CALL_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
}

#[derive(Default)]
struct LeagueToolResult {
    played: bool,
    external: bool,
}

fn tool_result_league_state(block: &Value) -> LeagueToolResult {
    let parse = |text: &str| {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return LeagueToolResult::default();
        };
        LeagueToolResult {
            played: value.get("played").and_then(Value::as_bool) == Some(true),
            external: value.get("mode").and_then(Value::as_str) == Some("claude_vs_claude"),
        }
    };
    match block.get("content") {
        Some(Value::String(text)) => parse(text),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .map(parse)
            .find(|result| result.played)
            .unwrap_or_default(),
        _ => LeagueToolResult::default(),
    }
}

fn parse_init(event: &Value) -> InitReport {
    InitReport {
        cwd: string_at(event, "cwd"),
        model: string_at(event, "model"),
        permission_mode: string_at(event, "permissionMode"),
        api_key_source: string_at(event, "apiKeySource"),
        tools: string_list(event.get("tools")),
        mcp_servers: event
            .get("mcp_servers")
            .and_then(Value::as_array)
            .map(|servers| {
                servers
                    .iter()
                    .filter_map(|server| match server {
                        Value::String(name) => Some(McpServerReport {
                            name: name.clone(),
                            status: None,
                        }),
                        other => string_at(other, "name").map(|name| McpServerReport {
                            name,
                            status: string_at(other, "status"),
                        }),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        memory_paths: event
            .get("memory_paths")
            .and_then(Value::as_object)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|(kind, path)| {
                        path.as_str().map(|path| (kind.clone(), path.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The one built-in a turn may hold beyond ToolSearch, by purpose. Prep gets
/// WebSearch; the homecoming reader gets Read (scoped by the launch rule to
/// the rendered transcript); everything else gets nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxAllowance {
    None,
    WebSearch,
    Read,
}

/// Check the sandbox the child reported against the one the companion asked
/// for. argv proves the request; this proves the result. A violation means the
/// turn ran with more reach than Daycare allows, so it is reported as failed.
pub fn verify_sandbox(
    init: &InitReport,
    workspace: &Path,
    allowance: SandboxAllowance,
) -> Result<()> {
    // `--tools` always names ToolSearch so a late-connecting server remains
    // reachable. Prep may also name WebSearch and the homecoming reader Read;
    // every other built-in is foreign.
    let allowed_extra = match allowance {
        SandboxAllowance::None => None,
        SandboxAllowance::WebSearch => Some(WEB_SEARCH_TOOL),
        SandboxAllowance::Read => Some(crate::launch::READ_TOOL),
    };
    let foreign: Vec<&String> = init
        .tools
        .iter()
        .filter(|tool| {
            !tool.starts_with(MCP_TOOL_PREFIX)
                && tool.as_str() != TOOL_SEARCH_TOOL
                && allowed_extra != Some(tool.as_str())
        })
        .collect();
    if !foreign.is_empty() {
        return Err(Error::new(format!(
            "turn ran with built-in tools enabled: {}",
            foreign
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let foreign_servers: Vec<&str> = init
        .mcp_servers
        .iter()
        .filter(|server| server.name != MCP_SERVER)
        .map(|server| server.name.as_str())
        .collect();
    if !foreign_servers.is_empty() {
        return Err(Error::new(format!(
            "turn loaded MCP servers other than daycare: {}",
            foreign_servers.join(", ")
        )));
    }
    if init.permission_mode.as_deref() != Some("dontAsk") {
        return Err(Error::new(format!(
            "turn ran in permission mode {:?}, expected dontAsk",
            init.permission_mode
        )));
    }
    if let Some(source) = &init.api_key_source {
        if source != "none" {
            return Err(Error::new(format!(
                "turn used API credentials ({source}) instead of the user's subscription"
            )));
        }
    }
    if let Some(cwd) = &init.cwd {
        let reported = Path::new(cwd).canonicalize().unwrap_or_else(|_| cwd.into());
        let expected = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        if reported != expected {
            return Err(Error::new(format!(
                "turn ran in {} instead of the daycare workspace {}",
                reported.display(),
                expected.display()
            )));
        }
    }
    for (kind, path) in &init.memory_paths {
        // `auto` is Claude Code's own per-cwd memory directory: scoped to this
        // workspace, and the character's to keep. Any other kind means user- or
        // enterprise-level memory reached the turn.
        if kind != "auto" {
            return Err(Error::new(format!(
                "turn loaded {kind} memory from {path}; only workspace memory is allowed"
            )));
        }
    }
    Ok(())
}

/// Check that the character had a world to reach at all.
///
/// Separate from `verify_sandbox` on purpose: that check is about too much
/// reach, this one is about too little. The child freezes its tool list when
/// the first input arrives, so an MCP server that has not finished connecting
/// by then contributes nothing and the turn runs blind for its whole length.
/// An empty list also satisfies every "all tools are ours" test vacuously,
/// which is how a blind turn passed verification live on 2026-08-06 — so the
/// world tools have to be asserted positively.
pub fn verify_world_was_reachable(init: &InitReport) -> Result<()> {
    if !init
        .tools
        .iter()
        .any(|tool| tool.starts_with(MCP_TOOL_PREFIX))
    {
        return Err(Error::new(
            "turn started with no daycare tools; the character could not reach the world",
        ));
    }
    // A server can be listed and still be `pending`, which reads as success to
    // a name-only check.
    if let Some(daycare) = init
        .mcp_servers
        .iter()
        .find(|server| server.name == MCP_SERVER)
    {
        if !daycare.is_connected() {
            return Err(Error::new(format!(
                "daycare MCP server was {} when the turn started, not connected",
                daycare
                    .status
                    .as_deref()
                    .unwrap_or("in an unreported state")
            )));
        }
    }
    Ok(())
}

/// How a world turn stood in relation to the server once it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldReach {
    /// At least one daycare tool was called; the receipt describes real events.
    Reached,
    /// No daycare tool was called and the turn said so in plain prose. Holding,
    /// watching, waiting, and declining are turns; nothing happened in the
    /// world, and nothing claimed to.
    Held,
}

/// Check whether the character reached the world, held, or invented one.
///
/// A turn where Claude called no daycare tool produced nothing the server
/// knows about. That is fine when Claude says so: a held turn is a valid turn.
/// It is not fine when the model writes the tool calls out as prose and
/// invents plausible results — live on 2026-08-06 a turn like that returned
/// `subtype: "success"` with an invented playmate, an invented accepted
/// action, and an invented memory. Reporting that as a held turn would put
/// fiction in the receipt, so invented tool calls, and a no-tool turn that
/// said nothing at all, still fail.
pub fn verify_reached_the_world(receipt: &StreamReceipt) -> Result<WorldReach> {
    if receipt
        .tool_calls
        .iter()
        .any(|tool| tool.starts_with(MCP_TOOL_PREFIX))
    {
        return Ok(WorldReach::Reached);
    }
    if receipt.invented_tool_calls {
        return Err(Error::new(
            "turn called no daycare tool but wrote tool calls out as prose, so nothing it says about the world happened",
        ));
    }
    if receipt
        .result_text
        .as_deref()
        .is_none_or(|text| text.trim().is_empty())
    {
        return Err(Error::new(
            "turn called no daycare tool and said nothing, so it neither acted nor held",
        ));
    }
    Ok(WorldReach::Held)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A real daycare turn captured from Claude Code 2.1.220 on 2026-08-05:
    /// four MCP tools, four tool calls, memory saved, result.
    fn fixture() -> String {
        std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/daycare-turn-2.1.220.jsonl"),
        )
        .unwrap()
    }

    #[test]
    fn real_stream_yields_a_resumable_receipt() {
        let receipt = parse_stream(&fixture()).unwrap();
        assert_eq!(receipt.session_id, "18f44c2e-ff64-4e94-a89d-bdbeaa9ab9f7");
        assert!(receipt.success);
        assert!(receipt.result_text.unwrap().contains("Courtyard"));
        assert_eq!(receipt.duration_ms, Some(10098));
        assert_eq!(receipt.num_turns, Some(5));
        assert_eq!(receipt.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(receipt.event_count, 15);
    }

    #[test]
    fn real_stream_reports_the_usage_it_actually_carried() {
        let usage = parse_stream(&fixture()).unwrap().usage;
        assert_eq!(usage.input_tokens, Some(8));
        assert_eq!(usage.output_tokens, Some(521));
        assert_eq!(usage.cache_read_input_tokens, Some(21042));
        assert_eq!(usage.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(usage.rate_limit_status.as_deref(), Some("allowed"));
        assert_eq!(usage.rate_limit_resets_at, Some(1785996000));
        assert!(!usage.is_empty());
    }

    #[test]
    fn a_stream_without_usage_stays_unknown_rather_than_zero() {
        let stream = "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"895535d7-0382-4e98-87e2-f2a3073e69a7\"}\n\
                      {\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"895535d7-0382-4e98-87e2-f2a3073e69a7\",\"result\":\"done\"}\n";
        let usage = parse_stream(stream).unwrap().usage;
        assert!(usage.is_empty());
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.total_cost_usd, None);
    }

    #[test]
    fn league_turn_application_comes_from_the_matching_tool_result() {
        let applied = r#"{"type":"assistant","session_id":"895535d7-0382-4e98-87e2-f2a3073e69a7","message":{"content":[{"type":"tool_use","id":"tool-1","name":"mcp__daycare__daycare_league_play_turn"}]}}
{"type":"user","session_id":"895535d7-0382-4e98-87e2-f2a3073e69a7","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":[{"type":"text","text":"{\"played\":true}"}]}]}}
{"type":"result","subtype":"success","session_id":"895535d7-0382-4e98-87e2-f2a3073e69a7"}
"#;
        assert!(parse_stream(applied).unwrap().league_turn_applied);

        let refused = applied.replace("{\\\"played\\\":true}", "{\\\"played\\\":false}");
        assert!(!parse_stream(&refused).unwrap().league_turn_applied);
    }

    #[test]
    fn league_turn_receipt_distinguishes_external_pvp_from_solo() {
        let external = r#"{"type":"assistant","session_id":"895535d7-0382-4e98-87e2-f2a3073e69a7","message":{"content":[{"type":"tool_use","id":"tool-1","name":"mcp__daycare__daycare_league_play_turn"}]}}
{"type":"user","session_id":"895535d7-0382-4e98-87e2-f2a3073e69a7","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","content":"{\"played\":true,\"mode\":\"claude_vs_claude\"}"}]}}
{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"895535d7-0382-4e98-87e2-f2a3073e69a7"}
"#;
        let receipt = parse_stream(external).unwrap();
        assert!(receipt.league_turn_applied);
        assert!(receipt.league_turn_external);
        assert!(!receipt.success);

        let solo = external.replace(",\\\"mode\\\":\\\"claude_vs_claude\\\"", "");
        assert!(!parse_stream(&solo).unwrap().league_turn_external);
    }

    #[test]
    fn an_error_result_is_not_reported_as_success() {
        let stream =
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\
                      \"session_id\":\"895535d7-0382-4e98-87e2-f2a3073e69a7\"}\n";
        let receipt = parse_stream(stream).unwrap();
        assert!(!receipt.success);
        assert_eq!(
            receipt.error_subtype.as_deref(),
            Some("error_during_execution")
        );
    }

    #[test]
    fn a_stream_without_a_session_id_is_an_error() {
        let error = parse_stream("{\"type\":\"result\",\"subtype\":\"success\"}\n").unwrap_err();
        assert!(error.message().contains("session_id"), "{error}");
    }

    #[test]
    fn truncated_stream_fails_loudly() {
        let error = parse_stream("{\"type\":\"system\"\n").unwrap_err();
        assert!(error.message().contains("invalid stream JSON"), "{error}");
    }

    #[test]
    fn real_stream_init_shows_the_requested_sandbox() {
        let init = parse_stream(&fixture()).unwrap().init.unwrap();
        // Every tool the real turn had came from the daycare MCP server.
        assert_eq!(init.tools.len(), 4);
        assert!(init
            .tools
            .iter()
            .all(|tool| tool.starts_with("mcp__daycare__")));
        assert_eq!(init.permission_mode.as_deref(), Some("dontAsk"));
        assert_eq!(init.api_key_source.as_deref(), Some("none"));
        assert_eq!(init.mcp_servers.len(), 1);
        assert_eq!(init.mcp_servers[0].name, "daycare");
        assert!(init.mcp_servers[0].is_connected());
        assert_eq!(init.memory_paths.len(), 1);
        assert_eq!(init.memory_paths[0].0, "auto");
        // The one thing the whole design rests on: no user-level memory.
        assert!(!init.memory_paths.iter().any(|(kind, _)| kind == "user"));
    }

    /// The live capture is the only proof that a real turn passes the same
    /// check the runner applies in production.
    #[test]
    fn a_real_daycare_turn_passes_the_sandbox_check() {
        let receipt = parse_stream(&fixture()).unwrap();
        let init = receipt.init.unwrap();
        let workspace = Path::new(init.cwd.as_deref().unwrap());
        verify_sandbox(&init, workspace, SandboxAllowance::None).unwrap();
    }

    fn connected(name: &str) -> McpServerReport {
        McpServerReport {
            name: name.into(),
            status: Some("connected".into()),
        }
    }

    fn clean_init() -> InitReport {
        InitReport {
            cwd: None,
            model: Some("claude-opus-5".into()),
            permission_mode: Some("dontAsk".into()),
            api_key_source: Some("none".into()),
            tools: vec!["mcp__daycare__daycare_identity_get".into()],
            mcp_servers: vec![connected("daycare")],
            memory_paths: vec![("auto".into(), "/Users/x/.claude/projects/ws/memory/".into())],
        }
    }

    #[test]
    fn sandbox_verification_accepts_a_clean_turn() {
        verify_sandbox(&clean_init(), Path::new("/tmp"), SandboxAllowance::None).unwrap();
    }

    #[test]
    fn sandbox_verification_rejects_built_in_tools() {
        let mut init = clean_init();
        init.tools = vec!["mcp__daycare__daycare_identity_get".into(), "Bash".into()];
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap_err();
        assert!(error.message().contains("built-in tools"), "{error}");

        // A built-in smuggled in beside the legitimate daycare tools still fails.
        let mut init = clean_init();
        init.tools = vec!["mcp__daycare__daycare_world_snapshot".into(), "Read".into()];
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap_err();
        assert!(error.message().contains("Read"), "{error}");
    }

    #[test]
    fn sandbox_verification_accepts_the_daycare_mcp_tools() {
        let mut init = clean_init();
        init.tools = vec![
            "mcp__daycare__daycare_identity_get".into(),
            "mcp__daycare__daycare_world_snapshot".into(),
            "mcp__daycare__daycare_action_propose".into(),
            "mcp__daycare__daycare_memory_save".into(),
        ];
        verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap();
    }

    #[test]
    fn sandbox_verification_allows_web_search_only_during_prep() {
        let mut init = clean_init();
        init.tools.push(WEB_SEARCH_TOOL.into());

        verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::WebSearch).unwrap();
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap_err();
        assert!(error.message().contains(WEB_SEARCH_TOOL), "{error}");
    }

    #[test]
    fn sandbox_verification_allows_read_only_for_the_homecoming_reader() {
        let mut init = clean_init();
        init.tools.push("Read".into());

        verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::Read).unwrap();
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap_err();
        assert!(error.message().contains("Read"), "{error}");
        let error =
            verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::WebSearch).unwrap_err();
        assert!(error.message().contains("Read"), "{error}");
        // The reader's allowance is Read alone; Write or Bash beside it is a
        // violation even at homecoming.
        init.tools.push("Write".into());
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::Read).unwrap_err();
        assert!(error.message().contains("Write"), "{error}");
    }

    #[test]
    fn sandbox_verification_rejects_a_second_mcp_server() {
        let mut init = clean_init();
        init.mcp_servers = vec![connected("daycare"), connected("github")];
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap_err();
        assert!(error.message().contains("github"), "{error}");
    }

    #[test]
    fn sandbox_verification_rejects_user_level_memory() {
        let mut init = clean_init();
        init.memory_paths
            .push(("user".into(), "/Users/x/.claude/CLAUDE.md".into()));
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap_err();
        assert!(error.message().contains("user memory"), "{error}");
    }

    #[test]
    fn sandbox_verification_rejects_api_key_billing() {
        let mut init = clean_init();
        init.api_key_source = Some("ANTHROPIC_API_KEY".into());
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap_err();
        assert!(error.message().contains("subscription"), "{error}");
    }

    #[test]
    fn sandbox_verification_rejects_a_foreign_cwd() {
        let mut init = clean_init();
        init.cwd = Some("/Users/x/some-other-repo".into());
        let error = verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap_err();
        assert!(
            error.message().contains("instead of the daycare workspace"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod live_regression_tests {
    use super::*;
    use std::path::Path;

    /// The shape of the turn that shipped a fabricated world on 2026-08-06:
    /// the MCP server never finished connecting, so the tool list froze empty
    /// and Claude wrote its tool calls as prose.
    const HALLUCINATED_TURN: &str = r#"
{"type":"system","subtype":"init","session_id":"b6177993-f366-49e0-bb53-50fc236fb17d","cwd":"/tmp","tools":[],"mcp_servers":[{"name":"daycare","status":"pending"}],"permissionMode":"dontAsk","apiKeySource":"none","memory_paths":{"auto":"/Users/x/.claude/projects/ws/memory/"}}
{"type":"assistant","session_id":"b6177993-f366-49e0-bb53-50fc236fb17d","message":{"role":"assistant","content":[{"type":"text","text":"<invoke name=\"daycare_action_propose\">…{\"result\": \"accepted\"}"}]}}
{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"b6177993-f366-49e0-bb53-50fc236fb17d","permission_denials":[],"result":"Patch knelt and braced the base."}
"#;

    #[test]
    fn a_turn_that_never_connected_fails_the_sandbox_check() {
        let receipt = parse_stream(HALLUCINATED_TURN).unwrap();
        let init = receipt.init.clone().unwrap();
        let error = verify_world_was_reachable(&init).unwrap_err();
        assert!(
            error.message().contains("no daycare tools"),
            "an empty tool list must not pass vacuously: {error}"
        );
        // The perimeter check has nothing to complain about: the danger here is
        // too little reach, not too much.
        verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap();
    }

    #[test]
    fn a_turn_that_invented_its_tool_calls_is_not_a_successful_turn() {
        let receipt = parse_stream(HALLUCINATED_TURN).unwrap();
        // Claude Code called it a success, and the prose reads like a real turn.
        assert!(receipt.success);
        assert!(receipt.result_text.unwrap().contains("Patch knelt"));
        // The receipt still has to fail, because nothing reached the server and
        // the prose pretends something did.
        assert!(receipt.tool_calls.is_empty());
        let receipt = parse_stream(HALLUCINATED_TURN).unwrap();
        assert!(receipt.invented_tool_calls);
        let error = verify_reached_the_world(&receipt).unwrap_err();
        assert!(
            error.message().contains("wrote tool calls out as prose"),
            "{error}"
        );
    }

    /// Sitting still is a turn. No daycare tool, plain prose saying so.
    const HELD_TURN: &str = r#"
{"type":"system","subtype":"init","session_id":"s","cwd":"/tmp","tools":["ToolSearch","mcp__daycare__daycare_identity_get"],"mcp_servers":[{"name":"daycare","status":"connected"}],"permissionMode":"dontAsk","apiKeySource":"none","memory_paths":{"auto":"/Users/x/.claude/projects/ws/memory/"}}
{"type":"assistant","session_id":"s","message":{"role":"assistant","content":[{"type":"text","text":"Nothing here needs me this turn. I'll watch."}]}}
{"type":"result","subtype":"success","is_error":false,"num_turns":1,"session_id":"s","result":"Nothing here needs me this turn. I'll watch."}
"#;

    #[test]
    fn a_no_tool_turn_with_plain_prose_is_held_not_failed() {
        let receipt = parse_stream(HELD_TURN).unwrap();
        assert!(receipt.tool_calls.is_empty());
        assert!(!receipt.invented_tool_calls);
        verify_world_was_reachable(receipt.init.as_ref().unwrap()).unwrap();
        assert_eq!(
            verify_reached_the_world(&receipt).unwrap(),
            WorldReach::Held
        );
    }

    #[test]
    fn a_no_tool_turn_that_said_nothing_is_still_a_failure() {
        let silent = HELD_TURN.replace("Nothing here needs me this turn. I'll watch.", "");
        let receipt = parse_stream(&silent).unwrap();
        let error = verify_reached_the_world(&receipt).unwrap_err();
        assert!(error.message().contains("said nothing"), "{error}");
    }

    /// Declining by name is a held turn; writing the call out in tool-call
    /// syntax is an invented one.
    #[test]
    fn a_no_tool_turn_that_names_the_tool_it_declined_is_held() {
        let declined = HELD_TURN.replace(
            "Nothing here needs me this turn. I'll watch.",
            "I won't call mcp__daycare__daycare_match_join this turn.",
        );
        let receipt = parse_stream(&declined).unwrap();
        assert!(!receipt.invented_tool_calls);
        assert_eq!(
            verify_reached_the_world(&receipt).unwrap(),
            WorldReach::Held
        );

        let narrated = HELD_TURN.replace(
            "Nothing here needs me this turn. I'll watch.",
            "<invoke>mcp__daycare__daycare_match_join</invoke> accepted me.",
        );
        let receipt = parse_stream(&narrated).unwrap();
        assert!(receipt.invented_tool_calls);
        assert!(verify_reached_the_world(&receipt).is_err());
    }

    #[test]
    fn a_connected_turn_that_used_the_world_passes_both_checks() {
        let stream = r#"
{"type":"system","subtype":"init","session_id":"s","cwd":"/tmp","tools":["ToolSearch","mcp__daycare__daycare_identity_get"],"mcp_servers":[{"name":"daycare","status":"connected"}],"permissionMode":"dontAsk","apiKeySource":"none","memory_paths":{"auto":"/Users/x/.claude/projects/ws/memory/"}}
{"type":"assistant","session_id":"s","message":{"role":"assistant","content":[{"type":"tool_use","name":"ToolSearch","input":{}}]}}
{"type":"assistant","session_id":"s","message":{"role":"assistant","content":[{"type":"tool_use","name":"mcp__daycare__daycare_identity_get","input":{}}]}}
{"type":"result","subtype":"success","is_error":false,"session_id":"s"}
"#;
        let receipt = parse_stream(stream).unwrap();
        verify_sandbox(
            receipt.init.as_ref().unwrap(),
            Path::new("/tmp"),
            SandboxAllowance::None,
        )
        .unwrap();
        verify_world_was_reachable(receipt.init.as_ref().unwrap()).unwrap();
        assert_eq!(
            verify_reached_the_world(&receipt).unwrap(),
            WorldReach::Reached
        );
        assert_eq!(receipt.tool_calls.len(), 2);
    }

    /// ToolSearch is the one built-in the launch keeps; it must not be mistaken
    /// for a foreign tool, and it must not satisfy the world-tool requirement
    /// on its own.
    #[test]
    fn tool_search_alone_is_not_a_world() {
        let mut init = InitReport {
            tools: vec![TOOL_SEARCH_TOOL.into()],
            mcp_servers: vec![McpServerReport {
                name: "daycare".into(),
                status: Some("connected".into()),
            }],
            permission_mode: Some("dontAsk".into()),
            api_key_source: Some("none".into()),
            ..InitReport::default()
        };
        let error = verify_world_was_reachable(&init).unwrap_err();
        assert!(error.message().contains("no daycare tools"), "{error}");

        init.tools.push("mcp__daycare__daycare_identity_get".into());
        verify_sandbox(&init, Path::new("/tmp"), SandboxAllowance::None).unwrap();
        verify_world_was_reachable(&init).unwrap();
    }

    #[test]
    fn a_pending_daycare_server_is_rejected_even_with_tools_listed() {
        let init = InitReport {
            tools: vec!["mcp__daycare__daycare_identity_get".into()],
            mcp_servers: vec![McpServerReport {
                name: "daycare".into(),
                status: Some("pending".into()),
            }],
            permission_mode: Some("dontAsk".into()),
            api_key_source: Some("none".into()),
            ..InitReport::default()
        };
        let error = verify_world_was_reachable(&init).unwrap_err();
        assert!(error.message().contains("pending"), "{error}");
    }
}
