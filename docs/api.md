# The mnem record API (v1)

mnem keeps one complete, local record of what your coding agents did: every Claude Code,
Codex and pi session, captured from the transcripts they write, with nothing lost or
duplicated, secrets redacted, and distilled memories that cite their evidence. The
record API lets your own tools read it, so they do not each have to parse agent
transcripts (and break when a format changes).

It is read-only, local and served by `mnem watch` (the background service) on the
viewer's port. `mnem api` prints the address and token.

```sh
TOKEN=$(cat ~/.mnem/api-token)
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:37777/v1
```

## Promises

- **Stable.** Within v1, fields are only ever added, never removed, renamed or given a
  new meaning. A breaking change gets new routes under `/v2`, and `/v1` keeps working.
- **Private.** Every request needs `Authorization: Bearer <token>`, the token in
  `~/.mnem/api-token` (created on first use, readable by you only). The server listens
  on 127.0.0.1 only and refuses requests whose Host is not local. A web page cannot
  send the header without a CORS preflight, which mnem never answers, so a browser
  cannot read your record.
- **Read-only.** No endpoint changes anything.
- **Complete.** Lists page by id with no gaps or repeats: pass the `next` you got back as
  `after`, until `next` is null. The same lets a tool sync incrementally (remember the
  last `next`, ask for `after` it later).
- **Personal details stay out unless asked for.** Memories of type `sensitive` are left
  out of `/memories` and `/search` unless you pass `include=sensitive`.

Times are milliseconds since the Unix epoch. Errors are `{"error": "..."}` with 400
(bad parameter, named), 401 (token), 403 (non-local Host), 404 (no such thing) or 500.

## Endpoints

### `GET /v1`

The API and mnem version, the embedding model, record counts, and the endpoint list.

### `GET /v1/sessions`

One item per agent session, oldest first. Parameters: `project`, `agent` (`claude`,
`codex`, `pi`), `since` (sessions active at or after this time), `after`, `limit` (1 to
1000, default 100).

```json
{"items": [{"id": "claude:5c3e…", "agent": "claude", "native_id": "5c3e…",
  "project": "github.com/acme/api", "cwd": "/home/me/api", "git_branch": "main",
  "title": "Fix retry storm", "started_at": 1790000000000, "last_event_at": 1790003600000,
  "scripted": false, "cursor": 412}], "next": 412}
```

`scripted` marks a session another agent drove from a brief (a review council, a test
run): mnem does not distil it or offer memories to it. Sessions page by `cursor` (their
ids are text): pass `next` as `after`.

### `GET /v1/sessions/{id}`

One session (the id URL-encoded, for example `claude%3A5c3e…`), with `events` and
`memories` counts.

### `GET /v1/events`

What happened in sessions, in order: one item per prompt, answer, command, file read or
edit, error, and so on. Parameters: `session`, `project`, `kind`, `after`, `limit`.

```json
{"items": [{"id": 96612, "session_id": "claude:5c3e…", "ts": 1790000123000, "turn": 4,
  "kind": "file_edit", "tool": "Edit", "path": "/home/me/api/src/retry.rs", "text": null,
  "is_error": false, "label": null, "subagent": null}], "next": 96612}
```

`kind` is one of `prompt`, `assistant`, `command`, `file_read`, `file_edit`, `tool`,
`error`, `recap`, `compaction`, `title`, `git_state`. `label` marks a prompt that came
from tooling rather than the person (`harness`); `subagent` names the subagent thread an
event belongs to, if any. Text is already redacted.

### `GET /v1/memories`

Distilled memories, oldest first. Parameters: `project`, `kind` (`observation`,
`summary`, `pinned`), `type`, `session`, `since`, `include=sensitive`, `after`, `limit`.

```json
{"items": [{"id": 1007867, "session_id": "pi:01a0…", "project": "github.com/acme/api",
  "kind": "observation", "type": "decision", "title": "Retry loop backs off on 429",
  "subtitle": "…", "narrative": "…", "facts": ["…"], "concepts": ["problem-solution"],
  "files_read": ["src/net.rs"], "files_modified": ["src/retry.rs"], "origin": "mnem",
  "model": "gpt-5.6-luna", "created_at": 1790003700000}], "next": 1007867}
```

`origin` is `mnem` for memories mnem distilled and `claude-mem` for imported ones.

### `GET /v1/memories/{id}`

One memory with what only mnem can say about it:

- `evidence`: the transcript events it cites (`event_id`, `kind`, `ts`, a 300-character
  `excerpt`). Fetch the full events with `/v1/events`.
- `files`: for each file it modified, `edits_kept`: whether the lines its session wrote
  are still in the file now (`kept` of `of` lines, `intact` when 4 in 5 or more remain),
  or null when that cannot be told on this machine.

### `GET /v1/search?q=…`

Memories ranked for a question: every-word matches, then any meaningful word, fused
with the nearest by meaning when the embedding model is loaded (`"semantic": true`).
Each item adds `match`: `words`, `meaning` or `both`. Parameters: `q` (required),
`project`, `include=sensitive`, `limit` (at most 200, default 100).

## Agent Trace

`mnem trace` writes [Agent Trace](https://agent-trace.dev) records (v0.1.0) for a
repository's commits: per commit, the ranges of added lines agents wrote, grouped by
session (`url`: `mnem://session/<id>`, the id `/v1/sessions/{id}` takes) with the model
(`anthropic/claude-…`, `openai/gpt-…`). `metadata.dev.mnem` gives the lines the commit
added and how many were attributed.

## Building on it

Good fits: standups and weekly summaries, time sheets per client, cost and agent
reports, team dashboards, exports to other formats, your own recall. If you build one,
tell us; we will list it here.
