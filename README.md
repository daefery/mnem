<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/hero-dark.svg">
    <img alt="mnem: your coding agents already write everything down. mnem remembers it." src="docs/assets/hero-light.svg" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/daefery/mnem/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/daefery/mnem?color=0f766e&label=release"></a>
  <a href="https://github.com/daefery/mnem/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/daefery/mnem/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: AGPL-3.0" src="https://img.shields.io/badge/license-AGPL--3.0-0f766e"></a>
  <img alt="Linux, WSL, macOS" src="https://img.shields.io/badge/runs%20on-Linux%20·%20WSL%20·%20macOS-555">
</p>

<p align="center">
  <b>Local-first memory for coding agents.</b> mnem keeps one complete record of every
  <b>Claude Code</b>, <b>Codex</b> and <b>pi</b> session, straight from the transcripts they
  already write, and gives your agents back what they learned: at session start, with
  each prompt, and when they open a file.
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#what-you-get">What you get</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#privacy">Privacy</a> ·
  <a href="#faq">FAQ</a> ·
  <a href="docs/reference.md">Reference</a>
</p>

---

## Why mnem

Most memory tools ask a model to write each memory at the moment you work, through a
hook that has to succeed. When the hook misfires or the model is busy, that session is
simply gone, and nothing tells you. **mnem reads the transcript the agent already wrote
to disk instead**, so a crash, a missed hook or an offline model can delay a memory but
never lose the session.

- **Nothing lost, and you'd know.** On the author's machine, claude-mem had stored nothing
  for 32% of a month's sessions (157 of 493). mnem re-read all 1.45 GB of those
  transcripts with no duplicates, and `mnem doctor` shows exactly how far behind it is.
- **Memories you can check.** Every memory cites the transcript events it came from, and
  knows whether the code its session wrote is still in your repository.
- **One memory for every agent.** Claude Code, Codex and pi read and write the same
  store; a session hears what another agent did in the same project since it last looked.
- **No extra subscription, nothing to run.** Memories are written by the Claude Code or
  Codex you're already signed in to (or any OpenAI-compatible endpoint). One binary, no
  Node, Python, Docker or vector database.
- **Measured, not claimed.** Recall is tested on your own past prompts, judged by two
  models, and a release gate refuses any change that makes it worse.

## Install

**One line** (Linux x86_64/arm64, WSL, macOS; no Rust needed):

```sh
curl -fsSL https://github.com/daefery/mnem/releases/latest/download/install.sh | sh
```

It downloads the binary for your system, checks its SHA-256, puts it in `~/.local/bin`,
connects Claude Code, Codex and pi, starts a small background service, and picks your
Claude Code or Codex sign-in to write memories. Then just keep working. After your first
session:

```sh
mnem doctor        # ends with "status: OK"
```

and open the viewer at **http://127.0.0.1:37777**.

<details>
<summary><b>Or as a Claude Code plugin</b></summary>

```
/plugin marketplace add daefery/mnem
/plugin install mnem@mnem
/mnem:setup
```

`/mnem:setup` installs the program with the same script. If you also ran the one-line
installer, the plugin's copies stay quiet, so nothing runs twice.
</details>

<details>
<summary><b>Or build from source</b></summary>

```sh
git clone https://github.com/daefery/mnem && cd mnem
cargo install --path . --locked --features fastembed
mnem install --watch
```

Needs Rust 1.89 or newer. `--features fastembed` adds the local embedding model used for
meaning-based search.
</details>

<details>
<summary><b>Options and older systems</b></summary>

- `MNEM_VERSION=v0.3.1` picks a release, `MNEM_BIN_DIR` another folder, `MNEM_NO_SETUP=1`
  installs only the binary.
- Linux with glibc 2.35 to 2.38 (Ubuntu 22.04, Debian 12) and Intel Macs get a "lite" build:
  the same mnem, with a smaller model for meaning-based search.
- Windows: use WSL.
- The first run downloads the embedding model once (about 90 MB).
</details>

## What you get

### Memory that comes back by itself

You don't call anything. When you start a session, type a prompt or the agent opens a
file, mnem adds a few relevant memories to the context, like this:

```text
mnem recall: past memories matching this prompt (full text: get_observations([ids]))
#2 summary · 3d ago · Stripe sometimes charges a customer twice when the webhook is retried. Find out why and fix it.
#1 bugfix · 3d ago · Replayed Stripe webhooks no longer create duplicate orders
```

Opening a file brings the memories about *that file*, each saying whether its edits are
still there: `(its edited lines are still there)` or `(its edited lines are gone)`.
Agents can also search with mnem's MCP tools (`search`, `get_observations`,
`timeline`, `recall_file`).

### Ask about past work, with sources

<p align="center"><img alt="mnem ask answering from memories, citing them" src="docs/assets/ask.svg" width="88%"></p>

The answer comes only from memories it was shown, and every source it cites is listed,
so you can check it.

### See everything in the viewer

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/viewer-dark.png">
    <img alt="The mnem viewer: a live feed of sessions, prompts and memories across agents" src="docs/assets/viewer-light.png" width="88%">
  </picture>
</p>

