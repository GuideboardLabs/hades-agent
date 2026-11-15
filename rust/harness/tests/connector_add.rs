//! `hades pack add <connector.tar.gz>`: the file path and the shelf path are one installer, so the
//! gate that protects a connector fetched from the shelf protects one somebody sent as a file.
//!
//! The archive is generated here (a tree plus `SHA256SUMS`, tarred) so nothing depends on a connector
//! build, a model or a network. Each test runs the real binary: the defect lived in the CLI branch that
//! bypassed the installer, so a test that called the installer directly could not see it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn work(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hades-connector-add-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a work dir");
    dir
}

/// A tree the connector installer accepts: a manifest, one executable entrypoint and a whole-tree
/// `SHA256SUMS`. Returns the archive and the sha256 of the archive's own bytes.
fn archive(work: &Path, id: &str) -> (PathBuf, String) {
    let tree = work.join(format!("{id}-tree"));
    std::fs::create_dir_all(tree.join("bin")).expect("bin");
    std::fs::write(tree.join("bin/test"), "#!/bin/sh\necho ok\n").expect("entry");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            tree.join("bin/test"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("mode");
    }
    std::fs::write(tree.join("README.md"), "read me\n").expect("readme");
    let manifest = format!(
        r#"{{"format":1,"kind":"connector","id":"{id}","version":"0.1.0","title":"Test",
           "summary":"A test connector","license":"MIT","protocol":{{"min":1,"max":1}},
           "platform":"{}","entrypoint":"bin/test"}}"#,
        harness::connector_pack::current_platform()
    );
    let sums = format!(
        "{}  bin/test\n{}  README.md\n{}  connector.json\n",
        mneme::sha256::hex(b"#!/bin/sh\necho ok\n"),
        mneme::sha256::hex(b"read me\n"),
        mneme::sha256::hex(manifest.as_bytes())
    );
    std::fs::write(tree.join("SHA256SUMS"), sums).expect("sums");
    std::fs::write(tree.join("connector.json"), manifest).expect("manifest");

    let path = work.join(format!("{id}-0.1.0.tar.gz"));
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&path)
        .arg("-C")
        .arg(&tree)
        .arg(".")
        .status()
        .expect("tar");
    assert!(status.success(), "the fixture archive was not written");
    let digest = mneme::sha256::hex(&std::fs::read(&path).expect("the archive"));
    (path, digest)
}

fn hades(home: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_hades"))
        .args(args)
        .env("HOME", home)
        .env_remove("HADES_PACKS_URL")
        .env_remove("HADES_PACKS_ALLOW_PRIVATE")
        .output()
        .expect("the hades binary runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn state_of(work: &Path) -> (PathBuf, PathBuf) {
    let home = work.join("home");
    std::fs::create_dir_all(&home).expect("a home");
    (home, work.join("state"))
}

/// Without `--yes` a connector archive from a file is a plan: the same rule the shelf path follows.
/// The defect: the file branch called the installer directly, so this wrote the tree and returned.
#[test]
fn a_connector_from_a_file_installs_nothing_without_yes() {
    let work = work("no-yes");
    let (path, _digest) = archive(&work, "test");
    let (home, state) = state_of(&work);
    let (code, out, err) = hades(
        &home,
        &[
            "pack",
            "add",
            path.to_str().expect("utf-8"),
            "--state",
            state.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(code, 0, "stdout: {out}\nstderr: {err}");
    assert!(out.contains("is a connector"), "{out}");
    assert!(out.contains("nothing was written"), "{out}");
    assert!(
        !state.join("connectors").exists(),
        "a connector was installed without --yes: {:?}",
        state.join("connectors")
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// With `--yes` the archive installs; the same bytes pinned by `--digest` install; an archive whose
/// bytes changed after that digest was taken is refused before the state directory is touched.
#[test]
fn a_connector_from_a_file_installs_with_yes_and_refuses_changed_bytes() {
    let work = work("yes");
    let (path, digest) = archive(&work, "test");
    let (home, state) = state_of(&work);
    let state_arg = state.to_str().expect("utf-8").to_string();

    let (code, out, err) = hades(
        &home,
        &[
            "pack",
            "add",
            path.to_str().expect("utf-8"),
            "--digest",
            &digest,
            "--yes",
            "--state",
            &state_arg,
        ],
    );
    assert_eq!(code, 0, "stdout: {out}\nstderr: {err}");
    assert!(out.contains("installed connector test 0.1.0"), "{out}");
    let installed = state.join("connectors/test/0.1.0/bin/test");
    assert!(installed.is_file(), "the connector did not land: {out}");
    let before = std::fs::read(&installed).expect("the installed entrypoint");

    // The bytes a pin describes changed after the sender hashed them.
    let changed = work.join("changed.tar.gz");
    let mut bytes = std::fs::read(&path).expect("the archive");
    bytes.push(b'x');
    std::fs::write(&changed, bytes).expect("the changed archive");
    let (code, out, err) = hades(
        &home,
        &[
            "pack",
            "add",
            changed.to_str().expect("utf-8"),
            "--digest",
            &digest,
            "--yes",
            "--state",
            &state_arg,
        ],
    );
    assert_ne!(code, 0, "a changed archive was installed: {out}");
    assert!(
        out.contains("does not match the digest you gave")
            || err.contains("does not match the digest"),
        "stdout: {out}\nstderr: {err}"
    );
    assert_eq!(
        std::fs::read(&installed).expect("the installed entrypoint"),
        before,
        "a refused archive replaced the installed connector"
    );
    let _ = std::fs::remove_dir_all(&work);
}

/// A pin that does not match refuses before anything is written, on the connector path exactly as on
/// the data path. The defect was the opposite order: install, then notice.
#[test]
fn a_wrong_digest_refuses_a_connector_before_it_is_installed() {
    let work = work("pin");
    let (path, _digest) = archive(&work, "test");
    let (home, state) = state_of(&work);
    let (code, out, err) = hades(
        &home,
        &[
            "pack",
            "add",
            path.to_str().expect("utf-8"),
            "--digest",
            &"0".repeat(64),
            "--yes",
            "--state",
            state.to_str().expect("utf-8"),
        ],
    );
    assert_ne!(code, 0, "a mismatched pin was installed: {out}");
    assert!(
        out.contains("does not match the digest you gave")
            || err.contains("does not match the digest"),
        "stdout: {out}\nstderr: {err}"
    );
    assert!(
        !state.join("connectors").exists(),
        "a refused archive wrote the connector tree: {:?}",
        state.join("connectors")
    );
    let _ = std::fs::remove_dir_all(&work);
}
