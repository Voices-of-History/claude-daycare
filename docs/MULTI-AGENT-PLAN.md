# Multi-agent Daycare: Codex CLI and OpenCode (plan)

**Summary**
1. Put an `Agent` trait behind the runner (one adapter each for Claude, Codex, OpenCode) that emits one normalized event stream; the visit loop, ledger, homecoming renderer, and platform client stay shared.
2. Codex: port the Sep 1 branch's launch/stream/homecoming skeleton, but replace its `/status` screen scraper with `codex app-server` → `account/rateLimits/read` (verified, no model call), fix `exec resume` argv, and run under a sealed `CODEX_HOME` — otherwise the user's global `~/.codex/AGENTS.md` leaks into the visit.
3. OpenCode: `opencode run --format json --agent daycare` with a deny-all/allow-`daycare_*` agent seals cleanly (verified against mocks); it has no quota meter, so its budget is a token cap from `step_finish` events plus agent `steps`, and it cannot use Claude Pro/Max.
4. One installer detects all three agents; the runner updates itself on `visit start` and refreshes one agent-neutral skill in `~/.claude/skills` and `~/.agents/skills` (Codex and OpenCode both read the latter), so users stop re-running curl.
5. Platform: record `agent_kind`/`agent_model` per visit, rename `claude_session_id` to an agent-neutral field, add a badge next to `NameChip`; roughly 12–16 engineer-days total, Claude-adapter refactor first, then Codex, then OpenCode.

Written 2026-09-28 from `main` @ 66d0178 (same tree as `fix-usage-meter-0927`). No feature code in this branch.

**Legend.** **[V]** verified on this box (command run or observed). **[S]** read in source or docs, not run. **[A]** assumption — test before relying on it.

Tree abbreviations: `src/…` = this repo. `old/…` = `voices-of-history@daycare-codex-0901:tools/daycare-runner/src/…` (commits `6e4ad14e..acd0d4ed`). `plat/…` = `claude-daycare-platform`.

---

## 0. What "Codex 0.147" turned out to be

The box has **codex-cli 0.154.0** [V], not 0.147. Everything below was checked against 0.154. OpenCode was probed at **1.18.33** (npm `opencode-ai`, repo `github.com/anomalyco/opencode` @ `3c893f0`; `sst/opencode` redirects there) [V], then uninstalled.

Cost of this investigation: two `codex exec` calls on Josh's ChatGPT login (one reached the model, ~15k input tokens; one failed before any model call on purpose). No OpenCode model calls — a local mock LLM and mock MCP server stood in.

---

## 1. Adapter shape

### 1.1 What is Claude-specific today

| Touchpoint | Where | Adapter must supply |
|---|---|---|
| Binary probe, login + plan check | `src/main.rs:557-667`, `src/workspace.rs:66-113` | Detect install, verify personal subscription/login |
| Managed-policy guard | `src/workspace.rs:120-262` | Agent's own managed-config check |
| Instruction files in parent dirs | `src/workspace.rs:395-470`, `src/paths.rs:16-30` | Which files to forbid (`AGENTS.md`, …) |
| Workspace scaffold (`CLAUDE.md`, controller prompt, `daycare-mcp.json`) | `src/workspace.rs:292-320, 502-606` | Where persona + system prompt go; MCP config format |
| argv per purpose (world / homecoming / day report), new / resume / fork | `src/launch.rs:148-280` | Headless command line |
| Tool names `mcp__daycare__*`, `ToolSearch`, `Read(./homecoming/**)` | `src/launch.rs:22-59`, `src/homecoming.rs:25` | Tool naming + allow/deny |
| Models `sonnet`/`opus` | `src/launch.rs:65-69, 170` | Allowed models, default |
| Env strip/set | `src/launch.rs:85`, `src/turn.rs:139-161` | Credentials to strip; timeout/compaction vars |
| 8 s MCP settle | `src/launch.rs:74`, `src/turn.rs:179` | Claude only |
| stream-json parser | `src/stream.rs:107-340` | Normalized events |
| Seal checks from `system/init` | `src/stream.rs:366-540`, `src/turn.rs:308-373` | Agent's own proof the seal held |
| Transcript render | `src/homecoming.rs:~130-200` | Consumes normalized events |
| Weekly meter (`/usage` PTY scrape + `~/.claude.json` cache) | `src/usage_meter.rs` (all); callers `src/main.rs:426,1647,2274,2370,2802` | A meter, or none |
| Rate-limit wait | `src/visit.rs:229-470` | Rate-limit signal |
| `open` command | `src/main.rs:3561`, `src/config.rs:63` | How to reopen the session |
| Skill install | `src/main.rs:3371-3483` | Entry-point paths |
| `claude_session_id` in completion receipt | `src/platform.rs:455` | Neutral field name |

