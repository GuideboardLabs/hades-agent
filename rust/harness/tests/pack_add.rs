//! `hades pack add` against a shelf, in one process of its own.
//!
//! This sets `HADES_PACKS_URL` and `HADES_PACKS_ALLOW_PRIVATE`, which are process-wide, so it is an
//! integration test rather than a unit test. The shelf is a loopback server: an index and one archive, both
//! generated here, so nothing depends on a repository or a network.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;

use harness::pack::ExportOptions;
use harness::packs_install;

fn shelf(listener: TcpListener, root: PathBuf) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 2048];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let path = request
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .trim_start_matches('/')
                .to_string();
            let file = root.join(&path);
            let body = std::fs::read(&file).unwrap_or_default();
            let status = if body.is_empty() && !file.is_file() {
                "404 Not Found"
            } else {
                "200 OK"
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        }
    })
}

fn sha256(bytes: &[u8]) -> String {
    mneme::sha256::hex(bytes)
}

#[test]
fn a_pack_is_fetched_verified_and_installed_only_when_asked() {
    let work = std::env::temp_dir().join(format!("hades-pack-add-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let source = work.join("source");
    std::fs::create_dir_all(source.join("skills/ledger")).expect("a source home");
    std::fs::create_dir_all(source.join("skills/ledger")).expect("a skill");
    std::fs::write(
        source.join("skills/ledger/SKILL.md"),
        "---\nname: ledger\ndescription: Use when the books must balance.\nversion: 1\n---\n\n# Ledger\n",
    )
    .expect("a skill file");

    let archive = work.join("dist").join("ledger-1.0.0.pack");
    std::fs::create_dir_all(archive.parent().expect("a dist")).expect("a dist directory");
    let options = ExportOptions {
        id: "ledger".to_string(),
        version: "1.0.0".to_string(),
        from: source.clone(),
        title: "Ledger".to_string(),
        summary: "Keep a ledger that balances.".to_string(),
        license: "Commercial".to_string(),
        derivatives: "forbidden".to_string(),
        network: false,
        writes: Vec::new(),
        requires_roles: Vec::new(),
        requires_tools: Vec::new(),
        skills: vec!["ledger".to_string()],
        profiles: Vec::new(),
        cases: Vec::new(),
        keywords: vec!["books".to_string()],
        out: Some(archive.clone()),
    };
    harness::pack::export(&options).expect("an archive");
    let bytes = std::fs::read(&archive).expect("the archive");
    let digest = sha256(&bytes);

    // The shelf: an index naming the archive and its digest, served from a loopback port.
    let shelf_root = work.join("shelf");
    std::fs::create_dir_all(shelf_root.join("dist")).expect("a shelf");
    std::fs::copy(&archive, shelf_root.join("dist/ledger-1.0.0.pack")).expect("the archive copy");
    let index = |digest: &str| {
        format!(
            r#"{{"menu":1,"packs":[{{"id":"ledger","version":"1.0.0","title":"Ledger",
            "summary":"Keep a ledger that balances.","license":"Commercial","derivatives":"forbidden",
            "keywords":["books"],"kind":["skill"],"items":[{{"kind":"skill","name":"ledger"}}],
            "requires_hades":">=0.1.0,<0.2","bytes":{},"sha256":"{digest}",
            "archive":"dist/ledger-1.0.0.pack"}}]}}"#,
            bytes.len()
        )
    };
    std::fs::write(shelf_root.join("INDEX.json"), index(&digest)).expect("an index");

    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let url = format!("http://{}/", listener.local_addr().expect("an address"));
    let _server = shelf(listener, shelf_root.clone());

    // SAFETY: this test binary runs one test; nothing else reads these variables.
    unsafe {
        std::env::set_var("HADES_PACKS_URL", &url);
        std::env::set_var("HADES_PACKS_ALLOW_PRIVATE", "1");
    }

    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");

    let planned = packs_install::add("ledger", &home, &state, false).expect("a plan");
    assert!(planned.contains("install"), "{planned}");
    assert!(
        !state.join("skills/proposed/ledger").exists(),
        "a plan wrote a skill"
    );

    let applied = packs_install::add("ledger", &home, &state, true).expect("an install");
    assert!(applied.contains("applied: 1 item"), "{applied}");
    assert!(
        state.join("skills/ledger/SKILL.md").is_file(),
        "the skill did not land: {applied}"
    );

    // A digest that does not match is refused before the archive is read, and nothing is written.
    std::fs::write(shelf_root.join("INDEX.json"), index(&"0".repeat(64))).expect("a bad index");
    let refused =
        packs_install::add("ledger", &home, &state, true).expect_err("a mismatched digest");
    assert!(refused.contains("hashes to"), "{refused}");
    assert!(refused.contains("nothing was written"), "{refused}");

    let _ = std::fs::remove_dir_all(&work);
}
