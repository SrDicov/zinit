//! The single sanctioned way for PID 1 to speak.
//!
//! A supervisor has no log sink at startup: the log system is something it
//! *will* run, not something it *has*. When a backend degrades — epoll
//! missing, signalfd missing, the monotonic clock missing — that fact still
//! has to go somewhere, because a supervisor that silently ends up on its
//! weakest backend is a supervisor whose bug reports cannot be reproduced.
//!
//! That somewhere is file descriptor 2, and this module is the only place in
//! the codebase allowed to write to it outside of tests. Centralizing it here
//! is what makes the rule enforceable: the CI `purity` job bans raw
//! `println!`/`eprintln!`/`dbg!` everywhere except this file, so a stray print
//! in the PID 1 path fails the build instead of showing up as a blank screen
//! on a machine nobody can ssh into.
//!
//! The write itself is panic-free by construction: it uses raw `write(2)`,
//! retries `EINTR`, and swallows every other error. An announcement that
//! could abort the supervisor would be worse than silence — `panic = "abort"`
//! in release means a failed `eprintln!` would kill PID 1, which is exactly
//! the class of failure this design refuses to have.

use std::io;

/// Announce a degradation: one line, `zinit: <message>`, on stderr.
///
/// Fire-and-forget. Called at most a handful of times per boot, only when a
/// preferred backend was unavailable and a fallback was taken. Never call this
/// on a hot path, and never call it for anything that is not a degradation —
/// routine status belongs in the log system, not on the console.
pub fn announce_degradation(message: &str) {
    // Two writes rather than one formatted allocation: this runs before any
    // allocator policy is decided, and there is no reason to involve the heap
    // in writing a constant prefix.
    write_all(b"zinit: ");
    write_all(message.as_bytes());
    write_all(b"\n");
}

/// Write the whole buffer to fd 2, retrying interruptions, ignoring failure.
fn write_all(mut buf: &[u8]) {
    while !buf.is_empty() {
        // SAFETY: fd 2 is stderr for the lifetime of the process; `buf` is a
        // live shared borrow for the duration of the call; `write` does not
        // retain the pointer. A short write advances the slice; any error
        // other than EINTR abandons the announcement rather than panicking.
        let rc = unsafe { libc::write(2, buf.as_ptr().cast(), buf.len()) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if rc == 0 {
            // A zero write on a blocking fd 2 means it was closed; there is
            // nowhere left to announce to.
            return;
        }
        buf = &buf[rc as usize..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcing_never_panics() {
        // There is no meaningful assertion on stderr content here — the test
        // harness owns fd 2 — only that the announcer cannot abort the caller.
        announce_degradation("test announcement; if you can read this it worked");
        announce_degradation("");
        announce_degradation(&"x".repeat(8192));
    }
}
