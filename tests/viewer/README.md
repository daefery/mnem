# Viewer checks

Browser checks that every feature of the web viewer (`ui/`) still works: the feed and
its paging, facts and narrative, summaries and their icons, agent badges, the health
pill, the project filter, search with match labels, the theme, the session-start
preview, the backup window, live updates, a phone-width layout and no console errors.

Run them after any change to `ui/`, from the repository root:

```sh
cargo build --release
cd tests/viewer && npm ci && sh run.sh
```

`run.sh` builds a demo record in a throwaway home and checks a viewer served from it;
nothing in `~/.ravnori` is read or changed. It needs Node, Python 3 and Chrome (or set
`RAVNORI_CHROME` to another Chromium binary).

To show that a change altered only the look, run both builds on the same record with a
fingerprint path and compare the files: they must be identical.

```sh
RAVNORI_BIN=/path/to/old/rvn sh run.sh /tmp/old.json
sh run.sh /tmp/new.json
cmp /tmp/old.json /tmp/new.json
```

`features.js` also runs against any viewer that is already up, such as your own:
`node features.js http://127.0.0.1:37777`. It only reads.