A live feed of every agent's prompts, memories and session summaries, with search,
project filters, backups and a one-click fix for any agent that isn't connected.
(Screenshots use a made-up demo project: [docs/demo](docs/demo/make-demo.py).)

### Build on the record

Your own tools can read the whole record through a local, read-only API, instead of
parsing three agents' transcript formats themselves:

```sh
mnem api        # prints the address and token
curl -s -H "Authorization: Bearer $(cat ~/.mnem/api-token)" \
  "http://127.0.0.1:37777/v1/search?q=webhook+retries"
```

Sessions, events, memories with their evidence, and search, paged so a tool can sync.
See [docs/api.md](docs/api.md).

### Which code did the agents write?

<p align="center"><img alt="mnem trace writing Agent Trace records for commits" src="docs/assets/trace.svg" width="88%"></p>

`mnem trace` writes [Agent Trace](https://agent-trace.dev) records for your commits,
**including commits made before any tracing tool was installed**, because the
transcripts already hold every edit.

## How it works

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/how-dark.svg">
    <img alt="Agents write transcripts; mnem captures them into one local record, distils memories with evidence, and serves them back to agents, to you and to your tools" src="docs/assets/how-light.svg" width="100%">
  </picture>
</p>

1. **Capture.** Every agent appends prompts, answers, commands and edits to a transcript
   file. mnem reads each file as it grows and commits what it read together with its
   position, so nothing is skipped or read twice.
2. **Distil.** After each turn, and in the background for anything missed, mnem asks your
   model to turn the session into short memories that cite the events behind them.
3. **Recall.** Hooks put the best memories in front of the agent at the right moments,
   ranked by words and by meaning with a local embedding model, and never repeated
   within a session.

## Privacy

- **Stays on your machine:** the record, the memories, the embedding model, the viewer,
  the API. There is no mnem server.
- **Redacted before it's stored:** API keys, tokens, passwords in URLs and the like.
- **Leaves your machine:** only redacted excerpts sent to the model *you* choose, to write
  memories (by default your own Claude Code or Codex sign-in), and a one-time model
  download from Hugging Face.
- **Yours to delete:** `mnem forget` removes a memory, session or project for good;
  `mnem uninstall` removes mnem's hooks and tools (your data stays until you delete
  `~/.mnem`).

## Everyday commands

| Command | What it does |
|---|---|
| `mnem doctor` | Is capture complete? Which agents are connected? Anything to fix? |
| `mnem ask "…"` | Answer a question about past work, with sources |
| `mnem search <words>` | Search everything captured |
| `mnem remember "…"` | Pin a fact every agent sees at session start |
| `mnem forget <id>` | Delete a memory, session or project for good |
| `mnem backup` / `mnem restore` | Verified snapshots (taken daily by default) |
| `mnem trace` | Agent Trace records for this repository's commits |
| `mnem api` | The record API's address and token |
| `mnem install` / `mnem uninstall` | Connect or disconnect your agents |

Every command, setting and measurement: [docs/reference.md](docs/reference.md).

## How it compares

| | mnem | Tools that write memories during the session |
|---|---|---|
| A failed hook or model call | delays a memory; the session is read again from its transcript | can drop that session's memory |
| Where a memory came from | cites the transcript events | not kept by the tools we checked |
| Is its code still there? | checked per memory | not tracked by the tools we checked |
| Agents | Claude Code, Codex, pi, one shared memory | some cover more agents (up to 9) |
| What to run | one binary | often Node, Python or a vector database |

Thirteen tools compared in detail, with sources: [the comparison and roadmap](https://daefery.github.io/mnem/comparison.html).

## FAQ

<details>
<summary><b>Will it slow my agent down?</b></summary>

Hooks are budgeted at 300 ms; typical prompt recall takes about 20 ms. Memories are
written after the turn, in the background.
</details>

<details>
<summary><b>Does it use my Claude or ChatGPT plan?</b></summary>

If you choose your Claude Code or Codex sign-in (the default when nothing else is set),
writing memories uses your plan: one request per piece of a finished session (about
16,000 characters of transcript), so a short session takes one and a long one several.
Catching up older sessions is capped at 100 requests a day. You can point it at any
OpenAI-compatible endpoint instead.
</details>

<details>
<summary><b>Do agents actually use the memories?</b></summary>

Judges rate about two thirds of the memories mnem shows as helpful, but agents open
fewer than 1 in 10 of them. How memories are presented is being tested right now, measured the same way;
progress is on the [roadmap](https://daefery.github.io/mnem/comparison.html#roadmap).
</details>

<details>
<summary><b>I already use claude-mem.</b></summary>

`mnem import` brings in your claude-mem history with its ids, and mnem can recover
sessions claude-mem never captured from the transcripts still on disk.
</details>

<details>
<summary><b>How do I remove it?</b></summary>

`mnem uninstall`, then delete `~/.local/bin/mnem` and, if you want your data gone too,
`~/.mnem`.
</details>

## Contributing

Issues and pull requests are welcome. Recall changes are measured before they ship
(`mnem eval --gate`); see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

mnem is free software under the [GNU AGPL v3.0](LICENSE): use it, change it and share it,
including at work. If you distribute a modified mnem, or run one as a service for
others, share your changes under the same license. A commercial license is available
for uses the AGPL doesn't suit.
