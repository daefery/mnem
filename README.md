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
    "models": ["gpt-5.6-luna", "developer/claude-haiku-4-5-20251001"],
    "auto_fallback": true
  }
}
```

- `semantic.model` (default `minishlab/potion-base-8M`), `semantic.enabled`: local
  semantic recall. The watch service keeps the model loaded and embeds new memories;
  hooks ask it for query vectors over localhost and fall back to keywords if it is down.
  Prompt recall leaves out memories from the asking session (the agent has that
  conversation already), keeps keyword order, drops keyword hits that are far from the
  prompt in meaning, and lets meaning-based matches fill empty slots.
  Recommended: `fastembed:AllMiniLML6V2` (build with `--features fastembed`; ONNX,
  46 MB binary, ~30 memories/s to embed, 14 ms per query in the service). Seven models
  were screened on judged real prompts for the job the model does in recall: reordering
  and filtering keyword candidates (not retrieval on its own). MiniLM separated helpful
  from unhelpful candidates best (AUC 0.731 against 0.657 for potion-8M, bootstrap 95%
  CI of the difference +0.008 to +0.144); bge-small-en-v1.5 (0.711) is within noise of
  it and half as fast. On 90 held-out real prompts, run once, MiniLM showed more helpful
  memories (179 vs 149 by gpt-5.6-luna, 141 vs 100 by claude-haiku-4-5) without more
  unhelpful ones. The trade: on the model-written set hit@1 fell 61% to 55% (two cases)
  and on the hand-written set prompts with no answer that still recalled something rose
  from 1 to 3 of 10. Reproduce with `mnem eval --set real-dev --judge --dump <file>` per
  model (judging costs about one LLM call per prompt the first time; later models reuse
  the cached judgments), then `mnem eval --analyze <files>`. `mnem embed --import <db>` reuses vectors
  computed in another copy of the database when the memory text still matches.
  Cross-encoder reranking of the top ten was tried the same way and not adopted: none
  of jina-reranker-v1-turbo-en, bge-reranker-base or jina-reranker-v2-base-multilingual
  separated helpful from unhelpful candidates clearly better than MiniLM (AUC 0.728,
  0.702, 0.745 against 0.713, all within noise), and each took 0.9-3.4 s per prompt on
  this CPU. That screen only let a reranker reorder and filter the keyword top ten;
  two uses stay untested: scoring a wider pool (keyword ranks 11-60 and meaning-only
  hits) and reordering MCP search results, where seconds of latency are acceptable.
  `mnem eval --rerank <model> --dump <file>` re-runs the comparison.
- `semantic.relevance_cosine` / `fill_cosine` / `search_cosine`: similarity thresholds.
  They depend on the model; tuned defaults exist for potion-8M (0.45 / 0.55 / 0.35)
  and MiniLM (0.30 / 0.50 / 0.35). Tune others with `mnem eval --set real-dev --judge
  --dump <file>`, which writes each candidate's cosine and judgment.
- `mnem eval`: measures recall on test sets in `~/.mnem/eval/` (private; they hold
  your prompts). `--set recall` (model-written questions, `--build N`), `--set vague`
  (hand-written, `"id": null` for prompts nothing should answer), `--set real-dev` /
  `real-test` (real prompts replayed as of when they were typed, `--build-real N`;
  add `--judge` to have the distillation models judge what recall showed, or
  `--judge-model <model>` for a second judge and its agreement with the first). Tune
  on `real-dev`; read `real-test` only to confirm.
- `distill.exclude_providers` / `distill.exclude_models`: never use these models, e.g.
  `["antigravity"]` and `["gemini"]`.
- `harness_prompts`: prompts matching these patterns are labelled as tooling-injected,
  not human asks.
- `distill`: any OpenAI-compatible endpoint (CLIProxyAPI by default). Models are tried
  in order; a model that hits quota or rate limits (HTTP 402/429) cools down for 30 min,
  an unavailable one (403/404) for 6 h, a failing one (5xx, timeout) for 5 min. With
  `auto_fallback`, any other text model the endpoint lists is tried next, cheapest-looking
  first. `mnem models` shows the live order and cooldowns.

## Changing recall: the release gate

Build the change, then run the new build's gate before installing it:

```sh
cargo build --release --features fastembed
target/release/mnem eval --gate                      # this build vs the installed mnem
mnem eval --gate --candidate-config new-config.json  # a settings change instead
```

It runs the installed mnem and the candidate one after the other on one frozen copy
of the database and the same cached judgments (live sessions cannot move one run and
not the other), over the
model-written, hand-written and real-prompt (tuning half) test sets, and exits 1 if
recall got worse:

- the embedding model must still load;
- known questions in the top 5 may drop by at most one case, found first by two;
- vague questions in the top 5 by at most one;
- prompts with no answer that still recall something: at most 3, and at most one more;
- real prompts, judged: helpful share of shown memories at most 3 points lower (its
  95% interval is printed beside it), unhelpful memories at most 10% (+3) more,
  prompts shown only unhelpful memories at most two more, prompts helped at most two
  fewer, and every prompt judged; the judge is always the live settings' models;
- MCP search on the known questions: top 5 and top 20 at most one lower, and no
  personal-detail memory listed unasked;
- speed: the slowest 5% of rankings, and of prompt recall as the hook runs it (process
  start plus a round trip to the build's own service on a copy of the data), stay
  under 300 ms, with at most one fallback to keywords.

Both builds run on their own copy of one snapshot, taken read-only before anything
opens the live database. Test sets that are missing or too small make it fail.

The real-prompt test half is never used by the gate, so repeated gating cannot tune
recall to it; read it once when a change is final. The gate takes about a minute; new
candidates are judged once by the configured models and cached.

## Moving to another machine

In the viewer (http://127.0.0.1:37777), open **Backup & move**:

1. On the old machine, **Create backup now**, then **Download**. The file holds every
   session, event and memory plus your settings (config.json names key files; it never
   holds keys). It holds your full history, so keep it private.
2. On the new machine, install mnem (`mnem install`), open the viewer, **Choose backup
   file**. It is uploaded and checked on a scratch copy (integrity, schema, search);
   you see it next to what this machine holds. Nothing changes until you confirm.
3. **Replace** restores it through SQLite's online-backup API. mnem first takes the
   database's write lock, then saves the current memory, then copies, so no write can
   fall between the two. Only data is taken from the file: its triggers, views and
   indexes are dropped and rebuilt from this build. Settings are off by default; the
   preview lists every setting that would change and flags any that change where
   prompts go or which key is used. After ticking them, **Restart mnem** (under systemd
   it comes back in about 10 s). Transcripts on the new machine are read again
   afterwards, so its own sessions return; the old machine's are kept as history and
   `mnem doctor` lists them as from another machine.

Needs free space for about twice the backup plus the current database. A backup whose
settings use `fastembed:` models needs a build with `--features fastembed`; the preview
says so. The watch service downloads a missing embedding model when it starts (internet
required) and re-embeds memories in the background; recall uses keywords until then.
The viewer's restart button appears only when mnem runs under a systemd unit with
`Restart=always` or `on-failure` (as `mnem install` sets up).

From a shell: `mnem backup`, copy `~/.mnem/backups/mnem-*.db`, then
`mnem restore <file> --apply [--settings]`. Nightly backups carry settings too.

The viewer's changing actions are POSTs that need an `X-Mnem` header from the viewer's
own origin, so another website open in the browser cannot trigger them.

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
