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

This runs the installed binary against a temporary local HTTP endpoint,
captures the outgoing tool definitions, and returns HTTP 400 before any
model runs. It uses the bundled gpt-5.5 catalog entry with only
`multi_agent_version` changed to `v2`, providing a positive control for a
model that advertises collaboration. With the same disabled feature flags,
`agents.enabled=true` still exposes the collaboration namespace;
`agents.enabled=false` removes it. No auth file is read or copied.

This proves removal of collaboration tools, not every built-in tool. The
capture still exposes `apply_patch`; read-only sandboxing and the post-turn
file-change rejection remain necessary. Request-user-input and update-plan
tools are explicitly disabled by the current seal settings.
