# mnem for Claude Code

mnem gives Claude Code (and Codex and pi, if you use them) a memory that cannot silently
lose a session. It reads the transcripts the agents already write to disk, keeps one
complete local record of every session, and distils memories that cite the transcript
events they came from and know whether the code they changed is still there. Memories
come back at session start, with each prompt and when the agent opens a file, and
`mnem ask "why did we …"` answers questions about past work with sources.

Install the plugin, then run `/mnem:setup` once: it installs the mnem program (a single
binary, no Rust or Node needed) and connects your agents.

## What this plugin runs, sends and fetches

- **Runs:** its hooks (session start, each prompt, end of turn, file reads and edits)
  and its MCP server call the `mnem` program installed on your machine, through the
  small shell scripts in `scripts/`. Without the program they do nothing, except that
  session start tells Claude to suggest `/mnem:setup`.
- **Installs (only when you run `/mnem:setup`):** the `mnem` binary from this
  repository's GitHub releases, checked against the release's SHA-256 checksums, into
  `~/.local/bin`; then `mnem install --watch` adds mnem's hooks and tools to Claude
  Code's settings (backing up each file), connects Codex and pi if installed, and starts
  a background service (systemd on Linux and WSL, launchd on macOS). When that is done,
  the plugin's own hooks stay quiet so nothing runs twice.
- **Stores:** everything in `~/.mnem` on your machine. Secrets (API keys, tokens,
  passwords in URLs) are redacted before anything is written.
- **Sends:** nothing to any mnem server; there is none. To turn sessions into
  memories, mnem sends redacted excerpts of your transcripts to the model you use: by
  default your own Claude Code (`claude -p`, sonnet) or Codex sign-in, or an
  OpenAI-compatible endpoint you configure. At most 100 background requests a day by
  default.
- **Fetches:** the embedding model for meaning-based search (about 90 MB, once, from
  Hugging Face).

## More

Full documentation, the record API for your own tools, and Agent Trace export:
https://github.com/daefery/mnem. mnem is free software under the GNU AGPL v3.0.
