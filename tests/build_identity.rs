//! The build identity `operator-status` carries: the source commit a package
//! was built from, which `packaging/PKGBUILD`'s build() exports to cargo as
//! `IBARA_BUILD`. The real build() runs on throwaway git checkouts with a
//! stand-in `cargo` that records what it was given.
//!
//! Failure cases, written before the change:
//! 1. A clean checkout names no commit, or another commit than its own.
//! 2. A modified tracked file is not marked `-dirty`, or an untracked file
//!    (a build's own leftovers) is.
//! 3. A tree that is no checkout of its own, such as a release source
//!    tarball unpacked inside another repository, names that repository's
//!    commit instead of none.
//! 4. An `IBARA_BUILD` in the builder's own environment reaches the build in
//!    place of the checkout's commit.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Dana")
        .env("GIT_AUTHOR_EMAIL", "dana@example.com")
        .env("GIT_COMMITTER_NAME", "Dana")
        .env("GIT_COMMITTER_EMAIL", "dana@example.com")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A core tree at `dir`, with the release settings PKGBUILD reads.
fn core_tree(dir: &Path) {
    fs::create_dir_all(dir.join("packaging")).unwrap();
    fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
    fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/release.env"), dir.join("packaging/release.env")).unwrap();
}

/// The IBARA_BUILD that PKGBUILD's build() gives cargo for the core at
/// `core`, or `None` when it gives none.
fn exported(root: &Path, core: &Path) -> Option<String> {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    fs::write(&cargo, "#!/bin/sh\nprintf '%s' \"${IBARA_BUILD-none}\" > \"$CARGO_SAW\"\n").unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let saw = root.join("cargo-saw");
    let _ = fs::remove_file(&saw);
    let out = Command::new("bash")
        .arg("-c")
        .arg(r#"set -e; source "$1"; build"#)
        .arg("bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/PKGBUILD"))
        .env("IBARA_CORE_DIR", core)
        .env("srcdir", root)
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
        .env("CARGO_SAW", &saw)
        .env("IBARA_BUILD", "from-the-builders-environment")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "build(): {}", String::from_utf8_lossy(&out.stderr));
    Some(fs::read_to_string(&saw).unwrap()).filter(|s| s != "none")
}

#[test]
fn a_package_names_the_commit_it_was_built_from_and_none_it_cannot_know() {
    let root: PathBuf = std::env::temp_dir().join(format!("ibara-build-identity-{}", std::process::id()));
    let core = root.join("core");
    core_tree(&core);
    git(&core, &["init", "-q", "-b", "main"]);
    git(&core, &["add", "."]);
    git(&core, &["commit", "-q", "-m", "First"]);
    let head = git(&core, &["rev-parse", "--short=12", "HEAD"]);

    assert_eq!(exported(&root, &core), Some(head.clone()), "a clean checkout names its own commit, not the builder's variable");
    fs::write(core.join("leftover.log"), "built\n").unwrap();
    assert_eq!(exported(&root, &core), Some(head.clone()), "an untracked file is not a change to the source");
    fs::write(core.join("Cargo.toml"), "[package]\nname = \"changed\"\n").unwrap();
    assert_eq!(exported(&root, &core), Some(format!("{head}-dirty")), "a modified tracked file marks the build");

    let unpacked = core.join("ibara-0.3.7");
    core_tree(&unpacked);
    assert_eq!(exported(&root, &unpacked), None, "a tree inside another checkout is no checkout of its own");
    let plain = root.join("plain");
    core_tree(&plain);
    assert_eq!(exported(&root, &plain), None, "outside any checkout the build is unknown");

    fs::remove_dir_all(&root).unwrap();
}
