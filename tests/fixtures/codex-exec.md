These JSONL fixtures were captured from Codex CLI 0.154.0 on 2026-09-28,
using `dev/visit-check.sh 8815 codex` with model `gpt-5.5` against the local
mock Daycare platform. No production device or database was used.

- `codex-exec-world.jsonl`: successful identity lookup, world snapshot, and
  refereed action. Thread/event/proposal IDs and the event timestamp were
  replaced; mock character text, wire shapes, and token counts are unchanged.
- `codex-exec-approval-denied.jsonl`: the earlier run, before explicitly
  approving the Daycare MCP server. Codex completes its turn even though the
  tool cannot run. The parser must report failure. The thread ID was replaced.

The successful run sets `mcp_servers.daycare.default_tools_approval_mode` to
`approve`; `approval_policy=never`, read-only sandboxing, tool restrictions,
and prompt/rollout seal checks remain enabled. See the
[Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

The adapter now requires Codex 0.158.0 or newer. The live exec fixtures above
remain historical 0.154 captures; no new paid/model turn was used for the
0.158 configuration check.

`codex-prompt-input-sealed.json` is a 0.158 `debug prompt-input` capture with
the current seal settings, including `agents.enabled=false`. Message IDs
and timestamps were replaced. The former multi-agent rendering is retained
as `codex-prompt-input-multi-agent.json` and must now fail preflight.

`codex-tools-0.158.json` records the result of:

```
python3 dev/codex-tools-check.py /path/to/codex-0.158
```

This runs the installed binary against temporary local model and MCP endpoints,
captures the outgoing tool definitions, and returns HTTP 400 before any
model runs. No auth file is read or copied. The bundled gpt-5.5 entry has
`multi_agent_version` changed to `v2` for the collaboration positive control.
The patch control retains `apply_patch_tool_type="freeform"`; sealed cases
set it to null, exactly as the runner's private catalog does. All other
bundled model metadata is preserved by the runner.

Results on CLI 0.158.0:

- The patch control exposes `apply_patch`; every sealed case excludes it.
- The collaboration control exposes `collaboration`; `agents.enabled=false`
  excludes it even when the model advertises multi-agent support.
- New, resumed, and homecoming requests expose the Daycare namespace plus
  `list_mcp_resources`, `list_mcp_resource_templates`, and `read_mcp_resource`.
  World requests exclude memory save; homecoming exposes only memory list/save.
- Daycare tools are direct (`omit_tools_from=["deferred"]`), so no tool-search
  host is needed. The three resource helpers can query only configured MCP
  servers; the seal configures only Daycare. They cannot directly read local
  files. Their use still fails the post-turn foreign-tool check.
- The day-report request has an empty tool catalog.
- Preflight with the same private catalog renders only persona and user text.

Why the catalog override is necessary: in Codex 0.158, native patch registration
checks `model_info.apply_patch_tool_type.is_some()`, independently of feature
flags. The runner exports the bundled catalog locally, keeps only the selected
model (gpt-5.5 in this capture),
clears that field, writes a private 0600 catalog, and passes `model_catalog_json`
to preflight and every exec/resume. Missing or unrecognized metadata fails
before launch. Source: [tool registration](https://github.com/openai/codex/blob/rust-v0.158.0/codex-rs/core/src/tools/spec_plan.rs)
and [configuration schema](https://github.com/openai/codex/blob/rust-v0.158.0/codex-rs/core/config.schema.json).
The same source registers the three resource helpers whenever an MCP server
exists; they have no individual configuration switch in this version.

This is actual request-catalog evidence, not a new live model visit. Installed
Mac validation and a full 0.158 visit remain release-manager checks.

## Code mode on Codex 0.162 (finding, 2026-10-16)

In the 0.162.1 bundled catalog every model except `gpt-5.5` has
`tool_mode = "code_mode_only"` (gpt-6.1-sol, gpt-6-astra, gpt-6-sol,
gpt-6-luna, gpt-5.6-sol/terra/luna, and others). Accounts no longer offer
gpt-5.5. Daycare refused those models and disabled `code_mode_host`, so no
Codex visit could start.

**Was the block necessary? No.** Code mode does not widen reach; it only
changes how the same tools are called.

Source (`rust-v0.162.1`):

- `core/src/tools/mod.rs` `requested_tool_mode`: the model's `tool_mode`
  wins over the `code_mode*` feature flags, so disabling those flags cannot
  turn a code-mode-only model into a direct one.
- `core/src/tools/code_mode/mod.rs` `take_unavailable_warning`: with
  `code_mode_host` disabled, a code-mode-only model "will fail closed". The
  request then carries no tools at all; the old seal produced exactly this.
- `code-mode-runtime/src/runtime/globals.rs`: the script runs in a bare V8
  isolate (`v8_enable_sandbox`). `console`, `Atomics`, `SharedArrayBuffer`
  and `WebAssembly` are deleted. The only globals are `tools`, `ALL_TOOLS`,
  output helpers (`text`, `image`, `audio`, `generatedImage`, `notify`),
  `store`/`load` (an in-memory map), timers, `yield_control` and `exit`.
  `module_loader.rs` rejects every import. `image()` accepts only `data:`
  URLs. There is no file, network, process or Node API.
- `globals.rs` `build_tools_object` exposes only the enabled nested tools.
  `core/src/tools/spec_plan.rs` `register_code_mode_executors` builds that
  list from the same tool registry as direct mode, and
  `code_mode/mod.rs` dispatches each nested call back through the core tool
  router. A tool the seal removes cannot be called from a script.
- `code-mode/src/remote_session/connection.rs`: Codex spawns
  `codex-code-mode-host` as a child over stdio; it listens on no port.
- `spec_plan.rs` `add_core_utility_tools`: the model's
  `experimental_supported_tools` adds `request_user_input_async` /
  `send_user_message_async` and the `clock` tools, independently of
  feature flags. These are new reach; the private catalog must clear them,
  as it clears `apply_patch_tool_type`.

Capture on this box (installed 0.162.1, gpt-6.1-sol, local capture endpoint,
no model call; method of `dev/codex-tools-check.py`):

- Current seal (`code_mode_host` disabled): stream warns "Code Mode is
  unavailable because code-mode host is disabled. Code mode will fail
  closed"; the request has no tools.
- Seal with `code_mode_host` allowed: top-level tools are `exec`, `wait`
  and `request_user_input_async`. `exec` nests `list_mcp_resources`,
  `list_mcp_resource_templates`, `read_mcp_resource`, `clock__curr_time`,
  and the two allowed Daycare tools. The `exec` description itself says
  "no Node, no file system, no network access, no console".
- Same, with `experimental_supported_tools` cleared in the private catalog:
  top-level `exec` and `wait` only; nested: the three MCP resource helpers
  and the Daycare tools. This is the same reach as gpt-5.5 in direct mode.
- `codex debug prompt-input` with the seal renders only the persona and the
  user message for gpt-6.1-sol, as for gpt-5.5.

`codex exec --json` drops thread items it has no JSONL mapping for
(`exec/src/event_processor_with_jsonl_output.rs`, `_ => None`). Nested
Daycare and resource-helper calls still appear as `mcp_tool_call` items,
so the stream check still sees them.

Forced hostile script (`dev/codex-sandbox-probe.py`, scripted local model,
no credentials). The sealed gpt-6.1-sol turn ran one `exec` script that
probed the isolate. Results returned to the model:

- `typeof` `require`, `process`, `fetch`, `Deno`, `XMLHttpRequest`,
  `WebSocket`, `console` and `WebAssembly`: all `undefined`.
- `Object.keys(tools)`: `list_mcp_resource_templates`, `list_mcp_resources`,
  `mcp__daycare__daycare_probe`, `read_mcp_resource`.
- Calling `exec_command`, `shell`, `shell_command`, `apply_patch`,
  `read_file`, `list_dir`, `view_image`, `write_stdin`,
  `request_user_input_async`, `clock__curr_time`, `web__run` or
  `spawn_agent`: `TypeError: tools[name] is not a function`.
- `import` of `fs`, `node:child_process`, an https URL and a file URL:
  `unsupported import in exec`.
- `read_mcp_resource` for `file:///etc/hostname` goes to the Daycare server
  (which answers "method not found"); for any other server name it fails
  with "unknown MCP server".
- `image()` with a file URL or remote URL: refused; only data URIs pass.

**Conclusion.** Allow `code_mode_host`; accept `tool_mode` values `null`,
`direct`, `code_mode` and `code_mode_only` (refuse unknown values); clear
`experimental_supported_tools` alongside `apply_patch_tool_type`. Shell,
patch, web, image, memory, agents and the rest stay disabled exactly as
before, and the code-mode isolate adds no reach of its own.