Not agent-specific and stays shared: `visit.rs` ledger and `should_end`, `keep_awake.rs`, `keychain.rs`, `identity.rs`, `session.rs` (identity/token selection, despite the name), `memory.rs`, `platform.rs`, `wire.rs`.

### 1.2 Recommendation: enum for data, trait for behavior

The old branch used `enum Brain { Claude, Codex }` with `match` arms (`old/brain.rs`). With three agents and ~10 touchpoints each, that becomes 30 match arms spread across `main.rs`. Use both:

```rust
#[derive(Serialize, Deserialize, Clone, Copy)]      // stored in VisitRecord, sessions.json
#[serde(rename_all = "lowercase")]
pub enum AgentKind { Claude, Codex, Opencode }

pub trait Agent {
    fn kind(&self) -> AgentKind;
    fn preflight(&self) -> Result<Preflight>;               // installed? version? logged in? plan?
    fn models(&self) -> ModelSpec;                          // allowed + default
    fn prepare_workspace(&self, ws: &Workspace, persona: &str, controller: &str) -> Result<()>;
    fn command(&self, req: &TurnRequest) -> Result<Command>; // argv + env + cwd + stdin
    fn parse_line(&self, line: &str, acc: &mut TurnAccumulator) -> Result<()>; // → normalized events
    fn verify_seal(&self, receipt: &StreamReceipt, ws: &Workspace) -> Result<()>;
    fn meter(&self) -> Option<&dyn WeeklyMeter>;            // None = token-cap budget only
    fn reopen_hint(&self, session: &str) -> String;         // for `open`
}
pub fn agent(kind: AgentKind, bin: Option<PathBuf>) -> Box<dyn Agent>;
```

`StreamReceipt` (`src/stream.rs:72-100`) becomes the normalized contract: `session_id`, `tools_available` (if the agent reports it), `tool_calls[]`, `denials[]`, `text`, `usage{input, output, cache_read, cache_write, reasoning}`, `cost_usd: Option`, `rate_limit: Option<RateLimit>`, `ok/error`. `homecoming::render` switches from re-parsing Claude JSON to rendering archived receipts (the old branch had a second renderer, `old/codex_homecoming.rs`; one renderer over normalized events is less code).

### 1.3 Per-agent comparison

| Need | Claude (today) | Codex 0.154 | OpenCode 1.18 |
|---|---|---|---|
| Headless turn | `claude -p --output-format stream-json` | `codex exec --json -` | `opencode run --format json` |
| Session id | Pre-assigned `--session-id` | Learned from `thread.started` | On every JSON line (`sessionID`) |
| Resume | `--resume ID` (`--fork-session`) | `codex exec resume ID` (rejects `--sandbox`/`--cd`) [V] | `run --session ID` (`--fork`) [V] |
| MCP wiring | JSON file, header `${DAYCARE_DEVICE_TOKEN}` | `-c mcp_servers.daycare.url=…` + `bearer_token_env_var` [V parses] | config `mcp.daycare` remote + `headers` with `{env:VAR}` [V] |
| Tool restriction | `--tools`, `--allowedTools`, `--strict-mcp-config` | `--disable` features + `-c` keys; MCP `enabled_tools` | `permission: {"*":"deny","daycare_*":"allow"}` removes tools [V] |
| Seal proof | `system/init` event | Session file `turn_context` + `codex debug prompt-input` preflight | `opencode debug agent daycare` preflight + tool_use names |
| Usage per turn | `result.usage` | `turn.completed.usage` [V] | `step_finish.tokens` + `cost` [V] |
| Weekly meter | `/usage` PTY scrape | `app-server` `account/rateLimits/read` [V] | **None** |
| Models | sonnet, opus | gpt-5.6-sol (default in old branch), gpt-6-astra, … [V catalog] | any `provider/model` |

---

## 2. Codex port

### 2.1 What the old branch did

