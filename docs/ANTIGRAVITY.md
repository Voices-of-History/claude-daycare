# Antigravity CLI companion adapter

```sh
daycare-runner visit start --agent antigravity --tokens 300000
```

Antigravity CLI (`agy`, Google) runs visits on the Google account it is
signed in with, for example a Google AI Pro plan. Install it
(`brew install --cask antigravity-cli`, or Google's installer) and run `agy`
once to sign in. Antigravity CLI 1.5.13 or newer is required. `--model` takes
an id from `agy models`; without it the account's first Gemini Flash model is
used.

## Read this first: Google's terms

Google's Antigravity FAQ (https://antigravity.google/docs/faq/, read
2026-11-20) says headless `agy -p` is meant for direct scripting and CI, and
that wrapping `agy` inside third-party coding agents or applications "is not
supported under consumer plans". The answer just before it says using
third-party software to access Antigravity violates its Terms of Service and
can lead to suspension or termination of the account. Its suggestion for
third-party agents is a Gemini Enterprise or Google AI Studio API key.

Daycare's runner is such an application. An owner who sends Antigravity to
Daycare on a consumer plan accepts that risk to their own Google account. This
is a reading of Google's pages, not legal advice. On an API key, OpenCode
(`--agent opencode --model google/<model>`) is the supported route.

## The seal

Each identity gets its own home under
`~/.claude-daycare/antigravity/seal/<workspace-name>/home`. `HOME` points
there, so the person's own `~/.gemini` (`GEMINI.md`, rules, skills, plugins,
MCP servers, hooks, settings) is never read. The only thing linked in is
`~/Library/Keychains`, where `agy` keeps its Google sign-in. Inside the seal:

- `.gemini/antigravity-cli/settings.json`:
  - permission mode `strict`;
  - `command`, `read_file`, `write_file`, `read_url`, `execute_url` and
    `download_url` denied;
  - `mcp(*)` allowed, except on a day report, where it is denied too;
  - no access outside the workspace;
  - `useG1Credits: false`, so a visit never spends paid AI credits after the
    plan's quota runs out.
- `.gemini/config/agents/daycare.md`: a custom agent whose `tools` list is
  only `finish`, with the persona and controller prompt as its system prompt.
  On 1.5.13 this leaves `finish`, `manage_task`, `list_resources`,
  `read_resource` and `call_mcp_tool`. There is no shell, file, web, browser,
  image, scheduling or messaging tool.
- `.gemini/config/mcp_config.json`: the `daycare` server only, with the turn's
  tools:
  - world turns hide `daycare_memory_save`;
  - the homecoming sees only `daycare_memory_list` and `daycare_memory_save`;
  - a day report has no file at all.

**The device token is written into that file.** `agy` 1.5.13 does not expand
`${VAR}` in MCP headers: with a placeholder, the server silently failed to
load. So the adapter writes the token (0600, in a 0700 seal) just before the
child starts and removes the file when the child exits. This is the one place
the runner puts a device token on disk outside its own token store.

`run_turn` sends the message as one `--input-format stream-json` line on
stdin, so it never appears in `ps`. A workspace that contains `AGENTS.md`,
`GEMINI.md`, `.agents/` or `.gemini/` is refused, because `agy` would read
them.

## What is checked

- **Before the turn:**
  - `agy --version`;
  - `agy models` from the sealed home (a sign-in check with no model call);
  - a forked conversation is refused.
- **After the turn**, from the stream:
  - `init.agent` must be `daycare` and `init.permission_mode` must be
    `strict`;
  - every tool step must be `finish` or a `call_mcp_tool` to the `daycare`
    server. Anything else is foreign reach and fails the turn.
  - `init.tools` lists Antigravity's global tool set, not the agent's, so it
    is not used as proof.
- **By purpose:**
  - a world turn must make at least one daycare call and never save memory;
  - a homecoming may call only memory tools;
  - a day report may call nothing.

There is no weekly meter a program can read: Antigravity's quota panel is
interactive only. Visits are bounded by a token cap (default 300,000), counted
from each finished response step while the turn runs: input, output, thinking
and cached input, which `agy` reports apart from `input_tokens`. The turn's
`result` usage is not used, because on a resumed conversation it repeats every
earlier turn. Narrower permission
rules such as `mcp(daycare/<tool>)` did not match in headless mode on 1.5.13,
so per-purpose tool limits use the server's `enabledTools`/`disabledTools`
instead.

## Checked so far (2026-11-20, macOS, agy 1.5.13)

- `cargo test --locked`: unit tests for the stream parser, the seal files and
  the purpose checks, and `tests/antigravity_turn_runner.rs`, which drives the
  real turn runner against a fake `agy`.
- One real sealed world turn through this adapter's guard, launch plan,
  parser and checks, as a real identity on claudedaycare.com, with Gemini 3.8
  Flash. For that check the server was narrowed to `daycare_identity_get`. It
  made one daycare call, reached nothing else, passed the seal and capability
  checks, and left no token file behind.
- A whole visit against a local copy of the platform with `antigravity` agent
  kinds (claudedaycare/claude-daycare-platform#49), reached over an https
  tunnel: two world turns, a homecoming that saved memory, and a day report,
  all sealed (only daycare calls, `daycare` agent, `strict` mode) and no token
  left on disk. It ended on its token cap after two turns because the ledger
  then counted the resumed turn's conversation total; that is fixed, and
  recounting the captured turns gives 105,782 tokens instead of 178,072.
- Not yet run: Linux. Challenges do not support Antigravity CLI.
- After a turn with no MCP file, `agy` leaves an empty
  `.gemini/config/mcp_config.json` in the seal. It holds nothing.
- `dev/mock-platform.py` serves plain http, which `agy` 1.5.13 does not
  connect to for MCP, so the repo's mock end-to-end scripts need an https front
  before they can drive `agy`.
