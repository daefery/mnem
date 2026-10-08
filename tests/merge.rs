//! Merging a teammate's backup: every row already here is left exactly as it was, their
//! sessions and memories are added, a project name both sides use is renamed to
//! `<prefix>/<name>` on their side, what was forgotten here stays forgotten, and
//! merging the same backup twice adds nothing. Its own test binary: it sets MNEM_HOME.

use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};

fn home() -> mnem::TempDir {
    mnem::TempDir::new("merge")
}

/// A session with two events and one distilled memory citing both, in `project`.
fn seed(c: &Connection, sid: &str, project: &str, title: &str) -> i64 {
    c.execute(
        "INSERT INTO sessions(id, agent, native_id, project, title, started_at, last_event_at)
         VALUES (?1, 'claude', ?1, ?2, ?3, 1000, 2000)",
        params![sid, project, title],
    )
    .unwrap();
    let mut ids = vec![];
    for (i, text) in ["fix the retry loop", "backoff with jitter now"]
        .iter()
        .enumerate()
    {
        c.execute(
            "INSERT INTO events(session_id, record_key, ts, kind, text, source_path, byte_offset)
             VALUES (?1, ?2, ?3, 'prompt', ?4, '/their/transcript.jsonl', ?5)",
            params![sid, format!("k{i}"), 1000 + i as i64, text, i as i64 * 100],
        )
        .unwrap();
        ids.push(c.last_insert_rowid());
    }
    c.execute(
        "INSERT INTO memories(session_id, project, kind, type, title, narrative, files_modified, origin, origin_id, created_at)
         VALUES (?1, ?2, 'observation', 'bugfix', ?3, 'n', '[\"src/retry.rs\"]', 'mnem', ?4, 1500)",
        params![sid, project, title, format!("{sid}@{}-{}#0", ids[0], ids[1])],
    )
    .unwrap();
    let m = c.last_insert_rowid();
    for e in &ids {
        c.execute(
            "INSERT INTO memory_evidence(memory_id, event_id, event_hash, relation) VALUES (?1, ?2, 'h', 'cited')",
            params![m, e],
        )
        .unwrap();
    }
    m
}

