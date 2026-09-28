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
