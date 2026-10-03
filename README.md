# mnem

Local-first memory for coding agents (Claude Code, Codex CLI, pi), built from the
transcripts those agents already write to disk.

mnem never puts an LLM or a queue on the write path. Each transcript is read
incrementally; parsed events and the file cursor are committed in one SQLite
transaction, and events are deduplicated by the agent's own record ids. A crash,
a missed hook or a rewritten file can delay capture but cannot lose it, and
`mnem doctor` shows exactly how far behind capture is.

## Install

No Rust or build needed. On Linux (x86_64 or arm64), WSL, or macOS:

```sh
curl -fsSL https://github.com/daefery/mnem/releases/latest/download/install.sh | sh
```

While the repository is private, with the GitHub CLI signed in to an account that can
see it (`gh auth login`):

```sh
gh release download -R daefery/mnem -p install.sh -O - | sh
```

The script downloads the binary for your system, checks its SHA-256 against the
release, puts it in `~/.local/bin` and runs `mnem install --watch`: it connects Claude
Code, Codex and pi (installed yet or not), starts the background service (systemd on
Linux and WSL, launchd on macOS), and sets distillation to the Claude Code or Codex you
are signed in to when no model is configured. Then, after your first session:

```sh
mnem doctor    # ends with "status: OK"; the viewer is at http://127.0.0.1:37777
```

The first run downloads the embedding model (about 90 MB, once). Linux with glibc 2.39
or newer (Ubuntu 24.04, Debian 13, Fedora 40) gets the full build; glibc 2.35 to 2.38
(Ubuntu 22.04, Debian 12) gets a lite build without ONNX Runtime, whose prebuilt library
needs glibc 2.38: meaning search then uses the small potion model, everything else is the
same. Intel Macs get the lite build too, since ONNX Runtime ships no library for them. Windows: use WSL. `MNEM_VERSION=v0.2.0`
picks a release, `MNEM_BIN_DIR` another folder, `MNEM_NO_SETUP=1` installs only the
binary. To build from source instead, see Build below.

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed: each
target is built and tested on its own runner (full Linux on Ubuntu 24.04, lite Linux on
22.04, macOS on Apple Silicon), then the archives, `checksums.txt` and `install.sh` are
attached to the release.

## Commands

| Command | What it does |
|---|---|
| `mnem backfill` | Ingest every transcript under `~/.claude/projects`, `~/.codex/sessions`, `~/.pi/agent/sessions` (incremental, safe to re-run) |
| `mnem import` | Import a claude-mem database (read-only snapshot; observation ids are kept) |
| `mnem install [--dry-run] [--only claude,codex,pi]` | Connect Claude Code, Codex and pi (hooks, MCP tools, pi extension), installed yet or not, then show each one's state. Backs up every file it changes |
| `mnem doctor [--strict]` | Capture coverage, lag, quarantine, lost bytes, which agents are connected, claude-mem comparison |
| `mnem context --cwd DIR` | The context injected at session start |
| `mnem search <query>` | Full-text search over captured events |
| `mnem ui [--port 37777]` | Web viewer: live feed of observations, summaries and prompts across agents, search, context preview (local only) |
| `mnem watch` | Background reconciliation + distillation; also serves the viewer on :37777 |
| `mnem distill` / `mnem models` | Tier-1 distillation through the model chain / show the chain and cooldowns |
| `mnem embed` | Download the local embedding model (Model2Vec, 30 MB) and embed memories for semantic recall |
| `mnem mcp` | MCP server: `search`, `timeline`, `get_observations`, `session_start_context` |
| `mnem api` | The record API for your own tools (read-only HTTP, token auth): address, token and an example. See [docs/api.md](docs/api.md) |
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