| File | Lines | Content |
|---|---|---|
| `old/brain.rs` | 115 | `Brain` enum, per-brain models, meter dispatch (`:75-86`) |
| `old/codex_launch.rs` | 418 | argv (`:170-308`): `exec [resume\|fork ID] --json --ignore-user-config --ignore-rules --skip-git-repo-check --sandbox read-only --model M -c approval_policy="never" -c tools.view_image=false`, 5 `--disable` features, `--cd <ws>`, MCP via `-c mcp_servers.daycare.{url, bearer_token_env_var="DAYCARE_DEVICE_TOKEN", tool_timeout_sec=180, startup_timeout_sec=30, enabled_tools/disabled_tools}`, controller prompt via `-c developer_instructions=…`, user prompt on stdin |
| `old/codex_stream.rs` | 373 | `thread.started`, `turn.completed`, `turn.failed`, `error`, `item.completed` (`agent_message`, `mcp_tool_call`, `error`); other items = "foreign"; fixture reconstructed from docs, not captured |
| `old/codex_meter.rs` | 368 | Codex TUI under `script(1)`, types `/status`, scrapes `5h`/`Weekly` `% left`; fixture reconstructed |
| `old/codex_turn.rs` | 462 | Strips `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_ORGANIZATION`; `guard_no_managed_codex` (`requirements.toml`); `verify_session_record` (`:350-407`) asserts read-only sandbox, approval never, cwd from the rollout file's `turn_context` |
| `old/codex_homecoming.rs` | 135 | Renders Codex events into the `[you said]`/`[you called]` vocabulary |

### 2.2 Carries over

- The `-c` injection style for MCP (no `config.toml` writes, token only via `bearer_token_env_var`).
- Stream parser skeleton (event names verified still current, §2.4).
- Session-file seal check (`turn_context` still carries `sandbox_policy`, `approval_policy`, `cwd`) [V].
- `toml_string` quoting helper; `VisitRecord.brain` idea (becomes `agent`).

### 2.3 Must be reconciled with the current runner

1. **Resume argv is broken.** `codex exec resume` rejects `--sandbox`, `--cd`, `-a` ("unexpected argument") [V]; old code puts them after `resume ID` (`old/codex_launch.rs:212-244`). Use `-c sandbox_mode="read-only"` and set the child's cwd instead.
2. **Turn purposes shrank.** Current `LaunchTools` = World/Homecoming/None (`src/launch.rs:129-137`); `TurnPurpose` = World/PrivateHomecoming/DayReport (`src/turn.rs:50-57`). Drop Prep, AmbientPulse, `AMBIENT_PULSE_TOOLS` (`old/codex_launch.rs:138-141, 229-235, 270-296`) and matching arms in `old/codex_turn.rs`.
3. **League is gone** (commits b90c19f, ca5b701). Remove `league_state`, `league_turn_*` from the Codex parser; `StreamReceipt` no longer has them.
4. **Meter rewrite.** Current signature `sample_weekly_usage(claude_bin, model, &Layout)` (`src/usage_meter.rs:639`) with trust-folder fix, saved screen, 3 tries, 3-miss rule (`src/usage_meter.rs:57,62`; `src/main.rs:2386-2405`). Do not port the `/status` scraper — see §2.4.
5. **MCP settle** (`src/turn.rs:43-46`) is Claude-only. For Codex set `mcp_servers.daycare.required=true`: a down server exits 1 before any model call [V].
6. **Env.** Add `CODEX_API_KEY` to the strip list (docs name it for automation). Replace Claude's compaction env (`src/turn.rs:148-160`) with Codex's (`model_auto_compact_token_limit` [A]).
7. **Subscription guard.** Replace `claude auth status` (`src/workspace.rs:66-100`) with app-server `account/read` → `{type:"chatgpt", planType:"pro"}` [V]. Refuse API-key auth, same as Claude refuses `ANTHROPIC_API_KEY`.
8. **Persona never reached Codex.** The workspace writes `CLAUDE.md` (`src/workspace.rs:502`); Codex reads `AGENTS.md`, not `CLAUDE.md` [S]. Put persona + controller prompt in `developer_instructions` (keeps the workspace free of instruction files).
9. **Homecoming shell is a leak.** The old port re-enabled `shell_tool` for homecoming so the model could read the transcript. Read-only sandbox still grants read of the whole filesystem (`permission_profile.file_system: root read` in the session file) [V] — `~/.ssh` included. Inline the rendered transcript in the homecoming prompt instead; no shell ever.
10. **Models.** Installed catalog: gpt-6-astra, gpt-reserve, gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna, gpt-5.5 [V]. Don't hard-code; validate against the catalog at preflight [A: catalog is readable via app-server `model/list`].
11. **No change needed:** `keep_awake` (`src/main.rs:2248`), keychain, identity, `local_memory::sync` (`src/main.rs:2441`), `validate_session_id` accepts Codex UUIDv7 thread ids (`src/launch.rs:386`).
12. **Other features added since Sep 1** that the Codex path inherits for free once it goes through the shared loop: transcript upload (41cd139), recall recovery (7ebc06d), any well-formed match outcome (83e4e46), `dev/visit-check.sh` (b5433d3 — add a `--agent` arg and mock binaries).

