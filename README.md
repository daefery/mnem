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

<h3 align="center">Ask why your code is the way it is. mnem answers from your coding agents' own sessions, with sources.</h3>

<p align="center">
  mnem keeps a complete local record of every <b>Claude Code</b>, <b>Codex</b> and <b>pi</b>
  session, read from the transcripts they already save. Every memory shows where it came
  from, each agent starts with what the others learned, and no session goes missing
  because a hook failed.
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#try-it-first-without-changing-anything">Try it first</a> ·
  <a href="#what-you-get">What you get</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#privacy">Privacy</a> ·
  <a href="#faq">FAQ</a>
</p>

```sh
curl -fsSL https://github.com/daefery/mnem/releases/latest/download/install.sh | sh
```

## Why mnem

Three weeks after an agent changed your code, you want to know why. The chat is gone, and
the commit message says "fix tests".

Most memory tools can't help, because they write a summary while you work, through a hook
that has to succeed, and keep only the summary. When the hook misfires or the model is
busy, that session is never saved, and nothing tells you.

mnem works from the transcripts instead. Claude Code, Codex and pi already save every
prompt, command and edit to disk. mnem reads those files, so:

- **No session goes missing.** A crash, a skipped hook or a model outage only delays a
  memory. On the author's machine mnem re-read 1.45 GB of transcripts from scratch with
  zero duplicates, and `mnem doctor` tells you exactly how far behind it is.
- **Every answer has a source.** Each memory points to the transcript lines it came from.
  `mnem ask "why did we..."` answers from memories and lists them, so you can check.
- **Old memories say they're old.** Each one knows whether the code its session wrote is
  still in your repository. On mnem's own repo, 75% of the lines agents wrote last month
  are still there; memories about the rest say so, so they don't send your agent the
  wrong way.
- **Your agents share one memory.** A Codex session hears what Claude Code did in the same
  project an hour ago.
- **You see which lines an agent wrote,** even in commits from before you installed it.

It runs on the Claude Code or Codex sign-in you already have. One binary, no server, no
Node, Python, Docker or vector database.

## Install

One line for Linux (x86_64, arm64), WSL and macOS. No Rust needed.

```sh
curl -fsSL https://github.com/daefery/mnem/releases/latest/download/install.sh | sh
```

The script downloads the binary for your system, checks its SHA-256 and puts it in
`~/.local/bin`. Then it connects Claude Code, Codex and pi, backing up each settings file
first, and starts a small background service that writes memories with your Claude Code
or Codex sign-in. Keep working as usual. After your first session:

```sh
mnem doctor        # ends with "status: OK"
```

The viewer is at **http://127.0.0.1:37777**.

> **Using Codex?** Codex runs no hook until you trust it. Open Codex, type `/hooks` and
> trust mnem's three hooks. Until then your Codex sessions are still recorded, they just
> don't get memories back. `mnem doctor` reminds you.

### Try it first, without changing anything

Want to see what mnem finds before it touches your setup? This installs only the binary
and reads your existing transcripts into its own folder. Your agents' settings stay
exactly as they are.

```sh
curl -fsSL https://github.com/daefery/mnem/releases/latest/download/install.sh | MNEM_NO_SETUP=1 sh
~/.local/bin/mnem backfill                  # read every saved session
~/.local/bin/mnem search "flaky test"       # search all of them, across agents
~/.local/bin/mnem doctor                    # what was found, per agent
```

To undo it, delete `~/.local/bin/mnem` and `~/.mnem`. To keep it, run `mnem install --watch`.

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

Needs Rust 1.89 or newer. `--features fastembed` adds the local model used for
meaning-based search.
</details>

<details>
<summary><b>Options and older systems</b></summary>

- `MNEM_VERSION=v0.3.3` picks a release, `MNEM_BIN_DIR` another folder, and
  `MNEM_NO_SETUP=1` installs only the binary.
- Linux with glibc 2.35 to 2.38 (Ubuntu 22.04, Debian 12) and Intel Macs get a "lite"
  build: the same mnem, with a smaller model for meaning-based search.
- Windows: use WSL.
- The background service downloads the search model once (about 90 MB).
</details>

## What you get

### Answers about past work, with sources

<p align="center"><img alt="mnem ask answering a question from memories and listing the ones it cited" src="docs/assets/ask.svg" width="88%"></p>

`mnem ask` answers only from the memories it found and lists every one it cited. If they
don't cover the question, it says so.

### Memory that shows up on its own

You don't call anything. When a session starts, when you type a prompt and when the agent
opens a file, mnem adds the few memories that match:

```text
mnem recall: past memories matching this prompt (full text: get_observations([ids]))
#2 summary · 3d ago · Stripe sometimes charges a customer twice when the webhook is retried. Find out why and fix it.
#1 bugfix · 3d ago · Replayed Stripe webhooks no longer create duplicate orders
```

Opening a file brings the memories about that file, each noting whether its edits are
still there. Agents can also search on their own through mnem's MCP tools, and
`mnem remember "..."` pins a fact every agent sees at session start.

### A live view of every agent

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/viewer-dark.png">
    <img alt="The mnem viewer: a live feed of prompts, memories and session summaries from Claude Code, Codex and pi" src="docs/assets/viewer-light.png" width="88%">
  </picture>
</p>

Every agent's prompts and memories as they happen. Search them, filter by project, take
a backup, or fix an agent that isn't connected with one click.
(The screenshots use a made-up project: [docs/demo](docs/demo/make-demo.py).)

