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
