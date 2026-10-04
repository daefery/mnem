#!/usr/bin/env python3
"""Build a demo mnem database with made-up sessions, for screenshots and trying mnem.

    mnem --db /tmp/mnem-demo.db doctor >/dev/null   # create the schema with mnem itself
    python3 docs/demo/make-demo.py /tmp/mnem-demo.db
    mnem --db /tmp/mnem-demo.db ui --port 37800

Everything here is fictional: the project (acme/shop-api), the sessions, the people.
Times are relative to now, so the feed always reads as recent work.
"""
import json
import sqlite3
import sys
import time

db = sys.argv[1] if len(sys.argv) > 1 else "/tmp/mnem-demo.db"
c = sqlite3.connect(db)
now = int(time.time() * 1000)
H = 3_600_000
PROJECT = "github.com/acme/shop-api"

sessions = [
    # id, agent, title, hours ago (start), branch
    ("claude:demo-1", "claude", "Fix duplicate charges from payment webhooks", 96, "fix/webhook-dupes"),
    ("codex:demo-2", "codex", "Retry storm against the rates API", 70, "main"),
    ("pi:demo-3", "pi", "Plan the orders table migration", 50, "main"),
    ("claude:demo-4", "claude", "Ship the orders migration", 26, "feat/orders-v2"),
    ("pi:demo-5", "pi", "Why are checkout tests flaky on CI?", 5, "main"),
]
turns = {
    "claude:demo-1": [
        ("Stripe sometimes charges a customer twice when the webhook is retried. Find out why and fix it.",
         "The webhook handler has no idempotency check: a retried `payment_intent.succeeded` creates a second order. I added a unique `stripe_event_id` column and skip events already processed, with a test that replays the same event twice.",
         ["src/webhooks/stripe.ts", "migrations/0042_stripe_event_id.sql", "test/webhooks.test.ts"],
         ["npm test -- webhooks", "git commit -m 'Ignore replayed Stripe events'"]),
    ],
    "codex:demo-2": [
        ("The rates API is returning 429s and our sync job makes it worse. Back off properly.",
         "The sync retried immediately in a tight loop. It now backs off exponentially (250 ms doubling, capped at 30 s, with jitter) and honours Retry-After. A burst of 429s now costs 6 calls instead of 400.",
         ["src/clients/rates.ts", "test/rates.test.ts"],
         ["npm test -- rates"]),
    ],
    "pi:demo-3": [
        ("We need to split orders into orders and order_items. What's the safest deploy order?",
         "Run the migration before the workers deploy: the new workers read order_items, the old ones ignore it. Backfill in batches of 5,000 with a resumable cursor, then switch reads behind the ORDERS_V2 flag.",
         ["docs/migrations/orders-v2.md"],
         []),
    ],
    "claude:demo-4": [
        ("Ship the orders migration we planned yesterday.",
         "Migration 0043 adds order_items; the backfill job resumes from its cursor and finished 1.2 M rows in 9 minutes. Reads switched behind ORDERS_V2; the old column stays until next week.",
         ["migrations/0043_order_items.sql", "src/jobs/backfill-order-items.ts", "src/orders/repository.ts"],
         ["npm run migrate", "npm test"]),
    ],
    "pi:demo-5": [
        ("Why are the checkout tests flaky on CI?",
         "They share one Postgres schema and run in parallel; two tests truncate `carts` while a third reads it. Each test file now gets its own schema, and the suite passed 50 runs in a row.",
         ["test/setup/db.ts", "vitest.config.ts"],
         ["npx vitest run --repeat 50"]),
    ],
}
memories = {
    "claude:demo-1": [
        ("bugfix", "Replayed Stripe webhooks no longer create duplicate orders",
         "Retried payment_intent.succeeded events were processed twice; orders now store stripe_event_id with a unique index and replays are skipped.",
         ["orders.stripe_event_id is unique (migration 0042)", "src/webhooks/stripe.ts skips events already in orders"],
         ["src/webhooks/stripe.ts", "migrations/0042_stripe_event_id.sql"]),
    ],
    "codex:demo-2": [
        ("bugfix", "Rates sync backs off on 429 and honours Retry-After",
         "The sync job retried instantly in a loop under rate limiting. It now waits 250 ms doubling to 30 s with jitter, and Retry-After wins when present.",
         ["Backoff: 250 ms base, x2, 30 s cap, full jitter", "A 429 burst costs 6 calls instead of 400"],
         ["src/clients/rates.ts"]),
    ],
    "pi:demo-3": [
        ("decision", "Deploy order for orders v2: migration first, then workers",
         "New workers read order_items and old ones ignore it, so the migration must land first. Backfill runs in resumable batches of 5,000; reads switch behind ORDERS_V2.",
         ["Order: migrate, deploy workers, backfill, flip ORDERS_V2", "Keep orders.items until a week after the flip"],
         []),
    ],
    "claude:demo-4": [
        ("feature", "orders v2 shipped: order_items table and resumable backfill",
         "Migration 0043 added order_items; the backfill resumes from a stored cursor and moved 1.2 M rows in 9 minutes. Reads go through ORDERS_V2.",
         ["Backfill cursor lives in jobs.cursor (job backfill-order-items)", "1.2 M rows in 9 min"],
         ["migrations/0043_order_items.sql", "src/jobs/backfill-order-items.ts"]),
    ],
    "pi:demo-5": [
        ("discovery", "Checkout tests were flaky because they shared one schema",
         "Parallel test files truncated carts while others read it. Each file now gets its own Postgres schema; 50 consecutive CI runs passed.",
         ["test/setup/db.ts creates schema test_<file hash>", "vitest runs files in parallel"],
         ["test/setup/db.ts"]),
    ],
}