- `semantic.model` (default `fastembed:AllMiniLML6V2` in builds with `--features fastembed`,
  as the release binaries are, else `minishlab/potion-base-8M`), `semantic.enabled`: local
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
  A distillation title rule that names the component, file or decision first was not
  adopted either: on 30 chunks with the contents unchanged and only titles rewritten,
  top-5 recall fell from 22 to 18 and titles-only choices to open a memory from 19 to 16;
  paths and ids replaced the words people ask in. `mnem eval --titles N` (re-distils N
  chunks with the candidate rule) and `--titles N --retitle` (rewrites only the titles
  of the last run's chunks) re-run it.
- `semantic.relevance_cosine` / `fill_cosine` / `search_cosine`: similarity thresholds.
  They depend on the model; tuned defaults exist for potion-8M (0.45 / 0.55 / 0.35)
  and MiniLM (0.30 / 0.50 / 0.35). Tune others with `mnem eval --set real-dev --judge
  --dump <file>`, which writes each candidate's cosine and judgment.
- `mnem eval`: measures recall on test sets in `~/.mnem/eval/` (private; they hold
  your prompts). `--set recall` (model-written questions, `--build N`), `--set vague`
  (hand-written, `"id": null` for prompts nothing should answer), `--set real-dev` /
  `real-test` (real prompts replayed as of when they were typed, `--build-real N`;
  add `--judge` to have the distillation models judge what recall showed, or
  `--judge-model <model>` for a second judge and its agreement with the first), and
  `--set recent-dev` / `recent-test` (`--build-recent N`: prompts of the last 30 days
  in projects that already held 20 memories mnem distilled itself, at most 6 per
  session and half per project; the `real` sets mostly predate those memories and
  see imported claude-mem ones). Tune on the dev halves; read the test halves only
  to confirm.
- `distill.exclude_providers` / `distill.exclude_models`: never use these models, e.g.
  `["antigravity"]` and `["gemini"]`.
- `harness_prompts`: prompts matching these patterns are labelled as tooling-injected,
  not human asks.
- `scripted_sessions`: patterns for prompts another agent's brief sends (a review
  council, a test run), e.g. `["^Round \\d+\\. Read /tmp/"]`. A session with a matching
  prompt is scripted: it is not distilled, not offered memories, left out of uptake, and
  the memories already made from it stay out of recall, search and the session-start
  context. Nothing is deleted; change the patterns and the next scan (every distillation
  pass, or `mnem doctor`, which shows the count) re-marks every session. On this machine
  24 of 2,482 sessions were scripted yet made 17% of a week's distilled memories, and
  9% of the memories shown to real sessions came from them.
- `distill.provider`: where distillation requests go. `openai` (the default) is any
  OpenAI-compatible endpoint, below. `claude-cli` runs `claude -p` (default model
  `sonnet`) and `codex-cli` runs `codex exec` (default `gpt-5.6-luna`, low effort),
  signed in as the user already is: no key, no proxy. Each run is isolated: an empty
  directory, no saved session, no tools, no MCP servers, no hooks (mnem's own included)
  and none of the user's settings or instructions. With nothing configured, `mnem install`
  picks Claude Code, else Codex, and sets `daily_calls` to 100. Chosen on 30 real session
  chunks judged blind against luna by two models: sonnet and codex with luna matched
  it, haiku made up more details and lost (so it is not the default). On a Claude plan a
  chunk costs about $0.04 of usage with sonnet; thinking is off, since it cost several
  times more and did not improve memories.
- `distill`: any OpenAI-compatible endpoint (CLIProxyAPI by default). Models are tried
  in order; a model that hits quota or rate limits (HTTP 402/429) cools down for 30 min,
  an unavailable one (403/404) for 6 h, a failing one (5xx, timeout) for 5 min. With
  `auto_fallback`, any other text model the endpoint lists is tried next, cheapest-looking
  first. `mnem models` shows the live order and cooldowns.
- `distill.backfill_days` (default 7) / `distill.daily_calls` (default 300; 0 = off):
  every 5 minutes, on its own thread so capture never waits on a model, the watcher
  distils the newest idle sessions of the last 2 days, then the ones that pass missed,
  oldest first, back to `backfill_days`, while all distillation in the last 24 hours has
  sent fewer than `daily_calls` requests (fallbacks to other models count). Backfill
  covers sessions from `backfill_days` before the watcher first ran; older ones wait for
  `mnem distill --since-days 30 --limit 1000 --max-calls 200` (newest first; repeat
  until done, `--dry-run` shows the cost). A session whose last chunk is too small to
  distil leaves the backlog after a day idle and is read again if it resumes.
  `mnem doctor` shows the backlog; a warning appears when sessions are about to leave
  the window and backfill will not reach them.

## Memories about a file

When Claude Code or pi first reads or edits a file in a session, mnem adds up to three
past memories about that file (those that changed it first; never the session's own,
never one the session was already shown), each with whether the lines its session
left in the file are still there: all or most of them (4 in 5), partly (how many), or
gone. Edit events keep only the path, so each edit is read back from the transcript
record it came from, checked to be that event's tool call (Claude Code and pi tool
input, Codex changes). Edits apply in order within the memory's chunk, so a line a later
edit replaced does not count; trimmed lines of 12 characters or more are matched whole,
and a line found more than once in the file now is no evidence either way. When that
cannot be told (edits made through a shell, a transcript gone or rewritten, more than 20
edits, a file over 2 MB), it says how the file changed since in git instead: commits
after the memory, uncommitted edits, lines added and removed, or that the file is gone.
On 460 recent memory-file pairs, 354 could be told: 268 still had their lines, 77
partly, 9 not, where the file-level check called nearly all of them changed. It takes
about 60 ms, once per file per session. Claude Code gets them from a PostToolUse hook; pi's extension appends them
to the read, edit or write result (`mnem file <path> --touch`, at most 2.5 s). Any agent can ask with the MCP tool `recall_file(path)`, and people with
`mnem file <path>`; `get_observations` also says, per file a memory touched, whether
its edits are still there and how the file changed since.