/// Every row of the user-visible tables, as text, ordered: what must not change.
fn fingerprint(c: &Connection) -> Vec<String> {
    let mut out = vec![];
    for (t, order) in [
        ("sessions", "id"),
        ("events", "id"),
        ("memories", "id"),
        ("memory_evidence", "memory_id, event_id"),
        ("distill_state", "session_id"),
        ("forgotten", "kind, key"),
    ] {
        let mut st = c
            .prepare(&format!("SELECT * FROM {t} ORDER BY {order}"))
            .unwrap();
        let n = st.column_count();
        let rows = st
            .query_map([], |r| {
                Ok((0..n)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap();
        for r in rows {
            out.push(format!("{t}: {}", r.unwrap()));
        }
    }
    out
}

fn snapshot_of(src: &Path, dir: &Path) -> PathBuf {
    let c = mnem::db::open(src).unwrap();
    let m = mnem::backup::create(&c, dir, 7).unwrap();
    dir.join(m.file)
}

#[test]
fn a_teammate_merge_adds_theirs_and_leaves_mine_untouched() {
    let home = home();
    // SAFETY: set before any other thread starts.
    unsafe { std::env::set_var("MNEM_HOME", &home) };

    // Mine: a project we share, one only I have, a pin, a forgotten session of theirs.
    let mine_path = home.join("mnem.db");
    let mine = mnem::db::open(&mine_path).unwrap();
    seed(
        &mine,
        "claude:mine-1",
        "github.com/acme/shop",
        "My retry fix",
    );
    seed(
        &mine,
        "claude:mine-2",
        "github.com/me/private",
        "My private work",
    );
    mnem::forget::remember(
        &mine,
        "Run tests with --frozen",
        Some("github.com/acme/shop"),
    )
    .unwrap();
    mine.execute(
        "INSERT INTO forgotten(kind, key, at) VALUES ('session', 'claude:theirs-gone', 1)",
        [],
    )
    .unwrap();
    // One event of hers I forgot earlier (her transcript line, seen here before).
    mine.execute(
        "INSERT INTO forgotten(kind, key, at) VALUES ('event', 'claude:theirs-2|k1', 1)",
        [],
    )
    .unwrap();
    let before = fingerprint(&mine);

    // Theirs (Ana's machine): the shared project, one only she has, a session I forgot,
    // a pin of hers, and a copy of one of my sessions (same id: skipped).
    let theirs_dir = home.join("ana");
    std::fs::create_dir_all(&theirs_dir).unwrap();
    let theirs_path = theirs_dir.join("ana.db");
    {
        let t = mnem::db::open(&theirs_path).unwrap();
        seed(
            &t,
            "claude:theirs-1",
            "github.com/acme/shop",
            "Ana found the deadlock",
        );
        seed(&t, "claude:theirs-2", "github.com/ana/tools", "Ana's tool");
        seed(
            &t,
            "claude:theirs-gone",
            "github.com/ana/tools",
            "Forgotten here",
        );
        seed(&t, "claude:mine-1", "github.com/acme/shop", "My retry fix");
        mnem::forget::remember(&t, "Deploy on Tuesdays only", Some("github.com/acme/shop"))
            .unwrap();
    }
    let snap = snapshot_of(&theirs_path, &theirs_dir);
    let backups = home.join("backups");

    let mut conn = mnem::db::open(&mine_path).unwrap();
    let plan = mnem::merge::preview(&snap, &conn, "ana").unwrap();
    assert_eq!(
        plan.renamed,
        vec![(
            "github.com/acme/shop".to_string(),
            "ana/github.com/acme/shop".to_string()
        )]
    );
    assert_eq!(
        (plan.sessions, plan.memories, plan.pins),
        (2, 2, 1),
        "{plan:?}"
    );
    assert_eq!((plan.skipped_sessions, plan.events), (2, 4), "{plan:?}");

    let done = mnem::merge::merge(&snap, &mut conn, &backups, "ana").unwrap();
    assert_eq!(done, plan, "the merge does what the preview said");

    // Every row of mine is exactly as it was.
    let after = fingerprint(&conn);
    for row in &before {
        assert!(after.contains(row), "changed or lost: {row}");
    }
    // A safety snapshot of my memory was taken first.
    assert_eq!(mnem::backup::list(&backups).unwrap().len(), 1);

    // Theirs, added, under the right names.
    fn strings(conn: &Connection, sql: &str) -> Vec<String> {
        let mut st = conn.prepare(sql).unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
    let q = |sql: &str| strings(&mnem::db::open(&mine_path).unwrap(), sql);
    assert_eq!(
        q("SELECT project FROM sessions WHERE id = 'claude:theirs-1'"),
        vec!["ana/github.com/acme/shop"]
    );
    assert_eq!(
        q("SELECT project FROM sessions WHERE id = 'claude:theirs-2'"),
        vec!["github.com/ana/tools"],
        "a project only they have keeps its name"
    );
    assert!(q("SELECT id FROM sessions WHERE id = 'claude:theirs-gone'").is_empty());
    let forgotten_back: i64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE session_id = 'claude:theirs-2' AND record_key = 'k1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        forgotten_back, 0,
        "a forgotten event must not come back through a merge"
    );
    assert_eq!(
        q("SELECT project FROM memories WHERE title = 'Ana found the deadlock'"),
        vec!["ana/github.com/acme/shop"]
    );
    // Her pin lands in her renamed project, not in my session-start context.
    assert_eq!(
        q("SELECT project FROM memories WHERE kind = 'pinned' AND title LIKE 'Deploy%'"),
        vec!["ana/github.com/acme/shop"]
    );
    let my_pins = mnem::forget::pinned(&conn, "github.com/acme/shop").unwrap();
    assert_eq!(
        my_pins.len(),
        1,
        "only my own pin in my project: {my_pins:?}"
    );

    // Their memory's evidence points at the copied events, and its origin id follows.
    let (oid, cited): (String, i64) = conn
        .query_row(
            "SELECT m.origin_id, (SELECT count(*) FROM memory_evidence v JOIN events e ON e.id = v.event_id
                                  WHERE v.memory_id = m.id AND e.session_id = m.session_id)
             FROM memories m WHERE m.title = 'Ana found the deadlock'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        cited, 2,
        "both cited events are linked to the copied events"
    );
    let range = oid
        .rsplit_once('@')
        .unwrap()
        .1
        .split('#')
        .next()
        .unwrap()
        .to_string();
    let (a, b) = range.split_once('-').unwrap();
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM events WHERE session_id = 'claude:theirs-1' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        (a.parse::<i64>().unwrap(), b.parse::<i64>().unwrap()),
        (ids[0], ids[1])
    );

    // Their sessions are history: never re-read from a transcript, never distilled here.
    let live: i64 = conn
        .query_row(
            "SELECT count(*) FROM events WHERE session_id LIKE 'claude:theirs%' AND source_path IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 0);
    let (pending, _) = mnem::distill::pending(&conn).unwrap();
    assert_eq!(pending, 0, "merged sessions must not wait for distillation");

    // Search finds their memory through the full-text index.
    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'deadlock'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);

    // Merging the same backup again adds nothing, and moves nothing: her project that
    // came in under its own name is not renamed now that it is "shared".
    let preview2 = mnem::merge::preview(&snap, &conn, "ana").unwrap();
    assert_eq!(preview2.renamed, plan.renamed, "{preview2:?}");
    let again = mnem::merge::merge(&snap, &mut conn, &backups, "ana").unwrap();
    assert_eq!(
        (again.sessions, again.memories, again.pins),
        (0, 0, 0),
        "{again:?}"
    );

    // A newer backup from her: only the new session comes in, in the same places.
    {
        let t = mnem::db::open(&theirs_path).unwrap();
        seed(
            &t,
            "claude:theirs-3",
            "github.com/ana/tools",
            "Ana's second tool fix",
        );
        seed(
            &t,
            "claude:theirs-4",
            "github.com/acme/shop",
            "Ana fixed refunds",
        );
    }
    let snap2 = snapshot_of(&theirs_path, &theirs_dir);
    let third = mnem::merge::merge(&snap2, &mut conn, &backups, "ana").unwrap();
    assert_eq!((third.sessions, third.memories), (2, 2), "{third:?}");
    assert_eq!(
        q(
            "SELECT project FROM sessions WHERE id IN ('claude:theirs-3', 'claude:theirs-4') ORDER BY id"
        ),
        vec!["github.com/ana/tools", "ana/github.com/acme/shop"]
    );
    for row in &before {
        assert!(
            fingerprint(&conn).contains(row),
            "changed by a later merge: {row}"
        );
    }

    let _ = std::fs::remove_dir_all(&home);
}