for sid, agent, title, ago, branch in sessions:
    start = now - ago * H
    c.execute(
        "INSERT OR REPLACE INTO sessions(id, agent, native_id, project, cwd, git_branch, title, started_at, last_event_at) VALUES (?,?,?,?,?,?,?,?,?)",
        (sid, agent, sid.split(":")[1], PROJECT, "/home/dev/shop-api", branch, title, start, start + 40 * 60_000),
    )
    t = start
    for n, (prompt, answer, files, commands) in enumerate(turns[sid], 1):
        def ev(kind, text=None, path=None, tool=None, step=60_000):
            nonlocal_t[0] += step
            c.execute(
                "INSERT INTO events(session_id, record_key, kind, text, path, tool, turn, ts) VALUES (?,?,?,?,?,?,?,?)",
                (sid, f"{sid}:{kind}:{nonlocal_t[0]}", kind, text, path, tool, n, nonlocal_t[0]),
            )
            return c.execute("SELECT last_insert_rowid()").fetchone()[0]
        nonlocal_t = [t]
        p = ev("prompt", prompt, step=0)
        reads = [ev("file_read", path=f, tool="Read") for f in files[:1]]
        edits = [ev("file_edit", path=f, tool="Edit") for f in files]
        cmds = [ev("command", cmd, tool="Bash") for cmd in commands]
        a = ev("assistant", answer)
        for ty, mt, narrative, facts, mfiles in memories[sid]:
            c.execute(
                "INSERT OR IGNORE INTO memories(session_id, project, kind, type, title, narrative, facts, concepts, files_read, files_modified, origin, origin_id, model, created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (sid, PROJECT, "observation", ty, mt, narrative, json.dumps(facts), json.dumps(["what-changed"]),
                 json.dumps(files[:1]), json.dumps(mfiles), "mnem", f"{sid}@{p}-{a}#0", "sonnet", nonlocal_t[0] + 120_000),
            )
            mid = c.execute("SELECT id FROM memories WHERE origin = 'mnem' AND origin_id = ?", (f"{sid}@{p}-{a}#0",)).fetchone()[0]
            for e in [p] + edits[:2] + [a]:
                c.execute("INSERT OR IGNORE INTO memory_evidence(memory_id, event_id) VALUES (?,?)", (mid, e))
        c.execute(
            "INSERT OR IGNORE INTO memories(session_id, project, kind, type, title, narrative, origin, origin_id, model, created_at, data) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            (sid, PROJECT, "summary", None, prompt, answer, "mnem", f"{sid}@{p}-{a}#s", "sonnet", nonlocal_t[0] + 150_000,
             json.dumps({"request": prompt, "completed": answer})),
        )
c.execute(
    "INSERT OR IGNORE INTO memories(project, kind, type, title, narrative, origin, origin_id, created_at) VALUES (?,?,?,?,?,?,?,?)",
    (PROJECT, "pinned", "decision", "Never call the live Stripe API from tests; use the recorded fixtures in test/fixtures/stripe.",
     "Never call the live Stripe API from tests; use the recorded fixtures in test/fixtures/stripe.", "mnem", "pin-demo-1", now - 80 * H),
)
c.commit()
print(f"demo record in {db}: {c.execute('SELECT count(*) FROM sessions').fetchone()[0]} sessions, "
      f"{c.execute('SELECT count(*) FROM events').fetchone()[0]} events, {c.execute('SELECT count(*) FROM memories').fetchone()[0]} memories")
