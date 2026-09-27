# mnem

Local-first memory for coding agents (Claude Code, Codex CLI, pi), built from the
transcripts those agents already write to disk.

mnem never puts an LLM or a queue on the write path. Each transcript is read
incrementally; parsed events and the file cursor are committed in one SQLite
transaction, and events are deduplicated by the agent's own record ids. A crash,
a missed hook or a rewritten file can delay capture but cannot lose it.

## Status

Early spike. Working today:

- `mnem backfill`: ingest every transcript under `~/.claude/projects`, `~/.codex/sessions`
  and `~/.pi/agent/sessions` (incremental, safe to re-run)
- `mnem ingest <file>`: catch up a single transcript
- `mnem doctor [--strict]`: capture coverage, lag, quarantine, and a comparison with an
  existing claude-mem database
- `mnem search <query>`: FTS5 search over captured events

Planned: claude-mem import, agent hooks with context injection, MCP server, background
distillation into typed observations, cross-agent handoff.

## Build

```sh
cargo build --release
./target/release/mnem backfill
./target/release/mnem doctor
```

Data lives in `~/.mnem/mnem.db` (override with `MNEM_HOME` or `--db`).