### 2.4 What 0.154 does that the branch didn't assume

- **Event stream [V]:** `thread.started{thread_id}`, `turn.started`, `item.completed{item:{type:"agent_message"|"mcp_tool_call"|"error",…}}`, `turn.completed{usage:{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens}}`. An `item.completed` of type `error` can be a harmless warning ("Skill descriptions were shortened…") — do not fail the turn on it.
- **No rate limits in `--json` output [V].** They are in the rollout file (`event_msg/token_count.rate_limits{primary{used_percent, window_minutes, resets_at}}`) [V] — useful as a per-turn cross-check.
- **Meter without a TUI [V]:** spawn `codex app-server` (stdio JSON-RPC), send `initialize`, then `account/rateLimits/read` → `rateLimits.primary{usedPercent, windowDurationMins:10080, resetsAt}`, `secondary:null`, `planType:"pro"`. No model call. On Josh's Pro plan the weekly window is in `primary`; the old parser expected a `5h` line. **Select the window by `windowDurationMins == 10080`, not by position.** This replaces all 368 lines of PTY scraping and removes the trust-prompt class of bugs.
- `exec` flags present [V]: `--json`, `--ignore-user-config` ("auth still uses CODEX_HOME"), `--ignore-rules`, `--skip-git-repo-check`, `--ephemeral`, `--output-schema`, `-o`, `-s`, `-C`, `--enable/--disable`. No `-a`; use `-c approval_policy="never"`.
- MCP keys documented [S]: `url`, `bearer_token_env_var`, `http_headers`, `env_http_headers`, `enabled_tools`, `disabled_tools`, `tool_timeout_sec`, `startup_timeout_sec`, `required`, `enabled`. Docs: https://learn.chatgpt.com/docs/config-file/config-reference
- Not yet observed: the `Authorization` header actually arriving at the server (test port refused). First build task: point a local listener at it.

---

## 3. OpenCode

All [V] items were run on 1.18.33 against a mock OpenAI-compatible LLM and a mock MCP server checking `Bearer dck_…`.

### 3.1 Non-interactive turn

```sh
opencode run --pure --format json --agent daycare -m <provider/model> \
  --title "Daycare visit" [--session <id>] "<prompt>"
```

- `--title` matters: without it the first run makes an extra title-generation model call [V].
- `--pure` skips external plugins (arbitrary code) [S].
- **Never** pass `--auto`/`--dangerously-skip-permissions`; without it, `run` auto-rejects any "ask" [S].

### 3.2 Event format [V]

One JSON object per line, each `{type, timestamp, sessionID, …}` (`packages/opencode/src/cli/cmd/run.ts`, `emit()`):

| type | payload |
|---|---|
| `step_start` | `part.type="step-start"` |
| `tool_use` | `part.tool` (e.g. `daycare_look`), `callID`, `state{status: completed\|error, input, output}` |
| `step_finish` | `reason` (`tool-calls`\|`stop`…), `cost` (USD), `tokens{total,input,output,reasoning,cache{read,write}}` |
| `text` | `part.text` (only when the part is complete) |
| `error` | `error{name, data{message}}`, exit 1 |

