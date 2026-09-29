//! Scratch directories for tests: writable space without assumptions.
//!
//! The machine's `/tmp` may be a quota-limited tmpfs (it is on the author's:
//! writes fail with `EDQUOT`), so tests never assume one location. Candidates
//! are tried in order — system temp, `$HOME/.cache`, the crate's
//! `target/test-scratch` — and the first one that survives a real
//! create-write-read round trip wins. A test that cannot prove writability
//! has no business asserting on file contents.

use std::path::PathBuf;

/// A fresh, writable, empty directory for one test.
///
/// The directory is created (previous runs with the same `tag` are removed
/// first) and returned. The caller owns cleanup; a leftover scratch dir is
/// litter, not a failure.
pub(crate) fn scratch(tag: &str) -> PathBuf {
    let pid = std::process::id();
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut sys = std::env::temp_dir();
    sys.push(format!("zservice-{tag}-{pid}"));
    candidates.push(sys);
    if let Ok(home) = std::env::var("HOME") {
        let mut h = PathBuf::from(home);
        h.push(".cache");
        h.push(format!("zservice-{tag}-{pid}"));
        candidates.push(h);
    }
    let mut m = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    m.push("target");
    m.push("test-scratch");
    m.push(format!("{tag}-{pid}"));
    candidates.push(m);
    for dir in candidates {
        let _ = std::fs::remove_dir_all(&dir);
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let probe = dir.join(".writable");
        if std::fs::write(&probe, b"x").is_ok() {
            let _ = std::fs::remove_file(&probe);
            return dir;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    panic!("no writable scratch space for test `{tag}`");
}
