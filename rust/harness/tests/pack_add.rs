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

    let archive = work.join("dist").join("ledger-1.0.0.hadepack");
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
        source: None,
        out: Some(archive.clone()),
    };
    harness::pack::export(&options).expect("an archive");
    let bytes = std::fs::read(&archive).expect("the archive");
    let digest = sha256(&bytes);

    // The shelf: an index naming the archive and its digest, served from a loopback port.
    let shelf_root = work.join("shelf");
    std::fs::create_dir_all(shelf_root.join("dist")).expect("a shelf");
    std::fs::copy(&archive, shelf_root.join("dist/ledger-1.0.0.hadepack"))
        .expect("the archive copy");
    let index = |digest: &str| {
        format!(
            r#"{{"menu":1,"packs":[{{"id":"ledger","version":"1.0.0","title":"Ledger",
            "summary":"Keep a ledger that balances.","license":"Commercial","derivatives":"forbidden",
            "keywords":["books"],"kind":["skill"],"items":[{{"kind":"skill","name":"ledger"}}],
            "requires_hades":">=0.1.0,<0.2","bytes":{},"sha256":"{digest}",
            "archive":"dist/ledger-1.0.0.hadepack"}}]}}"#,
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

    let planned = packs_install::add("ledger", None, &home, &state, false).expect("a plan");
    assert!(planned.contains("install"), "{planned}");
    assert!(
        !state.join("skills/proposed/ledger").exists(),
        "a plan wrote a skill"
    );

    let applied = packs_install::add("ledger", None, &home, &state, true).expect("an install");
    assert!(applied.contains("applied: 1 item"), "{applied}");
    assert!(
        state.join("skills/ledger/SKILL.md").is_file(),
        "the skill did not land: {applied}"
    );

    // A digest that does not match is refused before the archive is read, and nothing is written.
    std::fs::write(shelf_root.join("INDEX.json"), index(&"0".repeat(64))).expect("a bad index");
    let refused =
        packs_install::add("ledger", None, &home, &state, true).expect_err("a mismatched digest");
    assert!(refused.contains("hashes to"), "{refused}");
    assert!(refused.contains("nothing was written"), "{refused}");

    let _ = std::fs::remove_dir_all(&work);
}

/// The other half of sharing: a file arrives with no shelf behind it. The digest is what the sender said in
/// the message, and it is checked before the archive is read.
#[test]
fn a_file_that_arrived_is_checked_against_the_digest_you_were_given() {
    let work = std::env::temp_dir().join(format!("hades-file-add-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let source = work.join("source");
    std::fs::create_dir_all(source.join("skills/letter")).expect("a source");
    std::fs::write(
        source.join("skills/letter/SKILL.md"),
        "---\nname: letter\ndescription: Use when the words have to survive the trip.\ntags: writing\n---\n\n# Letter\n",
    )
    .expect("a skill");

    let archive = work.join("letter-1.0.0.hadepack");
    let options = ExportOptions {
        id: "letter".to_string(),
        version: "1.0.0".to_string(),
        from: source.clone(),
        title: "Letter".to_string(),
        summary: "Write a letter that survives.".to_string(),
        license: "MIT".to_string(),
        derivatives: "allowed".to_string(),
        network: false,
        writes: Vec::new(),
        requires_roles: Vec::new(),
        requires_tools: Vec::new(),
        skills: vec!["letter".to_string()],
        profiles: Vec::new(),
        cases: Vec::new(),
        keywords: vec!["writing".to_string()],
        source: None,
        out: Some(archive.clone()),
    };
    harness::pack::export(&options).expect("an archive");
    let digest = sha256(&std::fs::read(&archive).expect("the archive"));

    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");

    let refused = packs_install::add_file(&archive, Some(&"0".repeat(64)), &home, &state, true)
        .expect_err("a digest the sender did not send");
    assert!(
        refused.contains("does not match the digest you gave"),
        "{refused}"
    );
    assert!(
        !state.join("skills/letter").exists(),
        "a refused file was installed"
    );

    let applied = packs_install::add_file(&archive, Some(&digest), &home, &state, true)
        .expect("an install from a file");
    assert!(applied.contains("applied: 1 item"), "{applied}");
    assert!(state.join("skills/letter/SKILL.md").is_file(), "{applied}");

    let _ = std::fs::remove_dir_all(&work);
}

/// A digest the operator names is a pin on bytes. If the shelf names something else, the installation is
/// refused; the failure mode this forbids is installing a different archive and reporting success.
#[test]
fn a_pinned_digest_for_a_shelf_pack_is_not_ignored() {
    let work = std::env::temp_dir().join(format!("hades-pin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");
    let served = work.join("shelf");
    std::fs::create_dir_all(&served).expect("a shelf");

    // A shelf that answers, so the refusal is the pin's and not the network's.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let url = format!("http://{}/", listener.local_addr().expect("an address"));
    let index = r#"{"menu":1,"packs":[{"id":"ledger","version":"1.0.0","title":"Ledger",
        "summary":"Keep a ledger.","license":"MIT","derivatives":"allowed","keywords":[],
        "kind":["skill"],"items":[{"kind":"skill","name":"ledger"}],
        "requires_hades":">=0.1.0,<0.2","bytes":10,"sha256":"1111111111111111111111111111111111111111111111111111111111111111",
        "archive":"dist/ledger-1.0.0.hadepack"}]}"#;
    std::fs::write(served.join("INDEX.json"), index).expect("an index");
    let _server = shelf(listener, served.clone());
    // SAFETY: this test binary sets these before any fetch and does not run concurrently with the others.
    unsafe {
        std::env::set_var("HADES_PACKS_URL", &url);
        std::env::set_var("HADES_PACKS_ALLOW_PRIVATE", "1");
    }

    let refused = packs_install::add("ledger", Some(&"0".repeat(64)), &home, &state, true)
        .expect_err("a pin the shelf disagrees with");
    assert!(refused.contains("you asked for"), "{refused}");
    assert!(refused.contains("Nothing was fetched"), "{refused}");
    let _ = std::fs::remove_dir_all(&work);
}