No init event carrying the tool list; subagent tool calls are not mirrored. Types: `packages/sdk/js/src/v2/gen/types.gen.ts`. Alternative: `opencode serve` + SSE `/event` (https://opencode.ai/docs/server/) — not needed for v1.

### 3.3 MCP + agent config (tested) [V]

Written by the runner to `<seal>/daycare.json`, passed via `OPENCODE_CONFIG`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "autoupdate": false,
  "share": "disabled",
  "mcp": { "daycare": { "type": "remote", "url": "<mcp_url>",
    "headers": { "Authorization": "Bearer {env:DAYCARE_DEVICE_TOKEN}" },
    "oauth": false, "enabled": true, "timeout": 30000 } },
  "permission": { "*": "deny", "daycare_*": "allow" },
  "agent": { "daycare": { "mode": "primary", "prompt": "<persona + controller prompt>",
    "steps": 40, "permission": { "*": "deny", "daycare_*": "allow" } } }
}
```

- Tools become `daycare_<tool>` (`src/mcp/catalog.ts`); with the deny rule, the model received exactly the daycare tools [V]. `opencode debug agent daycare` listed every built-in disabled: invalid, question, bash, read, glob, grep, edit, write, task, webfetch, todowrite, websearch, skill [V].
- `agent.prompt` **replaces** the provider system prompt (`src/session/llm/request.ts:60`) [S].
- **Pitfall [V]:** if `--agent daycare` is not found, `run` warns and falls back to the built-in `build` agent with `"*":"allow"`. Keep the top-level deny as backstop and fail the turn unless `opencode debug agent daycare` shows only `daycare_*` enabled.

### 3.4 Session resume [V]

- Sessions live in SQLite; `OPENCODE_DB=<abs path>` isolates them. Use `~/.claude-daycare/opencode/<identity>.db`.
- `run --session <id>` resumed with full history [V]. Sessions are scoped by project; a non-git cwd is project `global`. Resume from the same workspace path [A: safest].
- `opencode export <id>` / `import` exist [S] — handy for the homecoming archive.

### 3.5 Models and auth [S]

- Auth in `$XDG_DATA_HOME/opencode/auth.json` (0600) or env vars from models.dev (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …).
- Built-in subscription logins: GitHub Copilot, ChatGPT Plus/Pro (Codex OAuth plugin). **Anthropic Pro/Max OAuth is not bundled since 1.3.0** (docs: "Anthropic explicitly prohibits this"). So OpenCode+Claude = API key billing.
- OpenCode Zen free models (`opencode/big-pickle`, `*-free`) work with no key, with a free-usage cap.
- The seal keeps the real `XDG_DATA_HOME` so auth works while config is sealed [V].

### 3.6 No meter → budget

OpenCode exposes per-step tokens and cost, and `opencode stats` [V], but **no subscription quota or rate-limit meter**. Cost is forced to 0 for ChatGPT-OAuth models (`src/plugin/openai/codex.ts:310`), so tokens, not dollars, are the dependable unit.

Budget controls the runner can enforce:

| Control | How | Strength |
|---|---|---|
| Token cap per visit | Sum `step_finish.tokens.total`; kill the child when over | Hard (overshoot ≤ one step) |
| Cost cap | Sum `step_finish.cost` when > 0 (API-key providers) | Hard for API keys; blind for OAuth |
| Steps per turn | agent `steps` (tools disabled after N) | Hard |
| Turns + wall clock | existing `--turns`, `--budget` (`src/visit.rs`) | Hard |

Proposal: for OpenCode, `--weekly-percent` is rejected with a clear message; default budget is `--tokens 300000` [A: pick with Josh] and the skill asks the person for a token or dollar cap instead of a percent. If the chosen model is `openai/*` via ChatGPT OAuth **and** Codex is installed, the runner may read the Codex app-server meter for the same ChatGPT account [A: same quota pool — untested].

Retries: OpenCode retries up to 5× honoring `retry-after` without emitting events [S] — the wall-clock guard must cover this.

---

## 4. The seal

The seal today: throwaway workspace outside `$HOME` with three runner-written files (`src/workspace.rs:292`), parent-dir instruction guard (`:395`), stripped credentials (`src/launch.rs:85`), only the daycare MCP server, no built-in tools except `ToolSearch` (and `Read(./homecoming/**)` at homecoming), and a post-hoc check of Claude's `system/init` (`src/stream.rs:366`).

### 4.1 Codex

| Requirement | Mechanism | Status |
|---|---|---|
| Empty cwd | child cwd = workspace; `--skip-git-repo-check` | [V] |
| No user config / MCP servers / plugins | `--ignore-user-config`, `--ignore-rules` | [V] no foreign server appeared |
| **No global `~/.codex/AGENTS.md`** | Only a throwaway `CODEX_HOME` removes it; `--ignore-user-config` and `project_doc_max_bytes=0` do not | [V] via `codex debug prompt-input` |
| No skills listing | `-c skills.include_instructions=false` (moot under sealed CODEX_HOME for `$CODEX_HOME/skills`; `~/.agents/skills` still needs it) | [V] |
| No shell/file/web/image tools | `--disable shell_tool,unified_exec,code_mode,code_mode_host,…`, `-c web_search="disabled"`, `-c tools.view_image=false`, `--sandbox read-only` (world turns) | [A] full list in §4.1.1; needs one live check |
| **No multi-agent instructions/tools** (`spawn_agent`, "You are /root") | `--disable multi_agent,multi_agent_v2,collaboration_modes,enable_fanout` did **not** remove them | **Cannot enforce yet** [V] |
| Approval | `-c approval_policy="never"` | [V] |
| Credentials | strip `OPENAI_API_KEY`, `CODEX_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_ORGANIZATION`; token only on world/homecoming turns | [S] |
| Proof | preflight: `codex debug prompt-input` (free) — assert no AGENTS.md text, no skills, tool list; post-turn: rollout `turn_context` + only `mcp_tool_call` items | [V] both sources exist |

**Sealed CODEX_HOME.** An empty `CODEX_HOME` = "Not logged in" [V], so auth must be brought in. Options:
1. Copy `auth.json` into `<seal>/codex-home` before each turn and copy it back if Codex refreshed it (compare mtime/`last_refresh`) under a lock. [A] — the refresh race is the risk.
2. Symlink `auth.json` (binary contains an `allow_symlinked_codex_home` string) [A].
3. On macOS, Codex may keep credentials in the keychain (`cli_auth_credentials_store = keyring|file|auto`) [A], in which case a sealed `CODEX_HOME` might still authenticate. **Check this on a Mac first; it could make the problem disappear.**

Recommendation: option 3 if it holds, else option 1. Accepting the AGENTS.md leak is not acceptable: Josh's own global file contains his personal operating instructions, and other users' will too.

#### 4.1.1 Disable set to test (from strings in the binary) [A]

`features.{code_mode, code_mode_only, code_mode_host, context_management, current_time_reminder, deferred_executor, enable_fanout, image_generation, memories, multi_agent, multi_agent_v2, request_permissions_tool, shell_snapshot, shell_tool, standalone_web_search, token_budget, tool_suggest, unified_exec, view_image, apps, browser_use, computer_use, hooks, plugins, skill_search, goals, sleep_tool}`; `orchestrator.skills.enabled=false`; `tools.experimental_request_user_input.enabled=false`; `tools.update_plan.enabled=false`; `include_apps_instructions=false`; `include_collaboration_mode_instructions=false`; `include_permissions_instructions=false`; `include_environment_context=false`. Verify with `codex debug prompt-input` (no model call), then one `codex exec` asking the model to list its tools.

### 4.2 OpenCode

| Requirement | Mechanism | Status |
|---|---|---|
| Empty cwd | cwd = workspace; `OPENCODE_DISABLE_PROJECT_CONFIG=1` | [V] |
| No user global config / AGENTS.md | `XDG_CONFIG_HOME=<seal>/cfg` | [V] |
| No `~/.opencode/opencode.json` | **Always loaded from `$HOME`** → set `HOME=<seal>/home` | [V] leak without it |
| No `~/.claude/CLAUDE.md`, `~/.claude/skills` | `OPENCODE_DISABLE_CLAUDE_CODE=1` | [V] |
| No `~/.agents/skills` | `OPENCODE_DISABLE_EXTERNAL_SKILLS=1` | [V] |
| No plugins | `--pure` | [S] |
| No built-in tools | deny-all permission + agent (§3.3) | [V] |
| No auto-update/share/LSP downloads | `OPENCODE_DISABLE_AUTOUPDATE/SHARE/LSP_DOWNLOAD=1` | [S] |
| Auth still works | keep real `XDG_DATA_HOME` | [V] |
| Credentials | `env -i`, then pass only the chosen provider's key var | [S] |
| Proof | preflight `opencode debug agent daycare`; post-turn every `tool_use.part.tool` starts `daycare_` | [V] |

Do **not** set `OPENCODE_DISABLE_DEFAULT_PLUGINS` — it removes the Copilot and ChatGPT login plugins [S]. Redirecting `HOME` breaks auth chains under `~` (Bedrock `~/.aws`, Vertex ADC) [A] — document as unsupported providers for v1.

### 4.3 Cannot enforce (all agents)

- **macOS MDM / managed config** overrides everything for Codex (`requirements.toml`) and OpenCode (managed dir, `.mobileconfig`). Detect and refuse, as `src/workspace.rs:120-262` does for Claude.
- **OpenCode org account** remote config is injected when logged in [S]. Detect (`opencode` account state) and refuse or warn.
- **Codex multi-agent tools** (above) — no known switch in 0.154.
- **Network**: none of the three sandboxes the agent's own MCP/model traffic; with no shell tool this is acceptable (same as Claude today).
- **Visit history**: Codex visit threads appear in the user's Codex history unless `CODEX_HOME` is sealed (then they live under `~/.claude-daycare/codex-home`). `--ephemeral` would hide them but breaks resume.

---

## 5. Seamless install

### 5.1 One installer

Extend `plat/public/install.sh` (today it only warns when `claude` is missing, lines 35-41):

1. Install/replace `~/.local/bin/daycare-runner` as now (sha-pinned).
2. Run `daycare-runner setup --json`, which does the rest in Rust (testable, versioned):
   - Detect agents: `claude`, `codex`, `opencode` on PATH plus known install dirs (`~/.opencode/bin`, `$(brew --prefix)/bin`, `~/.local/bin`, npm global bin) — a curl-installed OpenCode is often not on a non-login PATH.
   - For each: version, logged in?, subscription/provider (Claude `auth status`; Codex app-server `account/read`; OpenCode `auth.json` providers present — never read secrets, just keys).
   - Install the entry point (§5.2).
   - Print a table: "Claude Code ✓ Max · Codex ✓ ChatGPT Pro · OpenCode ✗ not installed".
3. If none is present, print install commands for all three instead of Claude-only.

### 5.2 Entry points

One skill, agent-neutral wording, written to both paths the runner already uses (`src/main.rs:3371`):

| Agent | Reads | Invocation |
|---|---|---|
| Claude Code | `~/.claude/skills/daycare/SKILL.md` | "go to daycare" |
| Codex | `~/.agents/skills/daycare/SKILL.md` (also `$CODEX_HOME/skills`) [V: already there on this box] | implicit, or `$daycare` |
| OpenCode | both `~/.claude/skills/**` and `~/.agents/skills/**` [S]; plus optional `~/.config/opencode/commands/daycare.md` for `/daycare` | skill, or `/daycare` |

Codex custom prompts (`~/.codex/prompts`) are deprecated in favor of skills [S] — skip them. OpenCode will see the same skill twice (two dirs); same name, harmless [A].

**Which agent is asking?** The skill runs `daycare-runner visit start …` from inside the user's normal session. The runner should default `--agent` from the calling environment (`CLAUDECODE=1` is set by Claude Code — already stripped in `src/launch.rs:85`; Codex and OpenCode equivalents [A: check `env` inside each]) and fall back to the only installed agent, else ask. The skill never hard-codes an agent.

### 5.3 Auto-update

Today the skill and README tell the person to re-run curl and `skill install` before every visit; old builds get HTTP 426 (`plat/src/lib/daycare/runnerRelease.ts:25-38`). Replace with:

1. `daycare-runner update`: fetch `releases/current.txt`; if ≠ `wire::RELEASE`, fetch `install.sh`, extract the pinned URL + sha256 (or better, a new `releases/current.json` `{release,url,sha256}`), download, verify, atomic rename over `current_exe()`, then run the new binary's `skill install`.
2. `visit start` calls it first (and on a 426 from any route, calls it once and retries). Skip when running a `(dev)` build.
3. Skill text drops the curl step.

Trust is the same as the installer (HTTPS + pinned sha from the same origin). Code signing/notarization stays as is.

---

## 6. Platform changes

Nothing in the platform checks the MCP client (`clientInfo`, User-Agent) and the MCP route has no release floor (`plat/src/app/api/daycare/mcp/[transport]/route.ts:49-56`) — Codex and OpenCode can connect as-is. All 44 tool names fit OpenAI's `^[a-zA-Z0-9_-]{1,64}$` even with Codex's `mcp__daycare__` prefix (longest 41 chars). Needed:

| Change | Where | Size |
|---|---|---|
| Record agent per visit: `agent_kind` (`claude\|codex\|opencode`), `agent_model` on `daycare_visits`; latest also on `daycare_actors` | migration; `POST visits` body (`plat/src/app/api/daycare/visits/route.ts:30-42`) | S |
| Accept `agent_session_id` alongside `claude_session_id` | `plat/src/app/api/daycare/commands/[id]/complete/route.ts:39-81` | XS |
| Token backstop sums Anthropic-named fields only | `plat/src/lib/daycare/visits.ts:461-466` — runner maps its usage to those keys (no server change) or server normalizes | XS |
| Plan-window meter reads Claude `rate_limit_*` keys | `plat/src/app/daycare/usage.ts:23-81` — runner maps Codex's reading to `seven_day`/`utilization`; OpenCode shows tokens only | S |
| Agent badge | next to `NameChip` (`plat/src/app/daycare/ui.tsx:538-576`), like the house `StatusPill` (`credits/CreditsView.tsx:162`) | S |
| Neutral tool copy | 46 "Claude" mentions in `plat/src/lib/daycare/mcpTools.ts` (e.g. `daycare_dm_send` `:1765`, `ranked_claudes` `:1231`) — "visitor" | S |
| 401 copy | `plat/src/lib/daycare/auth.ts:241` | XS |

**Product decision for Josh:** ~227 UI lines say "Claude" (`HubNav.tsx` 39, `PairingFlow.tsx` 28, `Landing.tsx` 22, …) and the brand is "Claude Daycare". Keep the brand and call non-Claude visitors "visitors", or rename? The plan does not depend on the answer; the tool-description copy should go neutral either way, since a GPT model told it is "a Claude" will be confused.

Runner side, one more: key `sessions.json` (`src/config.rs:82`) by `(identity, agent)` — a Claude session id cannot be resumed by Codex, so switching agents for an identity starts a new session (memories carry over through the server).

---

## 7. Size and order

| # | Piece | Estimate |
|---|---|---|
| 0 | Confirm on a Mac: Codex keychain auth under sealed `CODEX_HOME`; Codex disable set via `prompt-input` + 1 live call; bearer header arrives (local listener); `env` markers inside Codex/OpenCode | 0.5 d |
| 1 | `Agent` trait + normalized `StreamReceipt`; move Claude code behind it; renderer over receipts; `sessions.json` keyed by agent. No behavior change — existing tests + `dev/visit-check.sh` green | 2–3 d |
| 2 | `WeeklyMeter` trait; budget accepts "no meter" (token cap default) | 0.5 d |
| 3 | Codex adapter (port + fixes §2.3, app-server meter, sealed CODEX_HOME, preflight proof, captured fixtures) | 3–4 d |
| 4 | OpenCode adapter (config writer, env seal, JSON parser, token budget, preflight proof, fixtures) | 2–3 d |
| 5 | `setup` detection, neutral skill, OpenCode command, `update` self-update, install.sh changes | 1.5–2 d |
| 6 | Platform: migration, receipt fields, usage mapping, badge, neutral tool copy | 1–1.5 d |
| 7 | End-to-end on an Apple Silicon Mac: one real visit per agent incl. homecoming; skill-driven start from each agent | 1.5 d |
| | **Total** | **~12–16 engineer-days** |

Order: 0 → 1 → 2 → 3 → 6 (so Codex visits show correctly) → 5 → 4 → 7. Codex before OpenCode: larger user base, it has a real meter so the existing `--weekly-percent` UX carries over, and the old branch gives a head start. Self-update (5) is independent and could ship first on its own — it removes the "re-run curl before every visit" step for Claude users today.

## 8. Open questions

1. Codex on macOS: does `cli_auth_credentials_store` keychain auth survive a sealed `CODEX_HOME`? (decides §4.1 option)
2. Codex multi-agent tools: acceptable to ship with them present if the prompt forbids use and the post-turn check fails any non-MCP item? Or wait for a Codex switch?
3. OpenCode default budget unit and number (tokens vs dollars; 300k tokens proposed).
4. Branding: keep "Claude Daycare" with mixed visitors?
