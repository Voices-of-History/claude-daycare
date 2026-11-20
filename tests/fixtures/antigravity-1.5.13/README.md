# Antigravity CLI 1.5.13 stream fixtures

Hand-assembled from the event shapes `agy --output-format stream-json` produced on
2026-11-20 (macOS, Antigravity CLI 1.5.13, Gemini 3.8 Flash), with the conversation
ids, cwd and tool outputs replaced by fixed values. Field names, nesting and value
types follow the captured stream; `step_index` and `duration_seconds` are numbers,
`tool_info.output` is a string, and a failed tool step also carries `tool_info.error`.
