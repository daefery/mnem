//! Moving mnem to another machine through the viewer: back up on one, download,
//! upload and import on the other. Its own test binary, because it points MNEM_HOME
//! at scratch directories (the backup folder and config live there).

use rusqlite::{Connection, params};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn home(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mnem-transfer-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn seed(db: &Path, session: &str, title: &str) {
    let c = mnem::db::open(db).unwrap();
    c.execute(
        "INSERT INTO sessions(id, agent, native_id, project) VALUES (?1, 'claude', ?1, 'proj')",
        params![session],
    )
    .unwrap();
    c.execute(
        "INSERT INTO events(session_id, record_key, ts, kind, text) VALUES (?1, 'k1', 1, 'prompt', 'hello there')",
        params![session],
    )
    .unwrap();
    c.execute(
        "INSERT INTO memories(kind, type, title, project, origin, origin_id, created_at) VALUES ('observation', 'bugfix', ?1, 'proj', 'mnem', ?1, 1)",
        params![title],
    )
    .unwrap();
}

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

fn request(port: u16, method: &str, path: &str, extra: &str, body: &[u8]) -> Reply {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n{extra}\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let split = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..split]).into_owned();
    Reply {
        status: head[9..12].parse().unwrap(),
        head,
        body: out[split + 4..].to_vec(),
    }
}

fn json(r: &Reply) -> serde_json::Value {
    serde_json::from_slice(&r.body)
        .unwrap_or_else(|_| panic!("{} {}", r.head, String::from_utf8_lossy(&r.body)))
}

fn serve(db: PathBuf, port: u16) {
    std::thread::spawn(move || mnem::ui::serve(db, port, || {}));
    std::thread::sleep(std::time::Duration::from_millis(300));
}

const OK: &str = "X-Mnem: 1\r\n";

#[test]
fn back_up_download_and_import_on_another_machine() {
    // Machine A: some history and its own settings.
    let a = home("a");
    unsafe { std::env::set_var("MNEM_HOME", &a) };
    std::fs::write(a.join("config.json"), r#"{"harness_prompts":["^from-a"]}"#).unwrap();
    let db_a = a.join("mnem.db");
    seed(&db_a, "claude:old", "Backup restore stages the database");
    let port_a = 39000 + (std::process::id() % 1000) as u16;
    serve(db_a.clone(), port_a);

    // Changing actions need the viewer's header and origin.
    assert_eq!(request(port_a, "POST", "/api/backups", "", b"").status, 403);
    let evil = "X-Mnem: 1\r\nOrigin: http://evil.example\r\n";
    assert_eq!(
        request(port_a, "POST", "/api/backups", evil, b"").status,
        403
    );
    let cross = "X-Mnem: 1\r\nSec-Fetch-Site: cross-site\r\n";
    assert_eq!(
        request(port_a, "POST", "/api/backups", cross, b"").status,
        403
    );

    let made = request(port_a, "POST", "/api/backups", OK, b"");
    assert_eq!(made.status, 200, "{}", String::from_utf8_lossy(&made.body));
    let listed = json(&request(port_a, "GET", "/api/backups", "", b""));
    let file = listed["backups"][0]["file"].as_str().unwrap().to_string();
    assert_eq!(listed["backups"][0]["has_settings"], true);
    assert_eq!(listed["backups"][0]["memories"], 1);

    let got = request(port_a, "GET", &format!("/api/backups/{file}"), "", b"");
    assert_eq!(got.status, 200);
    assert!(
        got.head.contains(&format!("filename=\"{file}\"")),
        "{}",
        got.head
    );
    assert_eq!(
        got.body,
        std::fs::read(a.join("backups").join(&file)).unwrap()
    );
    for bad in [
        "/api/backups/..%2Fmnem.db",
        "/api/backups/../mnem.db",
        "/api/backups/mnem.db",
    ] {
        assert_eq!(request(port_a, "GET", bad, "", b"").status, 404, "{bad}");
    }

    // Machine B: a fresh install with one session of its own and default settings.
    let b = home("b");
    unsafe { std::env::set_var("MNEM_HOME", &b) };
    std::fs::write(b.join("config.json"), "{}").unwrap();
    let db_b = b.join("mnem.db");
    seed(&db_b, "claude:new", "Viewer colours changed");
    let port_b = port_a + 1;
    serve(db_b.clone(), port_b);

    // Anything that is not a mnem database is refused before it can do harm.
    let junk = request(port_b, "POST", "/api/import", OK, b"definitely not sqlite");
    assert_eq!(junk.status, 422, "{}", String::from_utf8_lossy(&junk.body));

    let preview = request(port_b, "POST", "/api/import", OK, &got.body);
    assert_eq!(
        preview.status,
        200,
        "{}",
        String::from_utf8_lossy(&preview.body)
    );
    let p = json(&preview);
    assert_eq!(p["backup"]["memories"], 1);
    assert_eq!(p["origin"]["has_settings"], true);
    assert_eq!(p["current"]["sessions"], 1);
    // Nothing changed yet.
    let title: String = Connection::open(&db_b)
        .unwrap()
        .query_row("SELECT title FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "Viewer colours changed");

    let upload = p["file"].as_str().unwrap();
    assert_eq!(
        request(
            port_b,
            "POST",
            "/api/import/apply?file=../../mnem.db",
            OK,
            b""
        )
        .status,
        400
    );
    let applied = request(
        port_b,
        "POST",
        &format!("/api/import/apply?file={upload}&settings=1"),
        OK,
        b"",
    );
    assert_eq!(
        applied.status,
        200,
        "{}",
        String::from_utf8_lossy(&applied.body)
    );
    assert_eq!(json(&applied)["settings_applied"], true);

    let c = Connection::open(&db_b).unwrap();
    let title: String = c
        .query_row("SELECT title FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "Backup restore stages the database");
    let from: String = c
        .query_row("SELECT id FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(from, "claude:old");
    // B's own memory was saved first, and its settings kept aside.
    assert_eq!(
        std::fs::read_to_string(b.join("config.json")).unwrap(),
        r#"{"harness_prompts":["^from-a"]}"#
    );
    let names: Vec<String> = std::fs::read_dir(&b)
        .unwrap()
        .chain(std::fs::read_dir(b.join("backups")).unwrap())
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("config.json.bak-")),
        "{names:?}"
    );
    assert!(
        names
            .iter()
            .any(|n| n.starts_with("mnem-") && n.ends_with(".db")),
        "{names:?}"
    );
    // The upload is gone once applied.
    assert_eq!(
        std::fs::read_dir(b.join("backups").join("incoming"))
            .unwrap()
            .count(),
        0
    );

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}
