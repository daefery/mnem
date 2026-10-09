---
description: Install or update the ravnori program that this plugin runs, and connect it to Claude Code, Codex and pi. Use when the user runs /ravnori:setup, when a session says the ravnori program is not installed, or when the user asks to install, update or check ravnori.
---

# Set up ravnori

The ravnori plugin's hooks and tools call the `rvn` program. This sets it up.

1. Check whether it is already installed: run `rvn --version`. If it prints 0.3.0 or
   newer and the user did not ask to update, skip to step 3. An older version must be
   updated: this plugin's hooks and tools need 0.3.0.
2. Install it with the release's install script (it downloads the binary for this
   system from GitHub, checks its SHA-256 against the release, puts it in
   `~/.local/bin`, then runs `rvn install --watch`):

   ```sh
   curl -fsSL https://github.com/daefery/ravnori/releases/latest/download/install.sh | sh
   ```

   Show the user its output. If it says to add `~/.local/bin` to PATH, tell them which
   line to add to their shell profile.
3. Run `rvn doctor` and report what it says in plain words: which agents are connected,
   whether distillation has a model (it uses the user's own Claude Code by default), and
   any alert. "status: OK" may take a session or two to appear.
4. Tell the user what ravnori does now: every Claude Code, Codex and pi session is kept
   locally; memories distilled from them are offered at session start and with each
   prompt; `rvn ask "<question>"` answers questions about past work with sources; the
   viewer is at http://127.0.0.1:37777.

Do not edit Claude Code settings yourself; `rvn install` does that and backs up each
file it changes. Windows needs WSL.
