//! `zctl` — the supervisor's control client.
//!
//! A shim over the C4 line protocol (`DESIGN.md` §7, frozen): it formats one
//! `C {id} …` frame, reads the one-line reply, prints it, and exits 0 on `R`
//! or 1 on `E`. There is deliberately no output parsing and no pretty
//! printing — the protocol is the interface, and a client that reinterpreted
//! it would be a second implementation of it to keep correct.
//!
//! ```text
//! zctl [--socket PATH] [--id TOKEN] <verb> [name]
//! ```
//!
//! Verbs pass through verbatim (`status`, `list`, `start`, `stop`,
//! `restart`, `kick`, `reload-all`, `version`, `help`); `zctl` knows no
//! service semantics at all. `--socket` defaults to `/run/zinit/ctl`,
//! overridable for tests through `ZINIT_SOCKET`, exactly like the
//! supervisor's own default.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Default control socket path. Mirrors the supervisor so the two cannot
/// drift apart by editing one default and not the other — which is also why
/// this is a literal and not a shared crate: sharing would couple the CLI's
/// release cycle to the supervisor's for one string.
const DEFAULT_SOCKET_PATH: &str = "/run/zinit/ctl";
/// Environment override, honoured exactly like the supervisor honours it.
const SOCKET_ENV: &str = "ZINIT_SOCKET";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut socket: Option<PathBuf> = None;
    let mut id = String::from("zctl");
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                i += 1;
                match args.get(i) {
                    Some(path) => socket = Some(PathBuf::from(path)),
                    None => return usage("--socket needs a path"),
                }
            }
            "--id" => {
                i += 1;
                match args.get(i) {
                    Some(token) => id = token.clone(),
                    None => return usage("--id needs a token"),
                }
            }
            "-h" | "--help" | "help" if words.is_empty() => {
                return usage(TLDR_HELP);
            }
            word => words.push(word.to_string()),
        }
        i += 1;
    }
    // `help` past a verb is a verbatim pass-through (the server answers it);
    // a bare `help` (or `-h`/`--help`) is local usage.
    let frame = match build_frame(&id, &words) {
        Ok(frame) => frame,
        Err(_) => return usage(USAGE),
    };
    let path = socket_path(socket.as_deref());
    match transact(&path, &frame) {
        Ok(reply) => {
            print!("{reply}");
            if reply.starts_with("R ") {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("zctl: {e}");
            ExitCode::from(1)
        }
    }
}

/// Short usage, printed for local mistakes. Server-side `help` (a verb the
/// supervisor answers) is passed through, never answered here.
const USAGE: &str = "usage: zctl [--socket PATH] [--id TOKEN] <verb> [name]";
const TLDR_HELP: &str = "usage: zctl [--socket PATH] [--id TOKEN] <verb> [name]\nverbs: status <name> | list | start <name> | stop <name> | restart <name> | kick <name> | reload-all | version | help";

/// Print usage to stderr and exit 2. `print!` would go to the console the
/// supervisor may be handing over; usage errors belong on stderr.
fn usage(message: &str) -> ExitCode {
    eprintln!("zctl: {message}");
    ExitCode::from(2)
}

/// The socket to dial: explicit flag, then environment, then default.
fn socket_path(flag: Option<&Path>) -> PathBuf {
    if let Some(path) = flag {
        return path.to_path_buf();
    }
    std::env::var_os(SOCKET_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH))
}

/// Render one client frame. The verb and its single optional argument pass
/// through verbatim: validating them here would duplicate the server's
/// parser, and two parsers is how "valid" starts meaning two things.
/// Freestanding whitespace is rejected — a name with a space in it cannot
/// survive the line protocol, so failing here (exit 2, before dialling) beats
/// failing there (an `E` frame nobody asked for).
fn build_frame(id: &str, words: &[String]) -> Result<String, ()> {
    if id.is_empty() || id.chars().any(char::is_whitespace) {
        return Err(());
    }
    let frame = match words {
        [] => return Err(()),
        [verb] => format!("C {id} {verb}\n"),
        [verb, name] => {
            if name.chars().any(char::is_whitespace) {
                return Err(());
            }
            format!("C {id} {verb} {name}\n")
        }
        _ => return Err(()),
    };
    if frame.chars().any(|c| c == '\r') {
        return Err(());
    }
    Ok(frame)
}

/// Send one frame, read the one-line reply (with the newline).
fn transact(path: &Path, frame: &str) -> std::io::Result<String> {
    let mut io = BufReader::new(UnixStream::connect(path)?);
    io.get_mut().write_all(frame.as_bytes())?;
    let mut reply = String::new();
    io.read_line(&mut reply)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn verbs_render_verbatim() {
        assert_eq!(
            build_frame("1", &words(&["status", "sshd"])),
            Ok(String::from("C 1 status sshd\n"))
        );
        assert_eq!(
            build_frame("7", &words(&["reload-all"])),
            Ok(String::from("C 7 reload-all\n"))
        );
        // Unknown verbs pass through: the *server* owns the vocabulary.
        assert_eq!(
            build_frame("3", &words(&["frobnicator"])),
            Ok(String::from("C 3 frobnicator\n"))
        );
    }

    #[test]
    fn framing_hazards_are_local_errors() {
        assert!(build_frame("1", &[]).is_err(), "bare id is no command");
        assert!(build_frame("1", &words(&["a", "b", "c"])).is_err());
        assert!(build_frame("1", &words(&["stop", "a b"])).is_err());
        assert!(build_frame("has space", &words(&["list"])).is_err());
        assert!(build_frame("", &words(&["list"])).is_err());
    }

    #[test]
    fn socket_default_matches_the_supervisor() {
        assert_eq!(
            socket_path(None),
            PathBuf::from("/run/zinit/ctl"),
            "client and supervisor must dial the same default"
        );
        assert_eq!(
            socket_path(Some(Path::new("/tmp/x.sock"))),
            PathBuf::from("/tmp/x.sock")
        );
    }

    /// End to end against a stub listener: the frame goes out byte-exact and
    /// the reply comes back untouched (no parsing, no prettifying).
    #[test]
    fn round_trip_against_a_stub_server() {
        let dir = std::env::temp_dir().join(format!("zctl-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("ctl.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(conn.try_clone().expect("clone"))
                .read_line(&mut line)
                .expect("read");
            assert_eq!(line, "C 9 status sshd\n");
            conn.write_all(b"R 9 0 sshd running desired=up pid=1 up=0s restarts=0/5\n")
                .expect("reply");
        });
        let reply = transact(&sock, "C 9 status sshd\n").expect("round trip");
        assert_eq!(
            reply,
            "R 9 0 sshd running desired=up pid=1 up=0s restarts=0/5\n"
        );
        server.join().expect("server thread");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
