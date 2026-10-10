//! The runit importer, end to end: `scripts/runit-import.sh` over a fixture
//! runsvdir, byte-compared outputs, every output re-parsed by `zconfig`.
//!
//! The script is POSIX sh and the assertions are order-free where the shell
//! leaves order unspecified (glob expansion): file contents are exact,
//! warnings are contains-checks. Runs under `sh` so the same test covers
//! dash and busybox ash wherever CI provides them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("runit-sv")
}

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("runit-import.sh")
}

fn scratch_out() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("zinit-import-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn read(name: &Path) -> String {
    std::fs::read_to_string(name).expect("read output")
}

/// Run the importer; return `(exit code, stderr)`.
fn import(sv: &Path, out: &Path) -> (i32, String) {
    let output = Command::new("sh")
        .arg(script_path())
        .arg(sv)
        .arg(out)
        .output()
        .expect("run sh");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn full_service_converts_exactly() {
    let out = scratch_out();
    let (code, _) = import(&fixture_dir(), &out);
    assert_eq!(code, 0);
    let f = fixture_dir().display().to_string();
    let want = format!(
        "type = script\ncommand = cd '{f}/web' && ./run; code=$?; exec ./finish $code 0\nready = ping:{f}/web/check\nlog = file:/var/log/web/web.log\nrestart = always\n"
    );
    let got = read(&out.join("web.conf"));
    assert_eq!(got, want);
    // And the output governs: what the importer writes, the parser takes.
    let desc = zconfig::parse_service("web", &got).expect("generated output must parse");
    assert_eq!(desc.command, format!("cd '{f}/web' && ./run; code=$?; exec ./finish $code 0"));
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn down_file_becomes_enabled_no_with_explicit_sink() {
    let out = scratch_out();
    let (code, _) = import(&fixture_dir(), &out);
    assert_eq!(code, 0);
    let f = fixture_dir().display().to_string();
    let want = format!(
        "type = script\ncommand = cd '{f}/worker' && exec ./run\nready = none\nlog = none\nrestart = always\nenabled = no\n"
    );
    assert_eq!(read(&out.join("worker.conf")), want);
    let desc =
        zconfig::parse_service("worker", &read(&out.join("worker.conf"))).expect("must parse");
    assert!(!desc.enabled);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn broken_services_are_skipped_loudly() {
    let out = scratch_out();
    let (code, stderr) = import(&fixture_dir(), &out);
    // Skips are warnings, not failures: the convertible services converted.
    assert_eq!(code, 0);
    assert!(out.join("web.conf").exists());
    assert!(out.join("worker.conf").exists());
    assert!(!out.join("broken.conf").exists());
    assert!(!out.join("cache.conf").exists());
    for needle in [
        "broken: no ./run, skipped",
        "cache: ./run is not executable, skipped",
        "odd service name, skipped",
        "web: ./conf is sourced shell, not imported",
    ] {
        assert!(stderr.contains(needle), "missing warning `{needle}`:\n{stderr}");
    }
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn usage_and_missing_dirs_fail() {
    let out = scratch_out();
    let missing = out.join("no-such-runsvdir");
    let status = Command::new("sh")
        .arg(script_path())
        .arg(&missing)
        .arg(&out)
        .status()
        .expect("run sh");
    assert_eq!(status.code(), Some(1));
    let status = Command::new("sh")
        .arg(script_path())
        .status()
        .expect("run sh");
    assert_eq!(status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&out);
}