Recorded paths are placed before they are compared: a relative path is resolved
against the memory's session directory when that was inside the repository (monorepo
packages stay apart); an absolute path inside the repository is compared exactly, and
one from another machine must end the same way with the repository's name before that
ending (so a same-named file in `node_modules` is not this one). Git runs with literal
path names and a 1.5 s limit per call. The file is claimed for the session before
anything else, so parallel reads of it, or a retry after a timeout, show nothing twice.

Measured before it was switched on: on 172 real file edits replayed as of the edit
(split by session), 44% had memories about the file, and of the memories offered
71% (tuning half) and 79% (held-out half) were judged helpful by gpt-5.6-luna, 58% and
55% by claude-haiku-4-5; only 4 of 198 repeated what prompt recall had shown. Told
which file the agent is working on (as it is when these arrive), the judge rates them
higher: with the file name hidden, 51% were judged helpful on the tuning half. So they
are about as useful as prompt recall, and they add what prompt recall does not find. The recall gate checks it on every change. Codex edits
through apply_patch and asks to re-trust changed hooks, so it uses `recall_file`.

## Is it used? `mnem uptake`

Evals say whether recalled memories would help; `mnem uptake` says what agents do with
them. Every memory mnem injects is recorded with its source (session start, prompt
recall, file recall), every call to mnem's MCP tools with the memories it asked for,
and every hook run with its duration. The report shows, per source, how many offered
memories were fetched in full within a day in the same project or cited by id (`#123`)
in the agent's replies, MCP calls per tool, and hook p50/p95. `mnem doctor` prints a
one-line summary. Transcripts keep neither hook context nor MCP arguments, so counting
starts when this is installed.

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
model-written, hand-written and both real-prompt (tuning half) test sets, and exits 1 if
recall got worse:

- the embedding model must still load;
- known questions in the top 5 may drop by at most one case, found first by two;
- vague questions in the top 5 by at most one;
- prompts with no answer that still recall something: at most 3, and at most one more;
- real prompts, judged, and the same for recent prompts: helpful share of shown
  memories at most 3 points lower (its 95% interval is printed beside it), unhelpful
  memories at most 10% (+3) more, prompts shown only unhelpful memories at most two
  more, prompts helped at most two fewer, and every prompt judged; the judge is always
  the live settings' models (a baseline built before the recent set skips its checks);
- MCP search on the known questions: top 5 and top 20 at most one lower, and no
  personal-detail memory listed unasked;
