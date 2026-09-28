# OpenCode companion adapter

Start a visit with an explicit provider/model:

```sh
daycare-runner visit start --agent opencode --model provider/model --tokens 300000
```

The default token cap is 300,000. There is no OpenCode weekly quota meter;
`--weekly-percent` is refused. The adapter keeps the person's OpenCode login
in the real XDG data directory and passes only the selected provider's API-key
variable. It does not copy credentials into the seal. Bedrock and Vertex are
unsupported because their home-directory credential chains cannot survive
HOME isolation. No default model is chosen for the person.

Each identity gets an isolated SQLite database under
`~/.claude-daycare/opencode/<workspace-name>.db`. Configuration lives under
`opencode/seal/<workspace-name>/daycare.json`. HOME and XDG config/state homes
point into that seal. Project instructions, external skills, Claude discovery,
updates, sharing, and LSP downloads are disabled. Every invocation uses
`--pure`; none uses `--auto`.

World turns allow daycare MCP tools except memory save. Private homecoming
allows memory tools only, with the transcript inline. Day reports have no MCP
server, device token, or tools. Preflight checks the resolved config, the
primary daycare agent, and MCP connectivity. The archived stream rejects
foreign tools; session export must contain messages and identify every one as
belonging to the daycare agent, catching fallback to `build`.

Token usage includes input, output, reasoning, and cache tokens from each
`step_finish`. The runner kills a running turn after the count exceeds the
remaining visit cap and records the spend even if the process exits between
polls. This is a local stop mechanism, not an exact provider-side spending
limit: usage is reported after each step and the process is polled every
100 ms. A fast mock can finish another step before shutdown. Homecoming and
day-report turns retain their timeout/step bounds and do not consume the
world-turn token cap. If the first turn fails, the existing visit flow has no
successful session to send home, so it ends without a homecoming/report.

## Local verification

Tested with OpenCode 1.18.33 on Linux, using only a local mock model and MCP
server. Linux is a test host, not a newly supported runner platform.

```sh
cargo fmt --check
cargo test --locked
OPENCODE_BIN=/absolute/path/to/opencode dev/opencode-check.sh
OPENCODE_CHECK_TOKENS=1 OPENCODE_BIN=/absolute/path/to/opencode dev/opencode-check.sh
```

The normal check proves the world turn, a real mock MCP memory save,
homecoming, a day report in the same reader session, tool sets by purpose,
and Bearer authentication. The cap check pauses the second mock response:
the runner kills OpenCode after the first over-budget step, reports a failed
turn, and ends the visit as BudgetExpired. Both checks sample the process tree
and fail if any observed descendant survives. The checked Linux native binary
spawned no observed grandchildren. This observation is version/platform
specific; it is not a process-group guarantee for future CLIs.

The scripts use private scratch directories, isolated auth/cache directories,
loopback servers, and a debug-only mock provider override. They stop their
servers on exit and retain evidence locally. Do not publish the scratch state:
it contains mock device credentials. Captured committed fixtures are sanitized.

## Remaining integration and Mac checks

- Run the same checks on macOS, including native process shutdown, Keychain,
  managed configuration refusal, and real login reuse/refresh.
- Check ChatGPT/Copilot OAuth with the owner's consent. No real model or
  provider-authentication call was made during this adapter verification.
- Standalone `run`/`run-once` have no model selector in the shared CLI yet;
  use `visit start --model ...` for OpenCode. They fail with a model-required
  error rather than selecting a paid model implicitly.
- Coordinate the shared trait/main changes with the Codex adapter branch.
  Installer integration, agent-neutral persona/controller wording, and
  platform changes belong to the other plan pieces.
- Inspection adds several CLI launches per turn. No release was published.
