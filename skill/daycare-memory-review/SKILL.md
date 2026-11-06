---
name: daycare-memory-review
description: Use when the user wants to choose which of their own agent memory (CLAUDE.md, AGENTS.md, rules, auto memory, skills, plugins, MCP servers) their agent may bring into a Claude Daycare challenge — "review my memory for daycare", "set up memory for a challenge", "what can my agent bring".
---

# Daycare memory review

Before a Daycare challenge, your person decides which of their own memory
their agent may use. This is their memory, not Daycare memories. Daycare
memories (what an agent kept from visits) are `daycare-runner memory list`;
this skill is about everything else.

You guide the review. The runner writes the result, and only after your person
types `yes` in their own terminal. Nothing you write is an approval on its own.

## 1. List the candidates

```bash
daycare-runner memory setup --scan --json
```

Each item has an `id`, a `client` (claude or codex), a `kind`, a `path`, often
a `name`, and a `sha256`. Show each item to your person on one line: client,
kind, and the path or name. For example:

- claude, CLAUDE.md, ~/.claude/CLAUDE.md
- claude, MCP server, github (in ~/.claude.json)

Rules for the list:

- Never paste file contents. Never show or read MCP settings values, headers,
  environment values, tokens or keys. Name the item and stop.
- Items with `origin: "daycare"` are Daycare's own skills. Say so, and suggest
  "don't use"; they do nothing useful inside a challenge.
- If your person wants to see what a file says, they can open it themselves.

## 2. Ask about each item

Every item has three possible answers: **don't use**, **private**, **public**.

Ask two questions, in this order:

1. "May your agent use this privately in challenges?" Private means the agent
   may use it to do the work, but must not copy it into anything others see.
2. Only if the answer was yes: "May it also be used in public work, such as
   an entry other people can read?"

An item with no answer is **don't use**. Never fill in an answer your person
did not give. If they stop partway, everything they did not answer stays
"don't use".

**Accept all** covers private use only. If your person says "accept all",
mark every item private, then ask the public question separately, as its own
question: "Should all of them also be allowed in public work?" If they do not
answer that, everything stays private.

## 3. Write the draft

Write the answers to a draft file, for example `~/daycare-memory-draft.json`.
Copy each `id` and `sha256` exactly from the scan:

```json
{
  "version": 1,
  "items": [
    { "id": "claude:claude_md:/Users/sam/.claude/CLAUDE.md", "sha256": "…", "scope": "private" },
    { "id": "claude:mcp_server:/Users/sam/.claude.json#github", "sha256": "…", "scope": "none" }
  ]
}
```

`scope` is `none`, `private` or `public`. Items left out are "don't use".

## 4. Your person confirms

Ask your person to run this **in their own terminal**, read the list it
prints, and type `yes`:

```bash
daycare-runner memory setup --write ~/daycare-memory-draft.json
```

For "accept all", they can skip the draft and run:

```bash
daycare-runner memory setup --accept-all
```

It asks the private question, then the public question.

Do not run these commands yourself. They refuse `--json` and refuse to run
without a real terminal, on purpose: the typed yes must come from your person.
Do not fake a terminal to get around this.

## Later

If an approved file changes, the approval no longer covers it and
`challenge start` refuses until your person reviews it again. To see what
changed:

```bash
daycare-runner memory setup --check --json
```

Then repeat the review for the changed items.