/// Two installations into one state at the same time. Each must get its own staging file, and each installed
/// skill must be the content of its own archive.
#[test]
fn two_installs_into_one_state_do_not_share_a_staging_file() {
    let work = std::env::temp_dir().join(format!("hades-concurrent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");

    let mut archives = Vec::new();
    for name in ["alpha", "beta"] {
        let source = work.join(format!("src-{name}"));
        std::fs::create_dir_all(source.join("skills").join(name)).expect("a source");
        std::fs::write(
            source.join("skills").join(name).join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Use when {name} is the one.\n---\n\n# {name}\n"
            ),
        )
        .expect("a skill");
        let out = work.join(format!("{name}.hadepack"));
        let options = ExportOptions {
            id: name.to_string(),
            version: "1.0.0".to_string(),
            from: source.clone(),
            title: name.to_string(),
            summary: format!("The {name} pack."),
            license: "MIT".to_string(),
            derivatives: "allowed".to_string(),
            network: false,
            writes: Vec::new(),
            requires_roles: Vec::new(),
            requires_tools: Vec::new(),
            skills: vec![name.to_string()],
            profiles: Vec::new(),
            cases: Vec::new(),
            keywords: Vec::new(),
            source: None,
            out: Some(out.clone()),
        };
        harness::pack::export(&options).expect("an archive");
        archives.push(out);
    }

    let mut threads = Vec::new();
    for archive in archives.clone() {
        let home = home.clone();
        let state = state.clone();
        threads.push(std::thread::spawn(move || {
            packs_install::add_file(&archive, None, &home, &state, true)
        }));
    }
    for thread in threads {
        thread.join().expect("a worker").expect("an install");
    }

    assert!(state.join("skills/alpha/SKILL.md").is_file());
    assert!(state.join("skills/beta/SKILL.md").is_file());
    let cached: Vec<_> = std::fs::read_dir(state.join("packs/archives"))
        .expect("a cache")
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "hadepack")
        })
        .collect();
    assert_eq!(cached.len(), 2, "one cache entry per digest: {cached:?}");
    let _ = std::fs::remove_dir_all(&work);
}

