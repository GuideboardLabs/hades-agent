fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for file in [".git/HEAD", ".git/packed-refs"] {
        println!("cargo:rerun-if-changed={}", root.join(file).display());
    }
    if let Ok(output) = std::process::Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(&root)
        .output()
        && output.status.success()
    {
        let reference = String::from_utf8_lossy(&output.stdout);
        println!(
            "cargo:rerun-if-changed={}",
            root.join(".git").join(reference.trim()).display()
        );
    }
    let revision = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=HADES_BUILD_REVISION={revision}");
}
