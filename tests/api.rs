//! The record API (/v1) over real HTTP: the token is required and compared exactly,
//! lists page by cursor without gaps or repeats, personal details stay out unless asked
//! for, and a memory comes with its evidence. Its own test binary: it sets MNEM_HOME.

use rusqlite::params;
use serde_json::Value;
use std::io::{Read, Write};

fn get(port: u16, path: &str, token: Option<&str>, host: Option<&str>) -> (u16, Value) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let host = host
        .map(str::to_string)
        .unwrap_or(format!("127.0.0.1:{port}"));
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status: u16 = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[test]
fn the_record_api_is_private_paged_and_complete() {
    let home = mnem::TempDir::new("api");
    // SAFETY: set before any other thread starts.
    unsafe { std::env::set_var("MNEM_HOME", &home) };
    let path = home.join("mnem.db");
    let c = mnem::db::open(&path).unwrap();
    for i in 0..5 {
        c.execute(
            "INSERT INTO sessions(id, agent, native_id, project, last_event_at) VALUES (?1, 'pi', ?1, 'proj', ?2)",
            params![format!("pi:s{i}"), 1000 + i],
        )
        .unwrap();
        c.execute(
            "INSERT INTO events(session_id, record_key, kind, text, turn, ts, source_path) VALUES (?1, ?2, 'prompt', ?3, 1, ?4, '/x.jsonl')",
            params![format!("pi:s{i}"), format!("k{i}"), format!("prompt number {i}"), 1000 + i],
        )
        .unwrap();
    }
    let ev: i64 = c
        .query_row("SELECT min(id) FROM events", [], |r| r.get(0))
        .unwrap();
    let mut ids = Vec::new();
    for (title, ty) in [
        ("Retry loop backs off on 429", "bugfix"),
        ("Deploy order: migrations before workers", "decision"),
        ("Home address of the user", "sensitive"),
    ] {
        c.execute(
            "INSERT INTO memories(session_id, project, kind, type, title, narrative, origin, origin_id, created_at)
             VALUES ('pi:s0', 'proj', 'observation', ?1, ?2, 'n', 'mnem', ?2, 2000)",
            params![ty, title],
        )
        .unwrap();
        ids.push(c.last_insert_rowid());
    }
    c.execute(
        "INSERT INTO memory_evidence(memory_id, event_id) VALUES (?1, ?2)",
        params![ids[0], ev],
    )
    .unwrap();
    // A memory whose session edited a file in a repository: one edit still there.
    let repo = home.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let kept_line = "let retries = backoff(attempt);";
    std::fs::write(
        repo.join("src/net.rs"),
        format!("fn f() {{\n    {kept_line}\n}}\n"),
    )
    .unwrap();
    let transcript = home.join("t.jsonl");
    let record = serde_json::json!({ "uuid": "u0", "message": { "content": [{ "type": "tool_use",
        "input": { "file_path": repo.join("src/net.rs"), "old_string": "", "new_string": kept_line } }] } });
    std::fs::write(&transcript, format!("{record}\n")).unwrap();
    c.execute(
        "INSERT INTO sessions(id, agent, native_id, project, cwd, last_event_at) VALUES ('pi:edit', 'pi', 'edit', 'proj2', ?1, 1)",
        [repo.to_string_lossy()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO events(id, session_id, record_key, kind, path, source_path, byte_offset) VALUES (900, 'pi:edit', 'u0:0', 'file_edit', ?1, ?2, 0)",
        params![repo.join("src/net.rs").to_string_lossy(), transcript.to_string_lossy()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO memories(session_id, project, kind, type, title, origin, origin_id, files_modified, created_at)
         VALUES ('pi:edit', 'proj2', 'observation', 'bugfix', 'Backoff added', 'mnem', 'pi:edit@900-900#0', ?1, 3000)",
        [serde_json::json!([repo.join("src/net.rs")]).to_string()],
    )
    .unwrap();
    let edited = c.last_insert_rowid();
    // The same memory as distillation often records it: a repo-relative path.
    c.execute(
        "INSERT INTO memory_files(memory_id, modified, path, name) VALUES (?1, 1, 'src/net.rs', 'net.rs')",
        [edited],
    )
    .unwrap();
    drop(c);

    let port = 39000 + (std::process::id() % 1000) as u16;
    let served = path.clone();
    std::thread::spawn(move || mnem::ui::serve(served, port, || {}));
    std::thread::sleep(std::time::Duration::from_millis(300));
    let token = mnem::api::token().unwrap();
    assert_eq!(token.len(), 64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(mnem::api::token_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the token file is owner-only");
    }

    // No token, a wrong one, or a foreign Host: refused.
    assert_eq!(get(port, "/v1", None, None).0, 401);
    // One character different (never the same character by chance).
    let last = if token.ends_with('0') { '1' } else { '0' };
    let wrong = format!("{}{last}", &token[..63]);
    assert_eq!(get(port, "/v1", Some(&wrong), None).0, 401);
    assert_eq!(
        get(port, "/v1", Some(&token), Some("evil.example:80")).0,
        403
    );

    let (s, meta) = get(port, "/v1", Some(&token), None);
    assert_eq!(s, 200);
    assert_eq!(meta["api"], 1);
    assert_eq!(meta["counts"]["sessions"], 6);

    // Sessions page by cursor: every one exactly once.
    let mut seen = Vec::new();
    let mut after: Option<i64> = None;
    loop {
        let q = match after {
            Some(a) => format!("/v1/sessions?limit=2&after={a}"),
            None => "/v1/sessions?limit=2".into(),
        };
        let (s, page) = get(port, &q, Some(&token), None);
        assert_eq!(s, 200);
        for it in page["items"].as_array().unwrap() {
            seen.push(it["id"].as_str().unwrap().to_string());
        }
        match page["next"].as_i64() {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 6, "{seen:?}");

    let (_, one) = get(port, "/v1/sessions/pi%3As0", Some(&token), None);
    assert_eq!(
        (
            one["id"].as_str(),
            one["events"].as_i64(),
            one["memories"].as_i64()
        ),
        (Some("pi:s0"), Some(1), Some(3))
    );

    // Events filter and page by id.
    let (_, evs) = get(port, "/v1/events?session=pi:s3", Some(&token), None);
    assert_eq!(evs["items"][0]["text"], "prompt number 3");

    // Personal details only when asked for.
    let (_, ms) = get(port, "/v1/memories", Some(&token), None);
    assert_eq!(ms["items"].as_array().unwrap().len(), 3);
    let (_, all) = get(port, "/v1/memories?include=sensitive", Some(&token), None);
    assert_eq!(all["items"].as_array().unwrap().len(), 4);
    assert_eq!(
        get(
            port,
            &format!("/v1/memories/{}", ids[2]),
            Some(&token),
            None
        )
        .0,
        200
    );

    // A memory comes with the events it cites.
    let (_, m) = get(
        port,
        &format!("/v1/memories/{}", ids[0]),
        Some(&token),
        None,
    );
    assert_eq!(m["title"], "Retry loop backs off on 429");
    assert_eq!(m["evidence"][0]["event_id"].as_i64(), Some(ev));
    assert_eq!(m["evidence"][0]["excerpt"], "prompt number 0");

    // Whether the memory's own edits are still in the file.
    let (_, e) = get(port, &format!("/v1/memories/{edited}"), Some(&token), None);
    let files = e["files"].as_array().unwrap();
    assert!(files.len() >= 2, "{e}");
    for f in files {
        assert_eq!(f["edits_kept"]["kept"], 1, "{f}");
        assert_eq!(f["edits_kept"]["intact"], true);
    }

    // Search ranks by words (no embedding model here) and hides personal details.
    let (s, found) = get(port, "/v1/search?q=retry%20backs%20off", Some(&token), None);
    assert_eq!(s, 200);
    assert_eq!(found["items"][0]["id"].as_i64(), Some(ids[0]));
    let (_, none) = get(
        port,
        "/v1/search?q=home%20address%20user",
        Some(&token),
        None,
    );
    assert!(none["items"].as_array().unwrap().is_empty(), "{none}");

    // Bad input is a 400 naming the problem; unknown paths a 404.
    assert_eq!(get(port, "/v1/memories?limit=0", Some(&token), None).0, 400);
    assert_eq!(get(port, "/v1/memories?after=x", Some(&token), None).0, 400);
    assert_eq!(get(port, "/v1/search", Some(&token), None).0, 400);
    assert_eq!(get(port, "/v1/nothing", Some(&token), None).0, 404);
    assert_eq!(get(port, "/v1/memories/999999", Some(&token), None).0, 404);
    let _ = std::fs::remove_dir_all(&home);
}