- speed: the slowest 5% of rankings, and of prompt recall as the hook runs it (a fresh
  process per real prompt: start-up, settings, database open and a round trip to the
  build's own service on a copy of the data), stay under 300 ms, with at most one
  fallback to keywords.

Both builds run on their own copy of one snapshot, taken read-only before anything
opens the live database. Test sets that are missing or too small make it fail.

The real-prompt test half is never used by the gate, so repeated gating cannot tune
recall to it; read it once when a change is final. The gate takes about a minute; new
candidates are judged once by the configured models and cached.

## Backups

`mnem watch` takes a verified backup when the newest is a day old and keeps the seven
newest in `~/.mnem/backups/` (about the database's size each). To back up only by hand,
untick **Back up automatically every day** under **Backup & move** in the viewer, or run
`mnem backup --auto off` (`--auto on` turns it back on). Then backups are taken only with
**Create backup now** or `mnem backup`, and a missing or old backup is no longer an
alert.

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
   prompts go or which key is used. After ticking them, **Restart mnem** (as a service
   it comes back in about 10 s). Transcripts on the new machine are read again
   afterwards, so its own sessions return; the old machine's are kept as history and
   `mnem doctor` lists them as from another machine.

Needs free space for about twice the backup plus the current database. A backup whose
settings use `fastembed:` models needs a build with `--features fastembed`; the preview
says so. The watch service downloads a missing embedding model when it starts (internet
required) and re-embeds memories in the background; recall uses keywords until then.
The viewer's restart button appears only when mnem runs as the service `mnem install
--watch` sets up, which restarts it: a systemd user unit with `Restart=always` on Linux
and WSL, a launchd agent with `KeepAlive` on macOS.

From a shell: `mnem backup`, copy `~/.mnem/backups/mnem-*.db`, then
`mnem restore <file> --apply [--settings]`. Automatic backups carry settings too.

The viewer's changing actions are POSTs that need an `X-Mnem` header from the viewer's
own origin, so another website open in the browser cannot trigger them.

## Guarantees and limits

- Capture is at-least-once with idempotent writes; replays never duplicate.
- Rewrites are detected by first-line fingerprint, file identity, and a hash of the
  4 KB before the cursor. An in-place, same-size change further back is not detected
  (no supported agent mutates history that way).
- Quarantined lines keep a redacted excerpt; replay reads the source at the stored offset.
- Secrets are redacted by pattern before storage: private keys, JWTs, OpenAI/Anthropic,
  GitHub, GitLab, AWS, Slack, ClickUp, Stripe, Google, Hugging Face, npm and Telegram
  tokens, Slack webhooks, Bearer headers, `*KEY/SECRET/TOKEN/PASSWORD=` assignments and
  passwords in URLs (`postgres://user:[redacted]@host`). When the patterns grow, the
  watcher redacts what is already stored again, once, in batches (events, memories,
  session titles); the transcripts on disk are the agents' own and stay as they are, and
  so do earlier backups. This is best effort, not a guarantee.

## Build

```sh
cargo install --path . --locked --features fastembed   # --features fastembed: semantic recall
mnem install --dry-run   # what it would change
mnem install --watch     # connect the agents, start the background service (systemd;
                         # launchd on macOS), and pick
                         # Claude Code or Codex for distillation if no model is set
mnem doctor              # ends with "status: OK"
```

### Agents installed before or after mnem

The order does not matter. `mnem install` writes each agent's part whether the agent
is installed yet or not (Claude Code's tools too, without its `claude` command), so an
agent installed later gets memory from its first session. Two cases need a step:

- **Codex** runs a hook only after you trust it: on its first start after `mnem
  install` it says hooks need review; type `/hooks` and trust mnem's.
- **An agent reinstalled or reset** can lose mnem's entries.

`mnem doctor` lists every agent as connected, not installed, not connected (with what
is missing), or waiting for Codex trust. The viewer (http://127.0.0.1:37777) shows the
same above the feed when an installed agent is not connected, with a **Connect**
button that does what `mnem install --only <agent>` does.

Data lives in `~/.mnem/mnem.db` (override with `MNEM_HOME` or `--db`).

## Licence

mnem is free software under the [GNU Affero General Public License v3.0](LICENSE): use
it, change it and share it, including at work. If you distribute a modified mnem, or run
one as a service for others, you share your changes under the same licence. A commercial
licence is available for uses the AGPL does not suit. Third-party components and their
licences are listed in `NOTICE` and, in each release, `THIRD-PARTY-LICENSES.txt`.
Contributions: see [CONTRIBUTING.md](CONTRIBUTING.md).