/// The format allows `author/tool`; the cache used to build that id into a path.
#[test]
fn a_namespaced_pack_id_installs_and_is_cached_under_its_digest() {
    let work = std::env::temp_dir().join(format!("hades-namespaced-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let source = work.join("src");
    std::fs::create_dir_all(source.join("skills/tool")).expect("a source");
    std::fs::write(
        source.join("skills/tool/SKILL.md"),
        "---\nname: tool\ndescription: Use when a namespaced pack is the one.\n---\n\n# Tool\n",
    )
    .expect("a skill");
    let archive = work.join("tool.hadepack");
    let options = ExportOptions {
        id: "author/tool".to_string(),
        version: "1.0.0".to_string(),
        from: source.clone(),
        title: "Tool".to_string(),
        summary: "A namespaced pack.".to_string(),
        license: "MIT".to_string(),
        derivatives: "allowed".to_string(),
        network: false,
        writes: Vec::new(),
        requires_roles: Vec::new(),
        requires_tools: Vec::new(),
        skills: vec!["tool".to_string()],
        profiles: Vec::new(),
        cases: Vec::new(),
        keywords: Vec::new(),
        source: None,
        out: Some(archive.clone()),
    };
    harness::pack::export(&options).expect("an archive");
    let digest = sha256(&std::fs::read(&archive).expect("the archive"));

    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");
    packs_install::add_file(&archive, None, &home, &state, true).expect("a namespaced install");
    assert!(state.join("skills/tool/SKILL.md").is_file());
    assert!(
        state
            .join("packs/archives")
            .join(format!("{digest}.hadepack"))
            .is_file()
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// A local import is read through a limit, not after one.
#[test]
fn an_oversized_local_archive_is_refused_before_it_is_read() {
    let work = std::env::temp_dir().join(format!("hades-oversize-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("a work dir");
    let huge = work.join("huge.hadepack");
    let file = std::fs::File::create(&huge).expect("a file");
    file.set_len(64 * 1024 * 1024).expect("a large file");
    drop(file);

    let refused = packs_install::add_file(&huge, None, &work, &work, true)
        .expect_err("an archive past the ceiling");
    assert!(refused.contains("over the"), "{refused}");
    assert!(refused.contains("Nothing was read"), "{refused}");
    let _ = std::fs::remove_dir_all(&work);
}

/// A pack that is already installed should be a skip. It was a conflict, because the installed skill carries
/// the installer's own frontmatter and can never hash to the archive.
#[test]
fn installing_the_same_pack_twice_is_a_skip() {
    let work = std::env::temp_dir().join(format!("hades-idempotent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let source = work.join("src");
    std::fs::create_dir_all(source.join("skills/once")).expect("a source");
    std::fs::write(
        source.join("skills/once/SKILL.md"),
        "---\nname: once\ndescription: Use when once is enough.\n---\n\n# Once\n",
    )
    .expect("a skill");
    let archive = work.join("once.hadepack");
    let options = ExportOptions {
        id: "once".to_string(),
        version: "1.0.0".to_string(),
        from: source.clone(),
        title: "Once".to_string(),
        summary: "Install me twice.".to_string(),
        license: "MIT".to_string(),
        derivatives: "allowed".to_string(),
        network: false,
        writes: Vec::new(),
        requires_roles: Vec::new(),
        requires_tools: Vec::new(),
        skills: vec!["once".to_string()],
        profiles: Vec::new(),
        cases: Vec::new(),
        keywords: Vec::new(),
        source: None,
        out: Some(archive.clone()),
    };
    harness::pack::export(&options).expect("an archive");

    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");
    packs_install::add_file(&archive, None, &home, &state, true).expect("a first install");
    let again =
        packs_install::add_file(&archive, None, &home, &state, true).expect("a second install");
    assert!(again.contains("already installed"), "{again}");
    let _ = std::fs::remove_dir_all(&work);
}

/// An install killed between moving the old skill aside and putting the new one in place leaves a catalog row
/// whose path is gone. Installing again is how that is repaired, and it has to be allowed.
#[test]
fn a_catalog_row_whose_skill_is_gone_is_repaired_by_installing_again() {
    let work = std::env::temp_dir().join(format!("hades-repair-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let source = work.join("src");
    std::fs::create_dir_all(source.join("skills/mend")).expect("a source");
    std::fs::write(
        source.join("skills/mend/SKILL.md"),
        "---\nname: mend\ndescription: Use when something has to be put back.\n---\n\n# Mend\n",
    )
    .expect("a skill");
    let archive = work.join("mend.hadepack");
    let options = ExportOptions {
        id: "mend".to_string(),
        version: "1.0.0".to_string(),
        from: source.clone(),
        title: "Mend".to_string(),
        summary: "Put it back.".to_string(),
        license: "MIT".to_string(),
        derivatives: "allowed".to_string(),
        network: false,
        writes: Vec::new(),
        requires_roles: Vec::new(),
        requires_tools: Vec::new(),
        skills: vec!["mend".to_string()],
        profiles: Vec::new(),
        cases: Vec::new(),
        keywords: Vec::new(),
        source: None,
        out: Some(archive.clone()),
    };
    harness::pack::export(&options).expect("an archive");

    let home = work.join("home");
    let state = work.join("state");
    std::fs::create_dir_all(&home).expect("a home");
    std::fs::create_dir_all(&state).expect("a state");
    packs_install::add_file(&archive, None, &home, &state, true).expect("a first install");

    // What a kill between the two renames leaves: the catalog row stays, the live path is gone.
    std::fs::remove_dir_all(state.join("skills/mend")).expect("the crash window");
    let repaired = packs_install::add_file(&archive, None, &home, &state, true).expect("a repair");
    assert!(state.join("skills/mend/SKILL.md").is_file(), "{repaired}");
    let _ = std::fs::remove_dir_all(&work);
}
