//! An untracked child (reparented orphan, double-forked daemon) must still
//! come back from `reap`: the [`ChildTracker`] contract promises *every*
//! finished child, and a subreaper supervisor on the pidfd backend that only
//! watched tracked pids would leak it as a zombie forever.
//!
//! Own process on purpose: the orphan sweep is `waitpid(-1)`, which reaps
//! *any* child of the caller — inside the unit-test binary that would steal
//! sibling tests' children out from under their exact-equality assertions.

use std::time::Duration;
use zrt::childproc::new_child_tracker;
use zrt::clock;
use zrt::sys;

#[test]
fn an_untracked_orphan_is_reaped_with_its_code() {
    let mut t = new_child_tracker().expect("tracker");
    // Deliberately NOT tracked: this is the orphan.
    let pid = match sys::fork().expect("fork") {
        0 => {
            let _ = clock::sleep_ms(50);
            sys::_exit(17);
        }
        pid => pid,
    };
    let deadline = clock::deadline_from_now(Duration::from_secs(5));
    let mut found = None;
    while clock::now_ms() < deadline {
        for (p, st) in t.reap().expect("reap") {
            if p == pid {
                found = Some(st);
            }
        }
        if found.is_some() {
            break;
        }
        clock::sleep_ms(50).expect("sleep");
    }
    let st = found.expect("orphan must be reaped");
    assert_eq!(st.code(), 17);
}
