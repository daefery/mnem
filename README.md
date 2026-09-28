# mnem

Local-first memory for coding agents (Claude Code, Codex CLI, pi), built from the
transcripts those agents already write to disk.

mnem never puts an LLM or a queue on the write path. Each transcript is read
incrementally; parsed events and the file cursor are committed in one SQLite
transaction, and events are deduplicated by the agent's own record ids. A crash,
a missed hook or a rewritten file can delay capture but cannot lose it, and
`mnem doctor` shows exactly how far behind capture is.

## Commands

| Command | What it does |
|---|---|
| `mnem backfill` | Ingest every transcript under `~/.claude/projects`, `~/.codex/sessions`, `~/.pi/agent/sessions` (incremental, safe to re-run) |
| `mnem import` | Import a claude-mem database (read-only snapshot; observation ids are kept) |
| `mnem install [--dry-run]` | Register hooks + MCP for Claude Code and Codex, and a pi extension. Backs up every file it changes |
| `mnem doctor [--strict]` | Capture coverage, lag, quarantine, lost bytes, claude-mem comparison |
| `mnem context --cwd DIR` | The context injected at session start |
| `mnem search <query>` | Full-text search over captured events |
| `mnem ui [--port 37777]` | Web viewer: live feed of observations, summaries and prompts across agents, search, context preview (local only) |
| `mnem watch` | Background reconciliation + distillation; also serves the viewer on :37777 |
| `mnem distill` / `mnem models` | Tier-1 distillation through the model chain / show the chain and cooldowns |
| `mnem embed` | Download the local embedding model (Model2Vec, 30 MB) and embed memories for semantic recall |
| `mnem mcp` | MCP server: `search`, `timeline`, `get_observations`, `session_start_context` |
| `mnem hook <agent> <event>` | Hook entry point (stdin JSON, Claude Code / Codex protocol) |

## What the agent sees

At session start (about 8 KB): recent sessions in the same project from every agent,
each as "human prompt -> final answer" pairs with edited files and a trailing error if
the session ended on one; the last summary; recent observations with ids; and a
capture-health footer. On each prompt, if another session in the project did
something since, a short "meanwhile" update.

## Configuration

`~/.mnem/config.json` (optional):

```json
{
  "harness_prompts": ["^: Firstmate instruction waiting"],
  "distill": {
    "base_url": "http://127.0.0.1:8317/v1",
    "api_key_json": "~/.pi/agent/cliproxyapi.json",
    "models": ["gpt-5.6-luna", "gemini-3.5-flash-lite", "developer/claude-haiku-4-5-20251001"],
    "auto_fallback": true
  }
}
```

- `semantic.model` (default `minishlab/potion-base-8M`), `semantic.enabled`: local
  semantic recall. The watch service keeps the model loaded and embeds new memories;
  hooks ask it for query vectors over localhost and fall back to keywords if it is down.
  Prompt recall keeps keyword order and lets meaning-based matches fill empty slots,
  which scored best on `mnem eval` (a fused ranking puts the right memory first more
  often but in the top five less often).
- `distill.exclude_providers` / `distill.exclude_models`: never use these models, e.g.
  `["antigravity"]` and `["gemini"]`.
- `harness_prompts`: prompts matching these patterns are labelled as tooling-injected,
  not human asks.
- `distill`: any OpenAI-compatible endpoint (CLIProxyAPI by default). Models are tried
  in order; a model that hits quota or rate limits (HTTP 402/429) cools down for 30 min,
  an unavailable one (403/404) for 6 h, a failing one (5xx, timeout) for 5 min. With
  `auto_fallback`, any other text model the endpoint lists is tried next, cheapest-looking
  first. `mnem models` shows the live order and cooldowns.

## Guarantees and limits

- Capture is at-least-once with idempotent writes; replays never duplicate.
- Rewrites are detected by first-line fingerprint, file identity, and a hash of the
  4 KB before the cursor. An in-place, same-size change further back is not detected
  (no supported agent mutates history that way).
- Quarantined lines keep a redacted excerpt; replay reads the source at the stored offset.
- Secrets are redacted by pattern before storage. This is best effort, not a guarantee.

## Build

```sh
cargo install --path .
mnem backfill && mnem import && mnem doctor
mnem install --dry-run
```

Data lives in `~/.mnem/mnem.db` (override with `MNEM_HOME` or `--db`).