### Which lines did an agent write?

<p align="center"><img alt="mnem trace writing Agent Trace records for recent commits" src="docs/assets/trace.svg" width="88%"></p>

`mnem trace` writes [Agent Trace](https://agent-trace.dev) records for your commits:
which added lines came from which agent session and model. It covers commits from before
you installed anything, because the transcripts already hold every edit. Useful in code
review, and for teams with rules about AI-written code.

### Build your own tools on it

A local, read-only API serves the whole record, so your scripts skip parsing three
transcript formats:

```sh
mnem api        # prints the address and token
curl -s -H "Authorization: Bearer $(cat ~/.mnem/api-token)" \
  "http://127.0.0.1:37777/v1/search?q=webhook+retries"
```

Sessions, events, memories with their evidence, and search, paged so a tool can sync.
See [docs/api.md](docs/api.md).

## How it works

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/how-dark.svg">
    <img alt="Agents write transcripts; mnem reads them into one local record, writes memories that cite their sources, and serves them back to agents, to you and to your tools" src="docs/assets/how-light.svg" width="100%">
  </picture>
</p>

1. **Capture.** Each agent appends prompts, answers, commands and edits to a transcript
   file. mnem reads each file as it grows and saves its place in the same database write,
   so nothing is skipped or read twice.
2. **Distil.** After each turn, and in the background for anything missed, your own model
   turns the session into short memories that cite the lines behind them.
3. **Recall.** Hooks put the best few memories in front of the agent at the right moment,
   ranked by words and by meaning with a local model, and never repeated in a session.

## Privacy

Everything stays on your machine: the record, the memories, the search model, the viewer
and the API. There is no mnem server.

Secrets are removed before anything is stored: API keys, tokens, passwords in URLs.

Two things leave your machine. To write memories, redacted excerpts go to the model you
chose, by default your own Claude Code or Codex sign-in. And the background service
downloads its search model once from Hugging Face.

`mnem forget` deletes a memory, a session or a whole project for good.
`mnem uninstall` disconnects your agents and keeps your data until you delete `~/.mnem`.

## Everyday commands

| Command | What it does |
|---|---|
| `mnem doctor` | Is every session captured? Which agents are connected? Anything to fix? |
| `mnem ask "..."` | Answer a question about past work, with sources |
| `mnem search <words>` | Search everything captured |
| `mnem remember "..."` | Pin a fact every agent sees at session start |
| `mnem forget <id>` | Delete a memory, session or project for good |
| `mnem backup` / `mnem restore` | Verified snapshots (taken daily by default); `--merge` adds a teammate's memory to yours |
| `mnem trace` | Agent Trace records for this repository's commits |
| `mnem api` | The record API's address and token |
| `mnem install` / `mnem uninstall` | Connect or disconnect your agents |

Every command, setting and measurement is in [docs/reference.md](docs/reference.md).

## How it compares

| | mnem | Tools that write memories during the session |
|---|---|---|
| A hook or model call fails | the memory waits; the session is read again from its transcript | that session's memory can be lost |
| Where a memory came from | points to the transcript lines | not kept by the tools we checked |
| Is its code still there? | checked for each memory | not tracked by the tools we checked |
| Agents | Claude Code, Codex and pi, one shared memory | some cover more (up to 9) |
| What you run | one binary | often Node, Python or a vector database |

Thirteen tools compared in detail, with sources: [the comparison and roadmap](https://daefery.github.io/mnem/comparison.html).

## FAQ

<details>
<summary><b>Will it slow my agent down?</b></summary>

Rarely enough to notice. On the author's machine over the past two weeks, the prompt
hook took 26 ms at the median and 167 ms for the slowest 5%. Memories are written after
the turn, in the background.
</details>

<details>
<summary><b>Does it use my Claude or ChatGPT plan?</b></summary>

Yes, if you write memories with your Claude Code or Codex sign-in, which is the default.
One request covers about 16,000 characters of a finished session, so a short session takes
one and a long one several. All background requests together stop at 100 a day. Sessions
past that wait for the next day; none are lost, and `mnem doctor` tells you. You can use
any OpenAI-compatible endpoint instead.
</details>

<details>
<summary><b>Do agents actually use the memories?</b></summary>

Not as often as they should, yet. Judges rate about two thirds of the memories mnem shows
as helpful, but agents open fewer than 1 in 10. Better ways to present them are being
tested and measured now ([roadmap](https://daefery.github.io/mnem/comparison.html#roadmap)).
The record, the sources and `mnem ask` work either way.
</details>

<details>
<summary><b>I already use claude-mem.</b></summary>

`mnem import` brings in your claude-mem history with its ids. mnem can also recover
sessions claude-mem never saved, from the transcripts still on disk. Try it first with
the no-changes steps above.
</details>

<details>
<summary><b>How do I remove it?</b></summary>

Run `mnem uninstall`, then delete `~/.local/bin/mnem`. To remove your data too, delete
`~/.mnem`.
</details>

## Contributing

Issues and pull requests are welcome. Changes to recall are measured before they ship
(`mnem eval --gate`); see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

mnem is free software under the [GNU AGPL v3.0](LICENSE). Use it, change it and share it,
at work too. If you distribute a modified mnem, or run one as a service for others, share
your changes under the same license. A commercial license is available for uses the AGPL
doesn't suit.
