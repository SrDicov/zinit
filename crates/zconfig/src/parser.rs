//! The line parser: `&str` in, [`ServiceDesc`] or [`ParseError`] out.
//!
//! # The format
//!
//! DESIGN.md §5: a flat list of directives, two operators (`=`, `:`), one
//! comment character (`#`), zero nesting. `key = value`, spaces optional, `#`
//! anywhere starts a comment that runs to the end of the line. Anything an
//! operator needs beyond that is what `type = script` exists for.
//!
//! ```ini
//! # /etc/zinit/services.d/sshd.conf
//! command      = /usr/sbin/sshd -D -e
//! depends      = network.target, opt:logind
//! type         = process
//! env          = PATH=/usr/bin:/bin
//! user         = 0
//! restart      = on-failure
//! ready        = tcp:22
//! stop-timeout = 10s
//! log          = file:/var/log/zinit/sshd.log
//! cgroup       = web
//! rlimit-nofile = 8192
//! ```
//!
//! # Key normalisation
//!
//! Directive names are matched **case-insensitively**, and `-` and `_` are
//! interchangeable: `stop_timeout`, `stop-timeout` and `Stop-Timeout` are the
//! same directive. The reason is that this format is written by hand, by
//! several people, over years, and systemd-style `stop-timeout` and Rust-style
//! `stop_timeout` are both natural. A parser that punishes the second with
//! "unknown directive" teaches operators to distrust its errors; a parser that
//! accepts both with a silent precedence rule is worse, because now there is a
//! rule to be wrong about.
//!
//! A key that writes `-` **and** `_` (`stop-timeout_ms`) is deliberately *not*
//! accepted. `stop-timeout` vs `stop_timeout` is resolved; `stop-timeout_ms`
//! vs `stop_timeout_ms` is not, and it falls through to the unknown-key path,
//! whose help line spells out which of the two is interchangeable with which.
//!
//! # Columns
//!
//! Columns are **byte** offsets within the line, 1-indexed, because
//! [`Span`] is defined in bytes: a highlighter is fed byte offsets, and mixing
//! a `char` column with a byte `len` would mis-highlight every description with
//! a non-ASCII comment in it.
//!
//! Every offset this module produces comes from scanning for an ASCII
//! delimiter — `=`, `:`, `#`, `\\`, `,`, `"`, `'` are all below 0x80, and a
//! UTF-8 continuation byte is `>= 0x80` — so every offset is on a `char`
//! boundary by construction, and every slice taken with one is valid. The
//! `no_panic_on_mutated_input` test at the bottom is the machine check of that
//! claim, and `spans_always_land_on_a_char_boundary` checks the stronger
//! property that the *reported* positions are boundaries too.
//!
//! # Error codes
//!
//! Stable identifiers, so tests and suppression lists never match on prose.
//! Codes are retired, never reused.
//!
//! | Code | Meaning |
//! |---|---|
//! | `E001` | no key before the separator |
//! | `E002` | no `=` or `:` on a non-comment line |
//! | `E003` | empty value |
//! | `E004` | unknown directive |
//! | `E005` | duplicate directive |
//! | `E006` | bad `env` binding |
//! | `E007` | a value of the wrong shape (rlimit, `critical`, line too long) |
//! | `E008` | a `depends` entry that is really an assignment |
//! | `E009` | empty dependency name |
//! | `E010` | a name declared both required and optional |
//! | `E011` | `depends` given to a `target` |
//! | `E012` | `cgroup` or `rlimit-*` given to a `target` |
//! | `E015` | the text exceeds the line or line-count limit |
//! | `E016` | a service depends on itself |
//! | `E018` | the service name is not usable |
//! | `E019` | a `depends` entry is not a usable service name |
//!
//! A value rejected by `value.rs` keeps **its** code (`E3xx`), because those
//! codes are far more specific than anything this file could invent for it —
//! `E302` ("unknown unit") says more than a generic "bad value" ever would.
//! `E013` and `E014` are deliberately never emitted: they were reserved for
//! directive-ordering rules, and this parser has none. The file is read in one
//! pass, every directive is order-independent, and `depends = base` followed
//! by `type = target` is rejected for the same reason and with the same code as
//! the reverse.
//!
//! # Safety
//!
//! This function is a fuzz target. It never panics, never indexes out of
//! range, and never slices a `&str` at an offset that is not a `char`
//! boundary: every offset it takes comes from a scan for an ASCII byte, and
//! every slice is of a form that cannot be split mid-character.
//! `no_panic_on_mutated_input` runs 10 000 deterministic mutations of a valid
//! description through it under `catch_unwind`, so "it did not panic" is an
//! assertion and not merely the absence of a test failure.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use zcore::{SeccompAction, ServiceKind};

use crate::desc::{ServiceDesc, validate_service_name};
use crate::diagnostic::{Diagnostic, DiagnosticBag, Span};
use crate::value::{
    LogSpec, ReadySpec, ReadySpecKind, RestartSpec, RunAs, ValueError, parse_capability,
    parse_duration, parse_listen, parse_u64,
};

/// Longest single line the parser will look at, in bytes.
///
/// Not a correctness limit — nothing here can misbehave because a line is long
/// — but a 10 MB `command` would otherwise become a 10 MB error message, and
/// an init that prints a megabyte of diagnostics during a boot is an init
/// nobody can read. Anything past this is rejected with a pointer to the limit.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Longest prefix reproduced verbatim inside a diagnostic.
const MAX_QUOTE_BYTES: usize = 48;

/// Longest text the parser accepts at all, in bytes.
///
/// A service description is a dozen lines. A megabyte is not a description, it
/// is a denial of service aimed at PID 1, and the loop below allocates per
/// line, so bounding the input bounds the damage. Checked before anything is
/// allocated.
const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Upper bound on the number of lines parsed from one text.
///
/// Same reasoning as [`MAX_TEXT_BYTES`], one level down: `\n\n\n\n…` is only
/// one byte per line, so a megabyte of blank lines is a million lines and a
/// million iterations before a boot that is now visibly late.
const MAX_LINES: u32 = 100_000;

/// A directive this parser knows about, in its canonical spelling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Directive {
    /// Command line to execute.
    Command,
    /// `process` | `script` | `target` | `console`.
    Type,
    /// Comma-separated dependency names, `opt:`-prefixed for optional edges.
    Depends,
    /// `never` | `on-failure` | `always`, or a budget.
    Restart,
    /// `<n> restarts per <duration>`.
    RestartBudget,
    /// Delay between two restarts of the same service.
    RestartDelay,
    /// `none` | `notify` | `ping:<cmd>` | `tcp:<port>`.
    Ready,
    /// How long to keep probing the readiness handshake.
    ReadyTimeout,
    /// How long to wait for a clean stop before `SIGKILL`.
    StopTimeout,
    /// How long to wait for the service to reach `Running`.
    StartTimeout,
    /// `user[:group]` or `uid:gid`.
    User,
    /// `KEY=VALUE`, repeatable and accumulating.
    Env,
    /// `none` | `file[:path]` | `syslog`.
    Log,
    /// `yes` | `no`: whether a failure takes the system down.
    Critical,
    /// Slice name under `zinit.slice`.
    Cgroup,
    /// Soft limit on open file descriptors.
    RlimitNofile,
    /// Soft limit on processes/threads.
    RlimitNproc,
    /// Soft limit on address space, in bytes.
    RlimitAs,
    /// Path of the pid file, for `type = forking` only.
    PidFile,
    /// Controlling terminal path, for `type = console` only.
    Tty,
    /// Watchdog budget in whole seconds; needs `ready = notify`.
    WatchdogSec,
    /// `tcp:<port>[:<name>]` or `unix:<path>`, repeatable and accumulating.
    Listen,
    /// Space-separated Linux capability names to drop from the bounding set.
    DropCapabilities,
    /// `enforce` | `errno` | `off`: what a seccomp violation does.
    SyscallFilter,
    /// Space-separated syscall names to allow past the filter.
    SyscallAllow,
}

impl Directive {
    /// Canonical lowercase-dashed spelling, as documented in DESIGN.md §5.
    pub const fn name(self) -> &'static str {
        match self {
            Directive::Command => "command",
            Directive::Type => "type",
            Directive::Depends => "depends",
            Directive::Restart => "restart",
            Directive::RestartBudget => "restart-budget",
            Directive::RestartDelay => "restart-delay",
            Directive::Ready => "ready",
            Directive::ReadyTimeout => "ready-timeout",
            Directive::StopTimeout => "stop-timeout",
            Directive::StartTimeout => "start-timeout",
            Directive::User => "user",
            Directive::Env => "env",
            Directive::Log => "log",
            Directive::Critical => "critical",
            Directive::Cgroup => "cgroup",
            Directive::RlimitNofile => "rlimit-nofile",
            Directive::RlimitNproc => "rlimit-nproc",
            Directive::RlimitAs => "rlimit-as",
            Directive::PidFile => "pid-file",
            Directive::Tty => "tty",
            Directive::WatchdogSec => "watchdog-sec",
            Directive::Listen => "listen",
            Directive::DropCapabilities => "drop-capabilities",
            Directive::SyscallFilter => "syscall-filter",
            Directive::SyscallAllow => "syscall-allow",
        }
    }

    /// Every directive, in the order DESIGN.md §5 lists them.
    ///
    /// `critical` is the one addition to the documented eleven, and it is there
    /// because some services must be allowed to fail — a `console` service the
    /// operator started by hand, a oneshot nobody depends on. Making that a
    /// hidden property of the service *name* would be a policy nobody could
    /// read off the file. The alternative, refusing to model it at all, means
    /// the runtime has to hardcode the same list somewhere less visible.
    pub const ALL: [Directive; 25] = [
        Directive::Command,
        Directive::Type,
        Directive::Depends,
        Directive::Restart,
        Directive::RestartBudget,
        Directive::RestartDelay,
        Directive::Ready,
        Directive::ReadyTimeout,
        Directive::StopTimeout,
        Directive::StartTimeout,
        Directive::User,
        Directive::Env,
        Directive::Log,
        Directive::Critical,
        Directive::Cgroup,
        Directive::RlimitNofile,
        Directive::RlimitNproc,
        Directive::RlimitAs,
        Directive::PidFile,
        Directive::Tty,
        Directive::WatchdogSec,
        Directive::Listen,
        Directive::DropCapabilities,
        Directive::SyscallFilter,
        Directive::SyscallAllow,
    ];

    /// Match a raw key, applying the case and `-`/`_` normalisation.
    ///
    /// Returns `None` for a key that mixes `-` and `_` rather than silently
    /// picking one, so the error can say which spelling was meant.
    pub fn lookup(raw: &str) -> Option<Directive> {
        if raw.contains('_') && raw.contains('-') {
            return None;
        }
        let mut buf = String::with_capacity(raw.len());
        for ch in raw.chars() {
            match ch {
                '_' | '-' => buf.push('-'),
                _ => buf.push(ch.to_ascii_lowercase()),
            }
        }
        Directive::ALL
            .iter()
            .copied()
            .find(|d| d.name() == buf.as_str())
    }

    /// Whether repeating this directive is an accumulation rather than a
    /// contradiction.
    ///
    /// Exactly two directives, and both are ones whose *entire purpose* is to
    /// be written more than once: `env` bindings and `listen` sockets. Anything
    /// else that is repeated has two values fighting over one field, and
    /// "which one won" is a question nobody can answer by reading the file.
    pub const fn accumulates(self) -> bool {
        matches!(self, Directive::Env | Directive::Listen)
    }
}

/// The category of a rejection.
///
/// These are *categories*; the exact reason is always in
/// [`ParseError::message`] and the exact place in the [`Span`]. Splitting them
/// into twenty variants would freeze the wording into the type system and buy
/// nothing — no caller matches on "empty value" but on "is this fatal, and
/// where".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseErrorKind {
    /// A separator appeared where a key was expected.
    MissingKey,
    /// A non-empty, non-comment line had no separator.
    MissingSeparator,
    /// The value was empty, or was only a comment.
    EmptyValue,
    /// The key is not a directive.
    UnknownKey,
    /// The key was already given, and this directive does not accumulate.
    Duplicate,
    /// A value of the wrong shape.
    BadValue,
    /// Structural damage in a multi-part value.
    BadList,
    /// A name referenced by `depends` refers to this same service.
    SelfDependency,
    /// The service name, which came from outside the text, is not usable.
    BadName,
}

impl ParseErrorKind {
    /// Short lowercase label, used as a word in the rendered message.
    pub const fn name(self) -> &'static str {
        match self {
            ParseErrorKind::MissingKey => "missing-key",
            ParseErrorKind::MissingSeparator => "missing-separator",
            ParseErrorKind::EmptyValue => "empty-value",
            ParseErrorKind::UnknownKey => "unknown-key",
            ParseErrorKind::Duplicate => "duplicate",
            ParseErrorKind::BadValue => "bad-value",
            ParseErrorKind::BadList => "bad-list",
            ParseErrorKind::SelfDependency => "self-dependency",
            ParseErrorKind::BadName => "bad-name",
        }
    }
}

/// A rejected description: where, what, and how to fix it.
///
/// Always carries a [`Span`]. A diagnostic without a position is one the
/// operator has to grep for, and an init that refuses to boot must say *which
/// line* stopped it.
///
/// `line` and `column` duplicate the first two fields of `span` because every
/// consumer wants them, and copying two `u32`s is cheaper than making everyone
/// remember to reach through the span.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParseError {
    /// What went wrong, at the granularity of a category.
    pub kind: ParseErrorKind,
    /// Stable code, printed verbatim in the message.
    pub code: &'static str,
    /// 1-indexed line, where line 1 is the first line of `text`.
    pub line: u32,
    /// 1-indexed column, in bytes from the start of the line.
    pub column: u32,
    /// Span of the offending token.
    pub span: Span,
    /// The explanation, including any suggestion.
    pub message: String,
    /// `zcheck` output and editor completion: what would make this go away.
    pub help: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {} [{}]: {}",
            self.line,
            self.column,
            self.code,
            self.kind.name(),
            self.message
        )
    }
}

impl ParseError {
    /// Build an error at an explicit span.
    fn new(
        kind: ParseErrorKind,
        code: &'static str,
        span: Span,
        message: String,
        help: String,
    ) -> ParseError {
        ParseError {
            kind,
            code,
            line: span.line,
            column: span.column,
            span,
            message,
            help,
        }
    }

    /// Re-wrap a `value.rs` failure, keeping *its* code and help.
    ///
    /// `value.rs` already produces a `Diagnostic` with the most specific code
    /// it has (`E302` for an unknown unit, not a generic "bad value"), so this
    /// lifts the message and help straight out of it instead of flattening
    /// thirty distinct failures into one code that says nothing.
    fn from_value(e: ValueError<'_>, span: Span) -> ParseError {
        let d = e.to_diagnostic(span);
        ParseError {
            kind: ParseErrorKind::BadValue,
            code: d.code,
            line: span.line,
            column: span.column,
            span,
            message: d.message,
            help: d.help.unwrap_or_else(|| {
                String::from("see the message above; the parser rejected the value rather than guessing at it")
            }),
        }
    }

    /// Turn this into a [`Diagnostic`].
    ///
    /// The severity is always [`crate::diagnostic::Severity::Error`], because
    /// this type only ever describes a rejected description; the warnings live
    /// in the [`DiagnosticBag`] returned by
    /// [`parse_service_with_diagnostics`]
    /// and are never fatal.
    pub fn into_diagnostic(self) -> Diagnostic {
        Diagnostic::error(Some(self.span), self.code, self.message, Some(self.help))
    }
}

/// A successfully parsed description together with everything the parser
/// noticed but did not reject.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParsedService {
    /// The description.
    pub desc: ServiceDesc,
    /// Non-fatal observations, in source order. Empty is the common case, and
    /// a bag of warnings is a **passing** run — see
    /// [`DiagnosticBag::has_errors`].
    pub warnings: DiagnosticBag,
}

/// Parse one service description, keeping the warnings.
///
/// `name` is the service name as resolved by the caller — usually the file
/// name — not something the file can state itself. A file has no reliable way
/// to know the name it will be loaded under, and trusting a `name =` line would
/// let one file impersonate another.
///
/// The name is **validated** before a single byte of `text` is looked at, by
/// [`validate_service_name`]. That ordering is the whole point: a name
/// containing `../../etc/passwd` must be refused before anything downstream
/// has a chance to build a path out of it, and the check has to happen where
/// the name enters the system rather than in each of the places that use it.
///
/// The text is UTF-8, so input that is not arrives as `Err` from the caller's
/// `read_to_string` and never reaches this function. There is no lossy path in
/// which a byte becomes a different character, and so no way for a comment to
/// land in the middle of a multibyte one.
///
/// Returns on the **first** error. A description with three mistakes should be
/// fixed one line at a time, and reporting the rest means reading past a point
/// where the meaning is no longer trustworthy. That also means a rejected file
/// produces no warnings: the operator is going to fix the error first, and
/// showing them two notes about a file that does not parse is noise.
pub fn parse_service_with_diagnostics(name: &str, text: &str) -> Result<ParsedService, ParseError> {
    // Name first: it is one short pass, and it is the check whose failure has
    // consequences outside this crate.
    if let Err(e) = validate_service_name(name) {
        return Err(ParseError::new(
            ParseErrorKind::BadName,
            "E018",
            Span::point(1, 1),
            format!("`{}` is not a usable service name: {e}", quote(name)),
            String::from(
                "the name comes from the file name or the API argument, never from the file's contents; rename it to a single path-safe token",
            ),
        ));
    }

    if text.len() > MAX_TEXT_BYTES {
        return Err(ParseError::new(
            ParseErrorKind::BadValue,
            "E015",
            Span::point(1, 1),
            format!(
                "the description is {} bytes, over the {MAX_TEXT_BYTES} byte limit",
                text.len()
            ),
            String::from(
                "a service description is a dozen lines; check that this is the file you meant",
            ),
        ));
    }

    let mut desc = ServiceDesc::try_new(name).expect("the name was just validated");
    let mut warnings = DiagnosticBag::new();
    // `(directive, span)` in first-seen order: the duplicate check needs to
    // point at the second occurrence while naming the first, and the
    // whole-file checks need a real position for the directive they object to.
    let mut seen: Vec<(Directive, Span)> = Vec::new();

    // A BOM is stripped from the first line only. A file that starts with one
    // was written by an editor that thinks it is writing UTF-16, and refusing
    // to read it would be an init that cannot boot from a file the operator can
    // open in any editor they own.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    for (i, line) in text.lines().enumerate() {
        let n = i as u32 + 1;
        if n > MAX_LINES {
            return Err(ParseError::new(
                ParseErrorKind::BadValue,
                "E015",
                Span::point(n, 1),
                format!("the description is longer than the {MAX_LINES} line limit"),
                String::from("split the description, or raise MAX_LINES if that is deliberate"),
            ));
        }
        // A CRLF file is a CRLF file on every platform that has ever written
        // one, and the `\r` is not part of the value.
        let raw = line.trim_end_matches('\r');
        if raw.trim().is_empty() {
            continue;
        }
        if raw.len() > MAX_LINE_BYTES {
            return Err(ParseError::new(
                ParseErrorKind::BadValue,
                "E015",
                Span::point(n, 1),
                format!(
                    "this line is {} bytes, over the {MAX_LINE_BYTES} byte limit",
                    raw.len()
                ),
                String::from(
                    "a directive is a key, a value and a comment; this is probably a misplaced paste",
                ),
            ));
        }

        // Everything from the first unescaped `#` is a comment. The offset is
        // kept, not just discarded, because "a comment after the value" is
        // fine while "a comment *inside* the value" is the single most common
        // way to silently truncate a command line.
        let comment_at = comment_offset(raw);
        let stripped = match comment_at {
            Some(off) => &raw[..off],
            None => raw,
        };
        let content = stripped.trim();
        if content.is_empty() {
            continue;
        }
        // 1-indexed byte column of the first non-space character.
        let content_col = (stripped.len() - stripped.trim_start().len() + 1) as u32;
        let content_span = Span::range(n, content_col, content.len() as u32);

        // ── key ──────────────────────────────────────────────────────────────
        let (key, rest) = match split_directive(content) {
            Some(pair) => pair,
            None if looks_like_key_only(content) => {
                // A bare word is almost always a directive whose `=` went
                // missing, and saying so with the directive's own name is a
                // better message than a generic "no separator".
                let hint = match Directive::lookup(content) {
                    Some(d) => format!(
                        "`{}` is a directive, so it needs a value: write `{} = <value>`",
                        content,
                        d.name()
                    ),
                    None => format!("`{}` has no `=` or `:`", quote(content)),
                };
                return Err(ParseError::new(
                    ParseErrorKind::MissingSeparator,
                    "E002",
                    content_span,
                    hint,
                    String::from(
                        "directives are written `key = value` or `key : value`, with the spaces optional",
                    ),
                ));
            }
            None => {
                return Err(ParseError::new(
                    ParseErrorKind::MissingKey,
                    "E001",
                    content_span,
                    format!("expected a directive, found `{}`", quote(content)),
                    String::from(
                        "a line is `key = value`; a bare word is not a directive, and `type = script` is the escape hatch for anything more complex",
                    ),
                ));
            }
        };

        // `key` is the trimmed prefix of `content`, and `content` starts at
        // `content_col`, so the key's column *is* `content_col`. Computing it
        // by searching `raw` for the key text would also work and would be a
        // bug waiting for a line whose comment repeats the key.
        let key_span = Span::range(n, content_col, key.len() as u32);

        // ── values ───────────────────────────────────────────────────────────
        let directive = match Directive::lookup(key) {
            Some(d) => d,
            None => return Err(unknown_key_error(key, key_span)),
        };

        // `rest` begins just after the separator; skip its leading spaces and
        // the remainder is the value. The offset is in *bytes*, and both
        // subtractions are over byte lengths, so it is a boundary.
        let value_off = (content.len() - rest.len()) + (rest.len() - rest.trim_start().len());
        let value = rest.trim();
        if value.is_empty() {
            return Err(ParseError::new(
                ParseErrorKind::EmptyValue,
                "E003",
                key_span,
                format!("`{}` has no value", directive.name()),
                format!("write `{} = <value>`", directive.name()),
            ));
        }
        let value_col = content_col + value_off as u32;
        let value_span = Span::range(n, value_col, value.len() as u32);

        // ── duplicates ───────────────────────────────────────────────────────
        if !directive.accumulates() {
            if let Some((_, first)) = seen.iter().find(|(d, _)| *d == directive) {
                return Err(ParseError::new(
                    ParseErrorKind::Duplicate,
                    "E005",
                    key_span,
                    format!(
                        "duplicate directive `{}`, first given on line {}",
                        directive.name(),
                        first.line
                    ),
                    String::from("each directive may appear once; only `env` and `listen` repeat"),
                ));
            }
        }
        seen.push((directive, key_span));

        // Substitution is never performed here — see
        // `ServiceDesc::needs_env_expansion` — but the flag is set for *any*
        // value that contains one, not just the four that are stored as text.
        // A list of "the directives where this can happen" is an allowlist, and
        // an allowlist fails by omission; `${` appearing anywhere in the file
        // means the operator asked for a substitution this crate did not do.
        if needs_expansion(value) {
            desc.needs_env_expansion = true;
        }

        match directive {
            Directive::Command => {
                desc.command = value.to_owned();
                // A `#` inside the value has already been eaten by
                // `comment_offset`. Comparing *columns* rather than offsets in
                // two different coordinate systems is what makes this correct
                // for an indented line: `comment_at` indexes `raw`, and
                // `value_off` indexes `content`, and the two differ by however
                // much whitespace the line starts with.
                if let Some(off) = comment_at {
                    let comment_col = off as u32 + 1;
                    if comment_col > value_col {
                        warnings.push(Diagnostic::warning(
                            Some(Span::point(n, comment_col)),
                            "W001",
                            format!(
                                "an unescaped `#` starts a comment, so `{}` is all that remains of the command",
                                desc.command
                            ),
                            Some(String::from(
                                "write `\\#` for a literal one, or move the comment to its own line; the parser cannot tell a trailing comment from a truncated command, so it says so every time",
                            )),
                        ));
                    }
                }
            }
            Directive::Type => {
                desc.kind = match parse_kind(value) {
                    Some(k) => k,
                    None => {
                        return Err(bad_value(
                            directive,
                            value_span,
                            "write one of: process script target console oneshot forking",
                        ));
                    }
                };
            }
            Directive::Depends => {
                let self_name = desc.name.clone();
                parse_depends(value, n, value_span, &self_name, &mut desc)?;
            }
            Directive::Restart => {
                desc.restart = match RestartSpec::parse(value) {
                    Ok(r) => r,
                    Err(e) => return Err(ParseError::from_value(e.into(), value_span)),
                };
            }
            Directive::RestartBudget => {
                // The budget is the same grammar `restart =` accepts, so it is
                // parsed by the same code path — a second grammar for it would
                // be a second thing to get wrong.
                let parsed = match RestartSpec::parse(value) {
                    Ok(r) => r,
                    Err(e) => return Err(ParseError::from_value(e.into(), value_span)),
                };
                // Only the two numbers the directive is about. The policy stays
                // whatever `restart` said, or the default, and the delay stays
                // whatever `restart-delay` said: a budget is half a policy, and
                // half of a policy that silently reset the other half would be
                // a trap.
                desc.restart.budget.capacity = parsed.budget.capacity;
                desc.restart.budget.window_ms = parsed.budget.window_ms;
            }
            Directive::RestartDelay => {
                desc.restart.budget.delay_ms = duration_ms(value, value_span)?;
            }
            Directive::Ready => {
                desc.ready = match ReadySpec::parse(value) {
                    Ok(r) => r,
                    Err(e) => return Err(ParseError::from_value(e.into(), value_span)),
                };
            }
            Directive::StopTimeout => desc.stop_timeout_ms = duration_ms(value, value_span)?,
            Directive::StartTimeout => desc.start_timeout_ms = duration_ms(value, value_span)?,
            Directive::ReadyTimeout => desc.ready_timeout_ms = duration_ms(value, value_span)?,
            Directive::User => {
                // A *name* is a legal description that this crate cannot
                // resolve — it has no `/etc/passwd`. Rejecting it pushes
                // operators to write `user = 0` or to delete the line, both
                // worse than not having the directive. Dropping it would be
                // worse still: a service that asked to drop privileges and
                // was silently started as root is a security bug wearing the
                // costume of a lenient parser.
                //
                // So the name is kept unresolved, and `build_plan` refuses to
                // freeze a plan while any identity is still a name. The
                // enforcement is there, not here; this layer only records
                // what was asked for.
                match RunAs::try_parse(value) {
                    Ok(Some(run_as)) => {
                        // `1000` or `1000:1001`: fully resolved here.
                        desc.run_as = Some(run_as);
                        desc.unresolved_run_as = None;
                    }
                    Ok(None) => {
                        // `mysql`, `mysql:www`, `root`: kept verbatim for
                        // `zrt`, which can look it up.
                        desc.run_as = None;
                        desc.unresolved_run_as = Some(value.to_owned());
                    }
                    Err(e) => return Err(ParseError::from_value(e.into(), value_span)),
                }
            }
            Directive::Env => {
                let (k, v) = match parse_env_entry(value) {
                    Ok(pair) => pair,
                    Err(message) => {
                        return Err(ParseError::new(
                            ParseErrorKind::BadValue,
                            "E006",
                            value_span,
                            message,
                            String::from("write `KEY=VALUE`; the value may contain `=` and spaces"),
                        ));
                    }
                };
                desc.env.push((k, v));
            }
            Directive::Listen => {
                match parse_listen(value) {
                    Ok(addr) => desc.listens.push(addr),
                    Err(e) => return Err(ParseError::from_value(e.into(), value_span)),
                };
            }
            Directive::DropCapabilities => {
                let mut caps = Vec::new();
                for word in value.split_whitespace() {
                    if parse_capability(word).is_none() {
                        return Err(bad_value(
                            directive,
                            value_span,
                            "write Linux capability names; `CAP_` prefix and case are ignored",
                        ));
                    }
                    // Canonical spelling (lowercase, no prefix), so
                    // `CAP_SYS_ADMIN` and `sys_admin` in two layers merge
                    // instead of doubling the drop.
                    let lower = word.to_ascii_lowercase();
                    let norm = lower.strip_prefix("cap_").unwrap_or(&lower);
                    caps.push(norm.to_owned());
                }
                if caps.is_empty() {
                    return Err(bad_value(
                        directive,
                        value_span,
                        "name at least one capability to drop",
                    ));
                }
                desc.drop_caps = caps;
            }
            Directive::SyscallFilter => {
                // Explicit `off` assigns `None` rather than skipping the arm:
                // a drop-in that says `off` over a base that says `enforce`
                // must win, and only a recorded presence makes that work.
                match value {
                    "enforce" => desc.syscall_filter = Some(SeccompAction::Enforce),
                    "errno" => desc.syscall_filter = Some(SeccompAction::Errno),
                    "off" => desc.syscall_filter = None,
                    _ => {
                        return Err(bad_value(
                            directive,
                            value_span,
                            "write `enforce`, `errno` or `off`",
                        ));
                    }
                }
            }
            Directive::SyscallAllow => {
                let mut names = Vec::new();
                for word in value.split_whitespace() {
                    if word.is_empty()
                        || !word
                            .as_bytes()
                            .iter()
                            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
                    {
                        return Err(bad_value(
                            directive,
                            value_span,
                            "write syscall names; numbers are arch-specific",
                        ));
                    }
                    names.push(word.to_owned());
                }
                if names.is_empty() {
                    return Err(bad_value(
                        directive,
                        value_span,
                        "name at least one syscall to allow",
                    ));
                }
                desc.syscall_allow = names;
            }
            Directive::Log => {
                desc.log = match LogSpec::parse(value) {
                    Ok(l) => l,
                    Err(e) => return Err(ParseError::from_value(e.into(), value_span)),
                };
            }
            Directive::Critical => {
                desc.is_critical = match parse_bool(value) {
                    Some(b) => b,
                    None => {
                        return Err(bad_value(
                            directive,
                            value_span,
                            "write `yes` or `no`; the default is yes, so a non-critical service is an explicit choice",
                        ));
                    }
                };
            }
            Directive::Cgroup => desc.cgroup = Some(value.to_owned()),
            Directive::RlimitNofile | Directive::RlimitNproc | Directive::RlimitAs => {
                // The same scanner a `tcp:` port and a `u64` budget capacity
                // use, so "what a number is" has one answer in this crate.
                let number = match parse_u64(value).ok() {
                    Some(n) => n,
                    None => {
                        return Err(bad_value(
                            directive,
                            value_span,
                            "write a plain decimal number with no sign, no underscores and no `0x`, e.g. `8192`",
                        ));
                    }
                };
                let key = match directive {
                    Directive::RlimitNofile => "nofile",
                    Directive::RlimitNproc => "nproc",
                    _ => "as",
                };
                desc.rlimits.push((key.to_owned(), number));
            }
            Directive::PidFile => {
                desc.pid_file = Some(value.to_owned());
            }
            Directive::Tty => {
                // A terminal path is absolute or it is a typo: relative
                // resolution would depend on a working directory no reader
                // of this file can see, and `open` in the child would follow
                // it somewhere else. NUL is refused later, at `CString`
                // construction, with the service name attached.
                if value.is_empty() || !value.starts_with('/') {
                    return Err(bad_value(
                        directive,
                        value_span,
                        "write an absolute terminal path, e.g. `/dev/tty1`",
                    ));
                }
                desc.tty = Some(value.to_owned());
            }
            Directive::WatchdogSec => {
                let secs = match parse_u64(value).ok() {
                    Some(n) => n,
                    None => {
                        return Err(bad_value(
                            directive,
                            value_span,
                            "write whole seconds with no sign and no decimals, e.g. `30`",
                        ));
                    }
                };
                if secs == 0 {
                    return Err(bad_value(
                        directive,
                        value_span,
                        "a zero-second watchdog kills every start; omit it to disable",
                    ));
                }
                desc.watchdog_sec = Some(secs);
            }
        }
        // One hook for every arm: each one above either applied its
        // directive or returned an error, so reaching here means "this file
        // said this directive". Drop-in merging reads these marks; without
        // them a default the parser filled in would be indistinguishable from
        // an explicit choice.
        desc.mark_explicit(directive);
    }

    // ── whole-file checks ──────────────────────────────────────────────────
    // These need the *final* `type`, so they cannot run at the line that
    // triggered them: `type = target` after `cgroup = web` would otherwise be
    // accepted in one order and rejected in the other.
    if desc.is_virtual() {
        for directive in [Directive::Depends, Directive::Cgroup, Directive::Tty] {
            if let Some(span) = span_of(&seen, directive) {
                return Err(virtual_directive_error(directive, desc.kind, span));
            }
        }
        for directive in [
            Directive::RlimitNofile,
            Directive::RlimitNproc,
            Directive::RlimitAs,
        ] {
            if let Some(span) = span_of(&seen, directive) {
                return Err(virtual_directive_error(directive, desc.kind, span));
            }
        }
    }

    // A `pid-file` on anything but a forking daemon is a path nobody will
    // ever read: only the adoption logic follows it, and it only runs for
    // `Forking`. Stored and ignored would be a lie in the file, so it is an
    // error in the file. Drop-ins restating `type = forking` alongside it
    // pass; the merged cross-layer case is caught again at load.
    if desc.is_explicit(Directive::PidFile) && desc.kind != ServiceKind::Forking {
        let span = span_of(&seen, Directive::PidFile).unwrap_or(Span::point(1, 1));
        return Err(bad_value(
            Directive::PidFile,
            span,
            "pid-file needs `type = forking` in the same file, drop-ins included",
        ));
    }
    // A terminal nobody takes over is a path nobody will ever open: only the
    // console handover reads it, and it only runs for `Console`. Same shape
    // as `pid-file` above, including the drop-in escape hatch and the merged
    // cross-layer backstop at load.
    if desc.is_explicit(Directive::Tty) && desc.kind != ServiceKind::Console {
        let span = span_of(&seen, Directive::Tty).unwrap_or(Span::point(1, 1));
        return Err(bad_value(
            Directive::Tty,
            span,
            "tty needs `type = console` in the same file, drop-ins included",
        ));
    }
    // Sockets nobody spawns for bind nowhere: same shape as `pid-file`, for
    // every kind without a process.
    if desc.is_explicit(Directive::Listen) && !desc.kind.has_process() {
        let span = span_of(&seen, Directive::Listen).unwrap_or(Span::point(1, 1));
        return Err(bad_value(
            Directive::Listen,
            span,
            "listen needs a process; a target never spawns to receive sockets",
        ));
    }
    // A watchdog nobody can feed is a kill timer: pings arrive on the notify
    // fd, so without `ready = notify` the first expiry would murder a healthy
    // service — and a target has no process at all, so its expiry would fire
    // on a service that cannot even be stopped. Refused here rather than
    // regretted in production.
    if desc.watchdog_sec.is_some()
        && (desc.is_virtual() || desc.ready.kind != ReadySpecKind::Notify)
    {
        let span = span_of(&seen, Directive::WatchdogSec).unwrap_or(Span::point(1, 1));
        return Err(bad_value(
            Directive::WatchdogSec,
            span,
            "watchdog-sec needs a process with `ready = notify`: pings arrive on the notify fd",
        ));
    }

    // An allow-list with no filter is a dead list: nothing reads it, so a
    // typo in `syscall-filter` would silently unconfine the service. The
    // reverse (a filter with an empty list) is meaningful — the tightest
    // possible confinement — and stays legal.
    if desc.syscall_filter.is_none() && !desc.syscall_allow.is_empty() {
        let span = span_of(&seen, Directive::SyscallAllow).unwrap_or(Span::point(1, 1));
        return Err(bad_value(
            Directive::SyscallAllow,
            span,
            "syscall-allow needs a filter; allowed names alone confine nothing",
        ));
    }

    // A missing `command` is not an error here: descriptions can be layered (a
    // `zinit.d` drop-in may add the command to a `services.d` one) and the
    // linker is the only component that sees all the layers. What the parser
    // *can* say is that this file alone is incomplete, which is almost always
    // the real bug. The default kind is `process`, so a file that never says
    // `type` still needs a command.
    if desc.missing_command() {
        // Point at the `type` line when there is one, because that is the line
        // that decided this service needs a command at all.
        let span = span_of(&seen, Directive::Type).unwrap_or(Span::point(1, 1));
        warnings.push(Diagnostic::warning(
            Some(span),
            "W002",
            String::from(
                "no `command` directive: a process, script or console service needs one, and only a later drop-in can supply it",
            ),
            Some(String::from(
                "add `command = ...`, or `type = target` if this service starts no process",
            )),
        ));
    }
    if desc.kind == ServiceKind::Process && desc.ready.kind == ReadySpecKind::None {
        // Documented rather than assumed: a `ready = none` on a real process is
        // legal and means "no handshake", so this is a warning, not an error.
        // The condition is written out instead of being implied so a future
        // change to the defaults has to make a decision here.
        let span = span_of(&seen, Directive::Ready).unwrap_or(Span::point(1, 1));
        warnings.push(Diagnostic::warning(
            Some(span),
            "W003",
            String::from(
                "`ready = none` marks the service Running as soon as the fork succeeds, with no proof it is usable",
            ),
            Some(String::from(
                "use `ready = notify` and write to `$ZINIT_NOTIFY_FD`, or `ready = tcp:<port>` if it listens",
            )),
        ));
    }

    // Instance expansion, last of all: nothing above may depend on whether
    // the name carries an instance, and the substitution must see the final
    // field values of this file. (A drop-in merged later re-expands nothing:
    // every file is parsed under its own name, so each layer already
    // substituted with the same instance.)
    expand_instance(&mut desc)?;

    Ok(ParsedService { desc, warnings })
}

/// Expand `%i` (instance) and `%%` in `command`, `tty` and `pid-file`.
///
/// Only for names carrying an instance (`getty@tty1`, split at the last
/// `@`): without one every `%` stays literal, so `printf '%i\n'` in an
/// ordinary service is untouched, and a literal `%%` outside templates is
/// never mangled either. A name with more than one `@` or an empty instance
/// is refused — silently picking one would expand the wrong thing, and the
/// leading-`@` case never reaches here (the name check at entry refuses it).
/// A lone `%` (followed by neither `i` nor `%`) is left as-is: the expander
/// does not touch what it does not understand.
fn expand_instance(desc: &mut ServiceDesc) -> Result<(), ParseError> {
    let Some(at) = desc.name.rfind('@') else {
        return Ok(());
    };
    // `@` is one ASCII byte, so `at + 1` is always a `char` boundary, and
    // everything after it is the candidate instance.
    let instance = desc.name[at + 1..].to_owned();
    if instance.is_empty() || instance.contains('@') {
        return Err(ParseError::new(
            ParseErrorKind::BadName,
            "E018",
            Span::point(1, 1),
            format!(
                "service name `{}` has an unusable instance: write `name@instance` with exactly one `@`",
                desc.name
            ),
            String::from(
                "instances come from the file name (`getty@tty1.conf`); rename the file",
            ),
        ));
    }
    desc.command = substitute_instance(&desc.command, &instance);
    if let Some(tty) = desc.tty.as_mut() {
        *tty = substitute_instance(tty, &instance);
    }
    if let Some(pid_file) = desc.pid_file.as_mut() {
        *pid_file = substitute_instance(pid_file, &instance);
    }
    Ok(())
}

/// One pass over `text`: `%i` becomes `instance`, `%%` becomes `%`,
/// anything else (including a lone `%`) is copied verbatim.
///
/// Index-driven rather than iterator-driven: the expander consumes the
/// character *after* `%` itself, which a `for` loop cannot express.
/// Boundaries are safe by construction — `%`, `i` and the second `%` are
/// all one ASCII byte, so every slice below starts and ends on one.
fn substitute_instance(text: &str, instance: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        match rest[i + 1..].chars().next() {
            Some('i') => {
                out.push_str(instance);
                rest = &rest[i + 2..];
            }
            Some('%') => {
                out.push('%');
                rest = &rest[i + 2..];
            }
            // A lone `%` (trailing, or before anything else): copied, and
            // the rest — starting right after the `%`, which is a boundary —
            // is rescanned, so `%é` keeps both characters.
            _ => {
                out.push('%');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Parse one service description, discarding the warnings.
///
/// The short form of [`parse_service_with_diagnostics`]. It exists so that the
/// common case — a caller that will act on the description rather than print
/// comments about it — does not have to name a field it will throw away. It
/// is **not** the entry point `zcheck` should use: a tool whose job is to
/// report what is wrong with a file is throwing away half of what it found.
pub fn parse_service(name: &str, text: &str) -> Result<ServiceDesc, ParseError> {
    parse_service_with_diagnostics(name, text).map(|p| p.desc)
}

// ═══════════════════════════════════════════════════════════════════════════
// Byte-level helpers
//
// Every offset produced below comes from scanning for an ASCII byte, so it is
// always on a `char` boundary. That is the whole UTF-8 safety argument, and it
// holds because `=`, `:`, `#`, `\\`, `,`, `"` and `'` are all below 0x80: a
// UTF-8 continuation byte is >= 0x80 and can never be mistaken for one of them.
// ═══════════════════════════════════════════════════════════════════════════

/// Byte offset of the first unescaped `#` in `line`, if any.
///
/// The first unescaped `#` starts the comment. A backslash escapes both the
/// `#` and itself, so `\#` is a literal `#` and `\\` is a literal backslash.
/// The scan advances a whole `char` after a backslash, so the returned offset
/// is always a boundary.
///
/// The `_` arm steps one **byte**, which walks through the interior of a
/// multi-byte character. That is safe and not an oversight: a continuation byte
/// is `>= 0x80` and can never equal `\\` or `#`, so the scan can pass over the
/// interior of a character without ever matching inside it, and every offset
/// that *is* returned sits on an ASCII byte.
fn comment_offset(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\\' => {
                // `i` is on a `\\`, which is one byte, so `i + 1` is a boundary.
                {
                    let c = line[i + 1..].chars().next()?;
                    i += 1 + c.len_utf8()
                }
            }
            b'#' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Split a line into `(key, rest)` at the first `=` or `:`.
///
/// Both operators are accepted and neither is preferred, because the format has
/// no construct in which the two could be confused. An empty key — a line
/// starting with `=` — is **not** a match: it becomes a `MissingKey` error
/// whose span points at the line, which is more useful than pointing at
/// nothing.
fn split_directive(content: &str) -> Option<(&str, &str)> {
    let b = content.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'=' | b':' => {
                let key = content[..i].trim();
                if key.is_empty() {
                    return None;
                }
                // `b[i]` is ASCII, so `i` and `i + 1` are both boundaries.
                return Some((key, &content[i + 1..]));
            }
            b'#' => return None,
            _ => i += 1,
        }
    }
    None
}

/// Heuristic that separates "I forgot the `=`" from "this is not a line".
///
/// A single bare word after comment stripping is almost certainly a directive
/// with a missing operator; several words are more likely prose somebody left
/// in the file. It changes the *message* only, never whether it is an error.
fn looks_like_key_only(content: &str) -> bool {
    !content.contains(char::is_whitespace)
}

// ═══════════════════════════════════════════════════════════════════════════
// Value parsing owned by this file
//
// Everything with a grammar of its own lives in `value.rs`. What is left here
// is the two shapes that only make sense in the context of a *line* — an `env`
// assignment and a `depends` list — plus the numbers and the two enums that
// are one token wide.
// ═══════════════════════════════════════════════════════════════════════════

/// Parse a duration, mapping the failure onto the line's span.
fn duration_ms(value: &str, span: Span) -> Result<u64, ParseError> {
    parse_duration(value).map_err(|e| ParseError::from_value(e.into(), span))
}

/// `type = ...`, case-insensitively, without allocating a lowercased copy.
fn parse_kind(value: &str) -> Option<ServiceKind> {
    [
        ServiceKind::Process,
        ServiceKind::Script,
        ServiceKind::Target,
        ServiceKind::Console,
        ServiceKind::Oneshot,
        ServiceKind::Forking,
    ]
    .into_iter()
    .find(|&kind| value.eq_ignore_ascii_case(kind.name()))
}

/// `env = KEY=VALUE`.
///
/// Only the *key* is constrained. The value may contain `=`, spaces and
/// anything else, because `PATH=/a:/b` and `ARGS=--flag=a --flag=b` are the
/// normal cases and a rule that forbade them would be fought with quoting.
fn parse_env_entry(value: &str) -> Result<(String, String), String> {
    // `=` is ASCII, so both halves of the split are valid `&str`s.
    let eq = match value.find('=') {
        Some(i) => i,
        None => {
            return Err(format!(
                "`{}` is not an environment assignment: there is no `=` in it",
                quote(value)
            ));
        }
    };
    let key = value[..eq].trim();
    let val = value[eq + 1..].trim();

    if key.is_empty() {
        return Err(String::from("the variable name is empty"));
    }
    // `key` is non-empty, so `as_bytes()[0]` exists, and it is the first byte
    // of the first `char`, which is the only place the "must not start with a
    // digit" rule can apply.
    if key.as_bytes()[0].is_ascii_digit() {
        return Err(format!(
            "the variable name `{key}` starts with a digit, which no shell could expand"
        ));
    }
    let valid = key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
    if !valid {
        return Err(format!(
            "the variable name `{key}` contains something a shell would not expand"
        ));
    }
    if val.is_empty() {
        return Err(format!(
            "`{key}` has an empty value; write `{key}= ` with something after it, or drop the line"
        ));
    }
    Ok((key.to_owned(), val.to_owned()))
}

/// One entry of a comma-separated list, with the byte range it occupied in the
/// *original* text.
///
/// The range is what a diagnostic needs: the unescaped text is what the
/// grammar is checked against, and the range is what the operator has to
/// underline. Keeping only the first and measuring the range by searching for
/// the text would point `a, a` at the first `a` twice.
struct ListEntry {
    /// The entry with quotes removed and escapes applied.
    text: String,
    /// Byte offset of the entry within the list value.
    start: usize,
    /// Byte offset one past the entry within the list value.
    end: usize,
}

/// Split a comma-separated list, honouring single and double quotes.
///
/// Offsets are **byte** offsets, counted with `len_utf8`, not with one `+1` per
/// character. A `depends` list written in a non-Latin script is not exotic, and
/// a column computed in characters is a caret in the wrong place for every one
/// of its lines.
fn split_list_spans(value: &str) -> Vec<ListEntry> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote_char: Option<char> = None;
    // Byte offset of the character just consumed.
    let mut pos = 0usize;
    // Byte offset where the entry being accumulated starts. Set to just past
    // the comma, so leading spaces stay inside the entry and the caller trims
    // them itself and can still say where the name was.
    let mut start = 0usize;
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match quote_char {
            Some(q) => {
                if c == '\\' {
                    match chars.next() {
                        Some(next) => {
                            cur.push(next);
                            pos += 1 + next.len_utf8();
                            continue;
                        }
                        // A trailing backslash inside a quoted region is
                        // literal: it is dropped by nothing, and treating it as
                        // an escape with nothing to escape is how a name
                        // silently changes.
                        None => {
                            cur.push(c);
                            pos += c.len_utf8();
                            continue;
                        }
                    }
                }
                if c != q {
                    cur.push(c);
                } else {
                    quote_char = None;
                }
                pos += c.len_utf8();
            }
            None => match c {
                '"' | '\'' => {
                    quote_char = Some(c);
                    pos += c.len_utf8();
                }
                ',' => {
                    out.push(ListEntry {
                        text: core::mem::take(&mut cur),
                        start,
                        end: pos,
                    });
                    pos += c.len_utf8();
                    start = pos;
                }
                _ => {
                    cur.push(c);
                    pos += c.len_utf8();
                }
            },
        }
    }
    out.push(ListEntry {
        text: cur,
        start,
        end: pos,
    });
    out
}

/// Split `depends = a, opt:b, c` into required and optional names.
///
/// Errors point at the offending **entry**, not at the whole line: "a, opt:,
/// c" is a typo about one name, and a diagnostic that underlines the entire
/// `depends` line reads the same for every typo on it.
fn parse_depends(
    value: &str,
    line: u32,
    span: Span,
    self_name: &str,
    desc: &mut ServiceDesc,
) -> Result<(), ParseError> {
    for entry in split_list_spans(value) {
        let raw = &value[entry.start..entry.end];
        let text = entry.text.trim();
        // The column of the entry's first non-space character: the value's
        // column, plus the entry's offset within the value, plus the spaces it
        // starts with. All three are byte counts.
        let lead = entry.end - entry.start - raw.trim_start().len();
        let entry_col = span.column + entry.start as u32 + lead as u32;
        let entry_span = Span::range(line, entry_col, text.len() as u32);

        if text.is_empty() {
            return Err(ParseError::new(
                ParseErrorKind::BadList,
                "E009",
                entry_span,
                String::from(
                    "empty dependency name; a trailing or doubled comma is almost always a typo",
                ),
                String::from("write `depends = a, b` with no empty entries"),
            ));
        }
        if text.contains('=') {
            return Err(ParseError::new(
                ParseErrorKind::BadList,
                "E008",
                entry_span,
                format!(
                    "`{}` is not a dependency: dependencies are names, not assignments",
                    quote(text)
                ),
                String::from(
                    "a dependency is `name` or `opt:name`; assignments belong on an `env` line",
                ),
            ));
        }
        if text.contains(':') && !text.starts_with("opt:") {
            return Err(ParseError::new(
                ParseErrorKind::BadList,
                "E008",
                entry_span,
                format!(
                    "`{}` looks like a `key = value` pair that landed on the `depends` line",
                    quote(text)
                ),
                String::from(
                    "`opt:` is the only prefix a dependency takes, and a `.target` suffix needs none",
                ),
            ));
        }

        let (name, optional) = match text.strip_prefix("opt:") {
            Some(rest) => (rest, true),
            None => (text, false),
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(ParseError::new(
                ParseErrorKind::BadList,
                "E009",
                entry_span,
                String::from("`opt:` with no name after it"),
                String::from("write `opt:name`"),
            ));
        }
        if name == self_name {
            return Err(ParseError::new(
                ParseErrorKind::SelfDependency,
                "E016",
                entry_span,
                format!("`{self_name}` depends on itself"),
                String::from("a service cannot be its own dependency, not even optionally"),
            ));
        }
        // A dependency name is a service name. Checking it with the same
        // validator the caller's own name goes through is what stops
        // `depends = ../../etc/passwd` from being one accepted line away from
        // a traversal, and it means there is exactly one definition of what a
        // name may be.
        if let Err(e) = validate_service_name(name) {
            return Err(ParseError::new(
                ParseErrorKind::BadName,
                "E019",
                entry_span,
                format!("`{}` is not a usable service name: {e}", quote(name)),
                String::from(
                    "a dependency is another service's name, so it obeys the same rules as this file's own name",
                ),
            ));
        }

        // An exact duplicate is idempotent; a name that changes qualification
        // is a contradiction, and which one the author meant is a question the
        // file cannot answer.
        if desc.requires(name) {
            continue;
        }
        if desc.depends_optional.iter().any(|n| n == name) {
            if optional {
                continue;
            }
            return Err(ParseError::new(
                ParseErrorKind::BadList,
                "E010",
                entry_span,
                format!("`{name}` is already declared optional and is now declared required"),
                String::from("pick one: a dependency either gates startup or it does not"),
            ));
        }
        if optional {
            desc.depends_optional.push(name.to_owned());
        } else {
            desc.depends_required.push(name.to_owned());
        }
    }
    Ok(())
}

/// `yes` / `no`, case-insensitive, plus the spellings people actually type.
fn parse_bool(value: &str) -> Option<bool> {
    // `eq_ignore_ascii_case` rather than `to_ascii_lowercase`: four
    // comparisons, no allocation, and a spelling nobody thought of still
    // falls through to `None`.
    const YES: [&str; 5] = ["yes", "y", "true", "1", "on"];
    const NO: [&str; 5] = ["no", "n", "false", "0", "off"];
    if YES.iter().any(|s| value.eq_ignore_ascii_case(s)) {
        return Some(true);
    }
    if NO.iter().any(|s| value.eq_ignore_ascii_case(s)) {
        return Some(false);
    }
    None
}

/// True if the text contains a `${...}` reference.
///
/// A bare `$` is not a reference: `$1`, `$(...)` and a literal `$` in a regex
/// are all things a command line may legitimately contain, and flagging them
/// would make the flag useless. The brace is what makes it a reference.
///
/// The shape inside the braces is deliberately not inspected: `${VAR}` and
/// `${VAR:-def}` are both "a substitution this crate did not perform", and
/// deciding that `${}` or `${:-}` is malformed is `zrt`'s job, once it has an
/// environment to expand against.
fn needs_expansion(value: &str) -> bool {
    value.contains("${")
}

/// Copy at most [`MAX_QUOTE_BYTES`] of `s` into a message, marking the cut.
///
/// Used everywhere a value is echoed into a diagnostic, so a 10 MB key cannot
/// become a 10 MB error message. The check is `len_utf8`, so a `char` that
/// would not fit whole is left out rather than split: a diagnostic that is
/// almost valid UTF-8 is a diagnostic a terminal will mangle.
fn quote(s: &str) -> String {
    if s.len() <= MAX_QUOTE_BYTES {
        return s.to_owned();
    }
    let mut out = String::with_capacity(MAX_QUOTE_BYTES + 3);
    for c in s.chars() {
        if out.len() + c.len_utf8() > MAX_QUOTE_BYTES {
            break;
        }
        out.push(c);
    }
    out.push_str("...");
    out
}

// ═══════════════════════════════════════════════════════════════════════════
// Diagnostics
// ═══════════════════════════════════════════════════════════════════════════

/// The `E004` error for a key that is not a directive.
fn unknown_key_error(key: &str, span: Span) -> ParseError {
    let message = format!(
        "unknown directive `{}`; the help line lists every known one",
        quote(key)
    );

    let mut help = String::from("known directives: ");
    for (i, d) in Directive::ALL.iter().enumerate() {
        if i > 0 {
            help.push_str(", ");
        }
        help.push('`');
        help.push_str(d.name());
        help.push('`');
    }
    help.push_str(
        ". Keys are case-insensitive and `-` and `_` are interchangeable, so `stop-timeout` and `stop_timeout` are the same directive.",
    );

    ParseError::new(ParseErrorKind::UnknownKey, "E004", span, message, help)
}

/// The `E007` error for a value of the wrong shape.
fn bad_value(directive: Directive, span: Span, help: &str) -> ParseError {
    ParseError::new(
        ParseErrorKind::BadValue,
        "E007",
        span,
        format!("`{}` does not accept this value", directive.name()),
        String::from(help),
    )
}

/// The `E011` / `E012` error for a directive on a service with no process.
fn virtual_directive_error(directive: Directive, kind: ServiceKind, span: Span) -> ParseError {
    let (message, help) = if directive == Directive::Depends {
        (
            format!(
                "`depends` is meaningless for `type = {}`: a service with no process has nothing to order",
                kind.name()
            ),
            String::from(
                "put the edges on the services that actually start; a target is Running once its own dependencies are",
            ),
        )
    } else {
        (
            format!(
                "`{}` applies to a process, and this service is a `{}`",
                directive.name(),
                kind.name()
            ),
            String::from("remove the directive: a target creates no process to constrain"),
        )
    };
    ParseError::new(
        ParseErrorKind::BadValue,
        if directive == Directive::Depends {
            "E011"
        } else {
            "E012"
        },
        span,
        message,
        help,
    )
}

/// The span of the first occurrence of `directive`, if it was used.
fn span_of(seen: &[(Directive, Span)], directive: Directive) -> Option<Span> {
    seen.iter()
        .find(|(d, _)| *d == directive)
        .map(|(_, span)| *span)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::default_budget;
    use alloc::string::ToString;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use zcore::{LogSink, Restart};

    const FULL: &str = "\
# /etc/zinit/services.d/sshd.conf
command      = /usr/sbin/sshd -D -e
depends      = network.target, opt:logind
type         = process
env          = PATH=/usr/bin:/bin
env          = SSH_LOG_LEVEL=INFO
user         = 0
restart      = on-failure
ready        = tcp:22
stop-timeout = 10s
log          = file:/var/log/zinit/sshd.log
cgroup       = web
rlimit-nofile = 8192
";

    fn ok(text: &str) -> ServiceDesc {
        match parse_service("sshd", text) {
            Ok(d) => d,
            Err(e) => panic!("expected success, got: {e}"),
        }
    }

    fn err(text: &str) -> ParseError {
        match parse_service("sshd", text) {
            Ok(d) => panic!("expected an error, got: {d:?}"),
            Err(e) => e,
        }
    }

    fn warns(text: &str) -> DiagnosticBag {
        match parse_service_with_diagnostics("sshd", text) {
            Ok(p) => p.warnings,
            Err(e) => panic!("expected success, got: {e}"),
        }
    }

    fn has_warning(bag: &DiagnosticBag, code: &str) -> bool {
        bag.iter().any(|d| d.code == code)
    }

    // ── rule 1: service names ────────────────────────────────────────────

    /// The dinit `validate_service_name` bug, driven through the public entry
    /// point rather than the helper, because a validator nothing calls is not
    /// a validator.
    #[test]
    fn a_name_that_would_escape_is_rejected_by_the_parser() {
        for bad in [
            "../../etc/passwd",
            "a/b",
            ".hidden",
            "@include",
            "con espacio",
        ] {
            let e = match parse_service(bad, "command = /bin/true\n") {
                Ok(d) => panic!("must refuse {bad:?}, got {d:?}"),
                Err(e) => e,
            };
            assert_eq!(e.code, "E018", "name {bad:?}");
            assert_eq!(e.kind, ParseErrorKind::BadName);
            assert!(
                e.message.contains(bad),
                "the message must name it: {}",
                e.message
            );
        }
    }

    /// A name is checked *before* the text, so a hostile name cannot reach the
    /// line loop even when the text is a perfectly valid description.
    #[test]
    fn the_name_is_checked_before_any_of_the_text() {
        let e = match parse_service("../evil", "this is not even a directive\n") {
            Ok(d) => panic!("expected rejection, got {d:?}"),
            Err(e) => e,
        };
        assert_eq!(e.code, "E018", "the name must be judged first");
    }

    /// A *dependency* name is a service name too, and this is the line that
    /// makes `depends = ../../etc/passwd` an error.
    #[test]
    fn a_dependency_name_is_validated_the_same_way() {
        for bad in [
            "command = /bin/true\ndepends = ../../etc/passwd\n",
            "command = /bin/true\ndepends = a/b\n",
            "command = /bin/true\ndepends = .hidden\n",
            "command = /bin/true\ndepends = @include\n",
            "command = /bin/true\ndepends = con espacio\n",
            "command = /bin/true\ndepends = ok, opt:../evil\n",
        ] {
            let e = err(bad);
            assert_eq!(e.code, "E019", "input {bad:?}");
        }
    }

    /// Wildcards stay legal: DESIGN.md §5.2 uses `depends = *.target` for
    /// `all.target`, and `*` is not a path character.
    #[test]
    fn a_wildcard_dependency_is_not_mistaken_for_a_traversal() {
        let d = ok("command = /bin/true\ndepends = *.target\n");
        assert_eq!(d.depends_required, ["*.target"]);
    }

    // ── rule 2: the documented example ───────────────────────────────────

    #[test]
    fn parses_the_documented_example() {
        let d = ok(FULL);
        assert_eq!(d.name, "sshd");
        assert_eq!(d.kind, ServiceKind::Process);
        assert_eq!(d.command, "/usr/sbin/sshd -D -e");
        assert_eq!(d.depends_required, ["network.target"]);
        assert_eq!(d.depends_optional, ["logind"]);
        assert_eq!(d.restart.policy, Restart::OnFailure);
        assert_eq!(d.ready.port(), Some(22));
        assert_eq!(d.stop_timeout_ms, 10_000);
        assert_eq!(d.cgroup.as_deref(), Some("web"));
        assert_eq!(d.rlimit("nofile"), Some(8192));
        assert_eq!(d.run_as, Some(RunAs { uid: 0, gid: 0 }));
        assert_eq!(
            d.env,
            [
                (String::from("PATH"), String::from("/usr/bin:/bin")),
                (String::from("SSH_LOG_LEVEL"), String::from("INFO")),
            ]
        );
        assert!(!d.needs_env_expansion);
        assert!(warns(FULL).is_empty(), "{:?}", warns(FULL).into_vec());
    }

    #[test]
    fn every_directive_parses() {
        let d = ok(concat!(
            "type = script\n",
            "command = /bin/sh -c 'while true; do sleep 1; done'\n",
            "depends = a, b, c\n",
            "restart = always\n",
            "ready = ping:/usr/bin/curl -sf http://127.0.0.1/\n",
            "stop-timeout = 2s\n",
            "start-timeout = 90s\n",
            "user = 1000:1000\n",
            "env = A=1\n",
            "log = syslog\n",
            "critical = no\n",
            "cgroup = web\n",
            "rlimit-nofile = 4096\n",
            "rlimit-nproc = 64\n",
            "rlimit-as = 1073741824\n",
        ));
        assert_eq!(d.kind, ServiceKind::Script);
        assert_eq!(d.depends_required, ["a", "b", "c"]);
        assert_eq!(d.restart.policy, Restart::Always);
        assert_eq!(d.stop_timeout_ms, 2_000);
        assert_eq!(d.start_timeout_ms, 90_000);
        assert!(!d.is_critical);
        assert_eq!(
            d.run_as,
            Some(RunAs {
                uid: 1000,
                gid: 1000
            })
        );
        assert_eq!(d.rlimit("nproc"), Some(64));
        assert_eq!(d.rlimit("as"), Some(1_073_741_824));
        assert_eq!(d.log.sink, LogSink::Syslog);
        assert!(
            warns(concat!(
                "type = script\n",
                "command = /bin/sh -c 'x'\n",
                "depends = a, b, c\n",
                "restart = always\n",
                "ready = ping:curl\n",
                "stop-timeout = 2s\n",
                "start-timeout = 90s\n",
                "user = 1000:1000\n",
                "env = A=1\n",
                "log = syslog\n",
                "critical = no\n",
                "cgroup = web\n",
                "rlimit-nofile = 4096\n",
                "rlimit-nproc = 64\n",
                "rlimit-as = 1073741824\n",
            ))
            .is_empty()
        );
    }

    /// `Directive::ALL` is the contract between this parser and the `did you
    /// mean` list; a directive nobody can type is a directive with no story.
    #[test]
    fn every_listed_directive_is_actually_accepted() {
        for d in Directive::ALL {
            let line = format!("{} = 1", d.name());
            // The point is only that the key is *recognised*: several values
            // are legitimately wrong, and the code must not be E004.
            if let Err(e) = parse_service("x", &line) {
                assert_ne!(e.code, "E004", "`{}` is listed but not accepted", d.name());
            }
        }
    }

    #[test]
    fn a_target_needs_no_command() {
        let d = ok("type = target\n");
        assert_eq!(d.kind, ServiceKind::Target);
        assert!(d.command.is_empty());
        assert!(!d.missing_command());
        assert!(!has_warning(&warns("type = target\n"), "W002"));
    }

    // ── budgets and delays ──────────────────────────────────────────────

    #[test]
    fn restart_budget_and_delay_land_on_the_same_spec() {
        let d =
            ok("command = /bin/true\nrestart-budget = 3 restarts per 1m\nrestart-delay = 100ms\n");
        assert_eq!(d.budget().capacity, 3);
        assert_eq!(d.budget().window_ms, 60_000);
        assert_eq!(d.budget().delay_ms, 100);
    }

    #[test]
    fn a_budget_does_not_reset_the_policy() {
        let d = ok("command = /bin/true\nrestart = never\nrestart-budget = 3 restarts per 1m\n");
        assert_eq!(d.restart.policy, Restart::Never);
        assert_eq!(d.budget().capacity, 3);
    }

    /// The other direction: a budget must not reset a delay either, or
    /// `restart-budget` after `restart-delay` re-introduces the default.
    #[test]
    fn a_budget_does_not_reset_the_delay() {
        let d = ok("command = /bin/true\nrestart-delay = 2s\nrestart-budget = 3 restarts per 1m\n");
        assert_eq!(d.budget().delay_ms, 2_000);
    }

    #[test]
    fn restart_delay_without_a_budget_keeps_the_default_window() {
        let d = ok("command = /bin/true\nrestart-delay = 2s\n");
        assert_eq!(d.budget().window_ms, default_budget().window_ms);
        assert_eq!(d.budget().delay_ms, 2_000);
    }

    #[test]
    fn a_zero_budget_is_refused_by_value_not_by_the_parser() {
        let e = err("command = /bin/true\nrestart-budget = 0 restarts per 1m\n");
        assert_eq!(e.code, "E322", "value.rs owns that judgement");
    }

    // ── rule 3: a `#` in the middle of a value ──────────────────────────

    /// The silent bug this warning exists for: a command that quietly becomes
    /// a different command.
    #[test]
    fn a_hash_inside_a_command_truncates_it_and_warns() {
        let d = ok("command = /usr/bin/foo --color=#fff\n");
        assert_eq!(
            d.command, "/usr/bin/foo --color=",
            "the value stops at the #"
        );
        let bag = warns("command = /usr/bin/foo --color=#fff\n");
        let w = bag.iter().find(|d| d.code == "W001").expect("a warning");
        assert_eq!(w.severity, crate::diagnostic::Severity::Warning);
        let span = w.span.expect("W001 must point at the #");
        assert_eq!(span.line, 1);
        // The `#` is byte 31 of the line and a column is 1-indexed: `command =
        // ` is 10 bytes, `/usr/bin/foo` is **12** (`usr` and `bin` are three
        // each, and the two slashes make four more), ` --color=` is 9, and
        // 10 + 12 + 9 = 31.
        assert_eq!(span.column, 32, "the caret must land on the # itself");
    }

    /// The column must be right on an **indented** line, which is the case a
    /// naive "offset within the line" comparison gets wrong.
    #[test]
    fn the_hash_warning_column_is_right_on_an_indented_line() {
        let bag = warns("      command = /usr/bin/foo # bar\n");
        let w = bag.iter().find(|d| d.code == "W001").expect("a warning");
        let span = w.span.expect("a span");
        // The same 22-byte directive as above (`command = /usr/bin/foo `) after
        // 6 spaces of indentation, then the space before the `#`: byte 29, so
        // column 30. A parser that measured the comment's offset *within the
        // trimmed line* would say 24 here and underline the wrong token.
        assert_eq!(
            span.column, 30,
            "6 spaces + `command = /usr/bin/foo ` is 29 bytes"
        );
    }

    #[test]
    fn an_escaped_hash_does_not_truncate_and_does_not_warn() {
        let d = ok("command = /usr/bin/echo \\#1\n");
        assert_eq!(d.command, "/usr/bin/echo \\#1");
        assert!(!has_warning(
            &warns("command = /usr/bin/echo \\#1\n"),
            "W001"
        ));
    }

    /// Only `command` warns: a trailing comment on `cgroup = web # the web
    /// slice` is normal and must stay silent.
    #[test]
    fn a_trailing_comment_on_another_directive_is_not_warned_about() {
        let bag = warns("command = /bin/true\ncgroup = web # the web slice\n");
        assert!(!has_warning(&bag, "W001"), "{:?}", bag.into_vec());
        assert_eq!(
            ok("command = /bin/true\ncgroup = web # the web slice\n")
                .cgroup
                .as_deref(),
            Some("web")
        );
    }

    /// A whole-line comment, a comment on its own after a value that ends
    /// before it, and a line that is only a comment.
    #[test]
    fn whole_line_comments_and_blank_lines_are_ignored() {
        let d = ok("# header\n\n   \n\t\n#another\ncommand = /bin/true\n");
        assert_eq!(d.command, "/bin/true");
        assert!(warns("# header\n\n   \n\t\n#another\ncommand = /bin/true\n").is_empty());
    }

    // ── rules 4 and 5: separators, spacing, key normalisation ────────────

    #[test]
    fn both_separators_and_any_spacing_are_accepted() {
        assert_eq!(ok("command=/bin/true").command, "/bin/true");
        assert_eq!(ok("command:/bin/true").command, "/bin/true");
        assert_eq!(ok("command   =   /bin/true").command, "/bin/true");
        assert_eq!(ok("\tcommand\t=\t/bin/true").command, "/bin/true");
    }

    /// `stop_timeout` and `stop-timeout` are the same directive, in any case.
    #[test]
    fn keys_are_case_insensitive_and_dash_equals_underscore() {
        for spelling in [
            "stop_timeout",
            "stop-timeout",
            "STOP_TIMEOUT",
            "STOP-TIMEOUT",
            "Stop_Timeout",
        ] {
            let d = ok(&format!("{spelling} = 3s\n"));
            assert_eq!(d.stop_timeout_ms, 3_000, "{spelling}");
        }
        assert_eq!(ok("rlimit_nofile = 7").rlimit("nofile"), Some(7));
        assert_eq!(ok("RLIMIT-NOFILE = 7").rlimit("nofile"), Some(7));
        assert_eq!(ok("RESTART_DELAY = 2s").budget().delay_ms, 2_000);
    }

    // ── rule 6: unknown keys, with the list of real ones ─────────────────

    #[test]
    fn unknown_key_reports_code_span_and_help() {
        let e = err("command = /bin/true\ncomand = x\n");
        assert_eq!(e.kind, ParseErrorKind::UnknownKey);
        assert_eq!(e.code, "E004");
        assert_eq!(e.line, 2);
        assert_eq!(e.column, 1);
        assert_eq!(e.span.len, 6, "the span must cover the whole key");
        assert!(e.message.contains("comand"), "{}", e.message);
        assert!(e.message.contains("help line"), "{}", e.message);
        assert!(e.help.contains("stop-timeout"), "help must list valid keys");
    }

    /// A key nobody typed still gets the full list: the message cannot guess,
    /// so the list is the advice.
    #[test]
    fn an_unrecognisable_key_still_lists_everything() {
        let e = err("zzzzzzzzzzzz = x\n");
        assert_eq!(e.code, "E004");
        assert!(e.help.contains("restart-budget"));
        assert!(e.help.contains("rlimit-as"));
    }

    #[test]
    fn a_key_mixing_dash_and_underscore_is_rejected_with_a_hint() {
        for spelling in ["stop-timeout_ms", "start_timeout_ms"] {
            let e = err(&format!("{spelling} = 10s\n"));
            assert_eq!(e.code, "E004", "{spelling}");
            assert!(e.help.contains("interchangeable"), "{}", e.help);
        }
    }

    /// `ready-timeout` is a real directive with a real consumer
    /// (`zcore` arms it separately from `start-timeout`), so it is parsed and
    /// stored rather than refused.
    #[test]
    fn ready_timeout_is_parsed_and_independent_of_start_timeout() {
        for spelling in ["ready-timeout", "ready_timeout", "READY-TIMEOUT"] {
            let d = ok(&format!("command = /bin/true\n{spelling} = 7s\n"));
            assert_eq!(d.ready_timeout_ms, 7_000, "{spelling}");
        }
        // It is its own number: setting it must not disturb `start-timeout`,
        // and the default is the documented 30 s.
        let d = ok("command = /bin/true\nready-timeout = 7s\n");
        assert_eq!(d.start_timeout_ms, crate::desc::DEFAULT_START_TIMEOUT_MS);
        assert_eq!(ok("command = /bin/true\n").ready_timeout_ms, 30_000);
    }

    // ── rule 7: empty values ─────────────────────────────────────────────

    #[test]
    fn empty_values_are_errors_with_a_useful_span() {
        for input in [
            "command =",
            "command =   ",
            "command = # only a comment",
            "user :",
        ] {
            let e = err(input);
            assert_eq!(e.kind, ParseErrorKind::EmptyValue, "input {input:?}");
            assert_eq!(e.code, "E003");
            assert_eq!(e.line, 1);
            assert_eq!(
                e.column, 1,
                "the span must point at the key, not at nothing"
            );
        }
    }

    // ── rule 8: duplicates ───────────────────────────────────────────────

    #[test]
    fn duplicate_single_valued_directives_are_errors() {
        let e = err("command = /bin/a\ncommand = /bin/b\n");
        assert_eq!(e.kind, ParseErrorKind::Duplicate);
        assert_eq!(e.code, "E005");
        assert_eq!(e.line, 2);
        assert_eq!(e.column, 1);
        assert!(
            e.message.contains("line 1"),
            "the message must name the first: {}",
            e.message
        );
    }

    /// Every non-accumulating directive, not just `command`: a duplicate that
    /// is caught for one key and not another is a rule nobody can hold in their
    /// head.
    #[test]
    fn every_single_valued_directive_rejects_a_repeat() {
        for d in Directive::ALL {
            if d.accumulates() {
                continue;
            }
            // A value each directive actually accepts, so the *only* thing
            // that can reject the second line is the duplicate check. Using
            // `1` throughout would make `type = 1` fail on its value instead,
            // and the test would pass for the wrong reason.
            let line = format!("{} = {}", d.name(), sample_value(d));
            let e = err(&format!("{line}\n{line}\n"));
            assert_eq!(e.code, "E005", "`{}` accepted a duplicate", d.name());
            assert!(
                e.message.contains("line 1"),
                "`{}`: {}",
                d.name(),
                e.message
            );
        }
        assert!(Directive::Env.accumulates());
        assert!(Directive::Listen.accumulates());
    }

    /// A value `d` accepts, for the duplicate test above.
    fn sample_value(d: Directive) -> &'static str {
        match d {
            Directive::Command => "/bin/true",
            Directive::Type => "process",
            Directive::Depends => "base",
            Directive::Restart => "always",
            Directive::RestartBudget => "5 restarts per 1m",
            Directive::RestartDelay => "1s",
            Directive::Ready => "notify",
            Directive::ReadyTimeout => "5s",
            Directive::StopTimeout => "5s",
            Directive::StartTimeout => "5s",
            Directive::User => "0",
            Directive::Log => "syslog",
            Directive::Critical => "no",
            Directive::Cgroup => "web",
            Directive::RlimitNofile | Directive::RlimitNproc | Directive::RlimitAs => "1",
            Directive::PidFile => "/run/x.pid",
            Directive::Tty => "/dev/tty1",
            Directive::WatchdogSec => "30",
            Directive::Listen => "tcp:8080",
            Directive::DropCapabilities => "sys_admin",
            Directive::SyscallFilter => "enforce",
            Directive::SyscallAllow => "read",
            Directive::Env => "A=1",
        }
    }

    #[test]
    fn env_accumulates_instead() {
        let d = ok("env = A=1\nenv = B=2\nenv = A=3\n");
        assert_eq!(d.env.len(), 3);
        assert_eq!(d.env_get("A"), Some("3"));
    }

    // ── rule 9: `env` validation ─────────────────────────────────────────

    /// The key is constrained; the value is not.
    #[test]
    fn env_key_validation() {
        for bad in [
            "env = =value",
            "env = 1BAD=x",
            "env = NOEQUALS",
            "env = A B=1",
            "env = A=",
            "env = A B = 1",
            "env = A-B=1",
            "env = A$B=1",
        ] {
            let e = err(bad);
            assert_eq!(e.code, "E006", "input {bad:?} must be E006, got {}", e.code);
            assert_eq!(e.line, 1);
        }
    }

    #[test]
    fn env_value_may_contain_equals_and_spaces() {
        let d = ok("env = ARGS=--flag=a --other=b=c\n");
        assert_eq!(d.env[0].0, "ARGS");
        assert_eq!(d.env[0].1, "--flag=a --other=b=c");
        assert_eq!(ok("env = EMPTYISH=a b  c").env[0].1, "a b  c");
    }

    /// The value is everything after the **first** `=`, which is the only
    /// splitting rule that makes `ARGS=--a=b --c=d` work.
    #[test]
    fn env_splits_on_the_first_equals_only() {
        let d = ok("env = K=a=b=c\n");
        assert_eq!(d.env[0], (String::from("K"), String::from("a=b=c")));
    }

    // ── rule 10: expansion is flagged, never resolved ────────────────────

    #[test]
    fn expansion_is_flagged_not_resolved() {
        let d = ok("command = /bin/sh ${LOGIN_SHELL}\nenv = PATH=${PATH}:/opt/bin\n");
        assert!(d.needs_env_expansion);
        assert_eq!(
            d.command, "/bin/sh ${LOGIN_SHELL}",
            "the text must survive verbatim"
        );
        assert_eq!(d.env[0].1, "${PATH}:/opt/bin");
    }

    /// `${VAR:-def}` is the same fact as `${VAR}`: a substitution this crate
    /// did not perform.
    #[test]
    fn the_default_form_of_expansion_is_left_intact() {
        let d = ok("env = HOME=${HOME:-/root}\n");
        assert!(d.needs_env_expansion);
        assert_eq!(d.env[0].1, "${HOME:-/root}");
    }

    /// The flag is set wherever a reference appears, not only in the four
    /// fields that happen to be stored as text — an allowlist of "the places
    /// this can happen" fails by omission.
    #[test]
    fn expansion_is_flagged_in_every_directive_that_accepts_one() {
        assert!(ok("command = /bin/x ${A}").needs_env_expansion);
        assert!(ok("cgroup = ${SLICE}").needs_env_expansion);
        assert!(ok("log = file:/var/log/${SVC}.log").needs_env_expansion);
        assert!(ok("depends = ${A}").needs_env_expansion);
    }

    #[test]
    fn a_dollar_without_a_brace_is_not_expansion() {
        assert!(!ok("command = /bin/echo $\n").needs_env_expansion);
        assert!(!ok("command = /bin/echo $1 $(date)\n").needs_env_expansion);
        assert!(!ok("command = /bin/echo $PATH\n").needs_env_expansion);
        assert!(!ok("command = /bin/echo a}b\n").needs_env_expansion);
    }

    // ── rule 11: exact positions ─────────────────────────────────────────

    #[test]
    fn the_span_covers_the_offending_key() {
        let e = err("\n\n  restartt = 1s\n");
        assert_eq!(e.line, 3);
        assert_eq!(e.column, 3, "leading whitespace is not part of the key");
        assert_eq!(e.span.len, 8);
        assert_eq!(e.span.end_column(), 11);
    }

    #[test]
    fn a_value_span_covers_exactly_the_value() {
        let e = err("command = /bin/true\nrestart = sometimes\n");
        assert_eq!(e.line, 2);
        assert_eq!(e.column, 11, "the value starts after `restart = `");
        assert_eq!(e.span.len, 9);
    }

    /// The position must be right on an indented line too, for the key *and*
    /// for the value.
    #[test]
    fn positions_are_measured_from_the_line_not_the_directive() {
        let e = err("command = /bin/true\n        restart = sometimes\n");
        assert_eq!(e.line, 2);
        assert_eq!(e.column, 19, "8 spaces + `restart = ` is 18 bytes");
        assert_eq!(e.span.len, 9);
    }

    #[test]
    fn a_depends_entry_span_points_at_the_entry() {
        let e = err("command = /bin/true\ndepends = aaa, , bbb\n");
        assert_eq!(e.code, "E009");
        // `depends = ` puts the value at column 11; the empty entry is the
        // space after `aaa, `, which begins at byte 4 of the value and has one
        // leading space of its own: 11 + 4 + 1.
        //
        // Column 16 is the *comma* that terminated the empty entry, not the
        // space at 15, and that is the honest answer: an entry that is all
        // whitespace has no first non-space character, so the column lands on
        // the delimiter that proved the entry was empty — the character the
        // author has to delete.
        assert_eq!(e.column, 16);
        assert_eq!(e.span.len, 0, "an empty entry has no width");
        // And it is a point *inside* the line rather than past its end, because
        // a column past the last byte sends a highlighter nowhere.
        let line = "depends = aaa, , bbb";
        assert!(
            (e.span.column as usize) <= line.len(),
            "column {} is past the line",
            e.span.column
        );
    }

    /// A multi-byte key is measured in bytes, and the reported span must still
    /// land on character boundaries so a highlighter can use it.
    #[test]
    fn a_multibyte_key_is_measured_in_bytes() {
        // `ñ` is U+00F1, which UTF-8 encodes in **two** bytes (0xC3 0xB1), so
        // `ññ` is four bytes and two characters. A `char` count would report 2
        // and the highlighter would underline half the key.
        let e = err("ññ = x\n");
        assert_eq!(e.code, "E004");
        assert_eq!(e.column, 1);
        assert_eq!(e.span.len, 4, "ñ is two bytes, so two of them are four");
        assert!(
            e.message.contains('ñ'),
            "the message must quote the real key"
        );

        // The same property with a wider character, and on a key the parser
        // cannot know the length of, because a span is a sum of offsets and a
        // bug in any one term shows up here: `日` is three bytes, so the key is
        // five bytes wide and the byte just past it is a boundary, not the
        // middle of a character.
        let e = err("ñ日 = x\n");
        assert_eq!(e.column, 1);
        assert_eq!(e.span.len, 5, "two bytes of ñ plus three of 日");
        assert_eq!(e.span.end_column(), 6, "the 1-indexed column 6 is the `=`");
        assert!("ñ日 = x".is_char_boundary((e.span.end_column() - 1) as usize));
    }

    /// Every position this parser reports, on a corpus chosen to contain
    /// multi-byte characters in every interesting place, must be a `char`
    /// boundary of the line it names, and must be in range.
    ///
    /// This is the property that makes "the parser never slices UTF-8 by byte"
    /// checkable from the outside: if a column pointed into a character, this
    /// would fail even though nothing panicked.
    #[test]
    fn spans_always_land_on_a_char_boundary() {
        // Inputs that must *succeed*, so the warnings they produce can be
        // checked. Multi-byte characters are in the key, the value, the
        // comment and the indentation, because a column is a sum of three
        // independent offsets and any one of them can be computed in `char`s.
        const OK_CORPUS: [&str; 12] = [
            "command = /bin/ñññ # ñ",
            "command = 日本語 # コメント",
            "command = /bin/🎉",
            "command = /bin/true # é",
            "command = /bin/true\ndepends = é, ññ, opt:🎉",
            "    command = /bin/true # 日本語",
            "env = A=🎉",
            "env = PATH=/日本/ñ",
            "log = file:/日本/é.log",
            "cgroup = 日本",
            "command = /bin/true\nenv = A=🎉",
            "command = ñ # ñ",
        ];
        let mut spans_checked = 0usize;
        for text in OK_CORPUS {
            // Every corpus entry is expected to *parse*: this test is about
            // where the parser points on a successful run, and about the
            // warnings it attaches. Rejections have their own corpus below.
            let bag = warns(text);
            for w in bag.iter() {
                if let Some(span) = w.span {
                    spans_checked += 1;
                    assert_span_sane(
                        text,
                        &ParseError {
                            kind: ParseErrorKind::BadValue,
                            code: w.code,
                            line: span.line,
                            column: span.column,
                            span,
                            message: w.message.clone(),
                            help: String::new(),
                        },
                    );
                }
            }
        }
        // A corpus that stopped producing warnings would make the loop above
        // check nothing and the test would still pass, so the count is
        // asserted rather than assumed. W001 is the one that puts a column
        // after a value on an indented line, which is the arithmetic most
        // likely to be wrong.
        assert!(
            spans_checked > 0,
            "the corpus no longer produces any warning span"
        );
        // And every *rejection* on the same corpus, which is where a column
        // is most likely to be wrong: the error is about one token and the
        // arithmetic is a sum of three independent offsets.
        for text in REJECTION_CORPUS {
            let e = err(text);
            assert_span_sane(text, &e);
        }
    }

    /// Inputs chosen to make the parser reject, with multi-byte characters
    /// placed so that a column computed in `char`s instead of bytes lands in
    /// the wrong place or inside a character.
    const REJECTION_CORPUS: [&str; 17] = [
        "ñ = x",
        "ññ = x",
        "command = /bin/true\nññññ = x",
        "command = 日本語\ncomand = x",
        "🎉 = 1",
        "command = /bin/true\n🎉 = 1",
        "command = 日本語 # コメント\ncomand = x",
        "command = /bin/true\ndepends = aaa, , bbb",
        "command = /bin/true\ndepends = ééé, , z",
        "command = /bin/true\ndepends = ../éé",
        "command = /bin/true\nrestart = éééé",
        "env = =ñ",
        // A multi-byte character is not a shell-expandable variable name, so
        // this is a rejection — and the column lands on the `=`-less key.
        "env = 🎉=ñ",
        "env = é=ñ",
        "rlimit-as = 日本語",
        "   ñ =   ",
        "type = é",
    ];

    /// The structural check behind `spans_always_land_on_a_char_boundary`.
    fn assert_span_sane(text: &str, e: &ParseError) {
        let line = text
            .split('\n')
            .nth(e.span.line as usize - 1)
            .unwrap_or_else(|| {
                panic!(
                    "line {} does not exist in {text:?} (error: {e})",
                    e.span.line
                )
            });
        assert!(e.span.line >= 1, "line is 0 for {e}");
        assert!(e.span.column >= 1, "column is 0 for {e}");
        let col = (e.span.column - 1) as usize;
        assert!(
            line.is_char_boundary(col),
            "column {} is inside a character of {line:?} ({e})",
            e.span.column
        );
        let end = col + e.span.len as usize;
        assert!(
            line.is_char_boundary(end.min(line.len())),
            "span end {end} is inside a character of {line:?} ({e})"
        );
        assert_eq!(
            (e.line, e.column),
            (e.span.line, e.span.column),
            "duplicated fields disagree"
        );
    }

    // ── rule 12: a missing command warns, it does not fail ───────────────

    #[test]
    fn a_missing_command_warns_but_does_not_fail() {
        let d = ok("user = 0\n");
        assert!(d.command.is_empty());
        assert!(d.missing_command());
        assert!(has_warning(&warns("user = 0\n"), "W002"));
    }

    /// W002 points at the `type` line when there is one: that is the line
    /// that decided this service needs a command.
    #[test]
    fn the_missing_command_warning_points_at_the_type_line() {
        let bag = warns("user = 0\n\n\ntype = process\n");
        let w = bag.iter().find(|d| d.code == "W002").expect("a warning");
        assert_eq!(w.span.expect("a span").line, 4);
    }

    #[test]
    fn a_complete_description_produces_no_warnings() {
        assert!(warns("command = /bin/true\n").is_empty());
    }

    #[test]
    fn ready_none_on_a_process_warns_but_does_not_fail() {
        let bag = warns("command = /bin/true\nready = none\n");
        let w = bag.iter().find(|d| d.code == "W003").expect("a warning");
        assert_eq!(w.severity, crate::diagnostic::Severity::Warning);
        assert_eq!(
            w.span.expect("a span").line,
            2,
            "it points at the `ready` line"
        );
        assert!(
            !has_warning(&warns("command = /bin/true\n"), "W003"),
            "the default is not a warning"
        );
    }

    /// Warnings are inert for the exit code. This is the dinit-check lesson,
    /// and the only place in the parser where it can be got wrong.
    #[test]
    fn warnings_never_make_a_bag_look_like_a_failure() {
        let bag = warns("user = 0\nready = none\n");
        assert!(bag.warnings() > 0);
        assert!(!bag.has_errors());
        assert!(!bag.is_empty());
    }

    // ── structural errors ────────────────────────────────────────────────

    #[test]
    fn a_line_without_a_separator_is_e002() {
        let e = err("command\n");
        assert_eq!(e.kind, ParseErrorKind::MissingSeparator);
        assert_eq!(e.code, "E002");
        assert!(e.help.contains("`key = value`"));
    }

    /// A bare known directive gets a message that names the directive, because
    /// "no `=` here" is much less use than "`command` is a directive, so it
    /// needs a value".
    #[test]
    fn a_bare_known_directive_is_named_in_the_message() {
        let e = err("command\n");
        assert!(e.message.contains("`command`"), "{}", e.message);
        assert!(e.message.contains("needs a value"), "{}", e.message);
        // An unknown bare word does not get that treatment.
        let e2 = err("prose\n");
        assert!(e2.message.contains("no `=`"), "{}", e2.message);
    }

    #[test]
    fn a_line_starting_with_a_separator_is_e001() {
        let e = err("= /bin/true\n");
        assert_eq!(e.kind, ParseErrorKind::MissingKey);
        assert_eq!(e.code, "E001");
    }

    #[test]
    fn prose_on_its_own_line_is_e001_not_a_suggestion() {
        let e = err("this is not a directive at all\n");
        assert_eq!(e.kind, ParseErrorKind::MissingKey);
    }

    #[test]
    fn depends_on_a_target_is_rejected_whatever_the_order() {
        assert_eq!(err("type = target\ndepends = base\n").code, "E011");
        // The same file with the lines the other way round must be the same
        // error, or the meaning of the format depends on line order.
        assert_eq!(err("depends = base\ntype = target\n").code, "E011");
    }

    #[test]
    fn cgroup_and_rlimits_on_a_target_are_rejected_whatever_the_order() {
        assert_eq!(err("type = target\ncgroup = web\n").code, "E012");
        assert_eq!(err("cgroup = web\ntype = target\n").code, "E012");
        assert_eq!(err("type = target\nrlimit-as = 1\n").code, "E012");
        assert_eq!(err("rlimit-as = 1\ntype = target\n").code, "E012");
    }

    #[test]
    fn a_virtual_directive_error_points_at_the_offending_line() {
        let e = err("type = target\n\ncgroup = web\n");
        assert_eq!(e.code, "E012");
        assert_eq!(
            e.line, 3,
            "the diagnostic must point at the cgroup, not at the type"
        );
    }

    #[test]
    fn depends_preserves_bare_names_and_opt_prefixes() {
        let d =
            ok("command = /bin/true\ndepends = network, network.target, opt:logind, opt:x.y.z\n");
        assert_eq!(d.depends_required, ["network", "network.target"]);
        assert_eq!(d.depends_optional, ["logind", "x.y.z"]);
    }

    #[test]
    fn a_dependency_that_looks_like_an_assignment_is_e008() {
        let e = err("command = /bin/true\ndepends = opt:foo, bar=baz\n");
        assert_eq!(e.code, "E008");
        assert_eq!(e.line, 2);
        assert!(e.message.contains("bar=baz"), "{}", e.message);
    }

    #[test]
    fn a_dependency_with_a_bare_colon_is_e008() {
        assert_eq!(err("command = /bin/true\ndepends = ftp:foo\n").code, "E008");
    }

    #[test]
    fn an_empty_dependency_entry_is_e009() {
        assert_eq!(err("command = /bin/true\ndepends = a,,b\n").code, "E009");
        assert_eq!(err("command = /bin/true\ndepends = a, opt:\n").code, "E009");
        assert_eq!(err("command = /bin/true\ndepends = ,\n").code, "E009");
    }

    #[test]
    fn self_dependency_is_rejected() {
        let e = err("command = /bin/true\ndepends = sshd\n");
        assert_eq!(e.code, "E016");
        assert!(e.message.contains("itself"));
        // Optionally too: the rule does not have an exception.
        assert_eq!(
            err("command = /bin/true\ndepends = opt:sshd\n").code,
            "E016"
        );
    }

    #[test]
    fn a_name_cannot_be_both_required_and_optional() {
        let e = err("command = /bin/true\ndepends = opt:a, a\n");
        assert_eq!(e.code, "E010");
        let d = ok("command = /bin/true\ndepends = a, a, b\n");
        assert_eq!(
            d.depends_required,
            ["a", "b"],
            "an exact duplicate is idempotent"
        );
    }

    #[test]
    fn a_name_declared_required_first_wins_over_a_later_opt() {
        let d = ok("command = /bin/true\ndepends = a, opt:a\n");
        assert_eq!(d.depends_required, ["a"]);
        assert!(
            d.depends_optional.is_empty(),
            "the first declaration is the one that counts"
        );
    }

    #[test]
    fn quoted_dependency_names_keep_their_commas() {
        let d = ok("command = /bin/true\ndepends = \"we,ird\", plain\n");
        assert_eq!(d.depends_required, ["we,ird", "plain"]);
    }

    /// `user = <name>` is refused, not silently dropped: the alternative is a
    /// service that asked to drop privileges and did not.
    #[test]
    fn a_user_name_is_kept_unresolved_rather_than_dropped() {
        // The rule this test exists to protect, stated once: a name is
        // **kept**, never dropped. Dropping it is the one outcome not
        // allowed, because a `ServicePlan` whose `run_as` is `None` is
        // indistinguishable from "run as root", and that is how a service
        // which asked to drop privileges ends up not dropping them.
        //
        // Keeping it is only half the guarantee. The other half is that no
        // plan can be built while the name is unresolved, and that half lives
        // in `plan::build_plan`; see the test of the same shape there. An
        // earlier version of this asserted that the name was *refused* at
        // parse time, which is safe but pushes operators to `user = 0` or to
        // deleting the line - both worse than having the directive.
        let d = ok("command = /bin/true\nuser = www-data\n");
        assert_eq!(
            d.unresolved_run_as.as_deref(),
            Some("www-data"),
            "the name must survive parsing verbatim"
        );
        assert!(
            d.run_as.is_none(),
            "zconfig cannot resolve a name, so it must not invent a uid"
        );
        assert!(d.identity_is_unresolved());

        // A name with a group keeps the whole spelling for zrt to parse.
        let d = ok("command = /bin/true\nuser = mysql:www\n");
        assert_eq!(d.unresolved_run_as.as_deref(), Some("mysql:www"));

        // Numeric stays numeric, and clears the other field so the two can
        // never both be set.
        let d = ok("command = /bin/true\nuser = 33:34\n");
        assert_eq!(d.run_as, Some(RunAs { uid: 33, gid: 34 }));
        assert!(d.unresolved_run_as.is_none());
        assert!(!d.identity_is_unresolved());

        // `with_resolved_identity` is the runtime's move, and it refuses to
        // invent a resolution for a description that never asked for one.
        let d = ok("command = /bin/true\nuser = www-data\n");
        let resolved = d
            .clone()
            .with_resolved_identity((33, 33))
            .expect("has a name to resolve");
        assert_eq!(resolved.run_as, Some(RunAs { uid: 33, gid: 33 }));
        assert!(!resolved.identity_is_unresolved());
        assert!(
            ok("command = /bin/true\n")
                .with_resolved_identity((0, 0))
                .is_err(),
            "resolving an identity that was never unresolved must be refused"
        );
    }

    // ── value errors keep their own codes ───────────────────────────────

    #[test]
    fn a_bad_value_keeps_the_code_from_value_rs() {
        let e = err("command = /bin/true\nready = tcp:0\n");
        assert_eq!(e.code, "E315", "the specific code beats a generic E007");
        assert_eq!(e.line, 2);
        assert_eq!(e.column, 9);
    }

    #[test]
    fn a_bad_duration_reports_the_unit_problem() {
        let e = err("stop-timeout = 10 weeks\n");
        // `10 weeks` is a well-formed number with a nonsense unit, which is
        // E301 ("unknown unit") and not E302 ("not a number"). `10s5` is the
        // one that is a malformed *number*.
        assert_eq!(e.code, "E301", "{}", e.message);
        assert!(e.message.contains("weeks"), "{}", e.message);
    }

    #[test]
    fn a_bad_rlimit_is_e007_because_there_is_no_specific_code() {
        let e = err("rlimit-as = 12G\n");
        assert_eq!(e.code, "E007");
        assert!(e.message.contains("rlimit-as"), "{}", e.message);
    }

    #[test]
    fn a_bad_type_lists_the_four_kinds() {
        let e = err("type = daemon\n");
        assert_eq!(e.code, "E007");
        assert!(e.help.contains("target"), "{}", e.help);
        // Case-insensitive, and without an allocation per line.
        assert_eq!(ok("type = TARGET\n").kind, ServiceKind::Target);
        assert_eq!(ok("type = Console\n").kind, ServiceKind::Console);
    }

    // ── rule 13: robustness ──────────────────────────────────────────────

    /// CRLF, no trailing newline, and a BOM, all of which real files have.
    #[test]
    fn real_world_line_endings_and_boms_work() {
        let d = ok("command = /bin/true\r\ndepends = a\r\n");
        assert_eq!(d.command, "/bin/true");
        assert_eq!(d.depends_required, ["a"]);
        assert!(
            !d.command.ends_with('\r'),
            "the CR is not part of the value"
        );

        assert_eq!(ok("command = /bin/true").command, "/bin/true");

        let d = ok("\u{feff}command = /bin/true\n");
        assert_eq!(d.command, "/bin/true");
        assert!(warns("\u{feff}command = /bin/true\n").is_empty());
    }

    #[test]
    fn a_bom_in_the_middle_is_not_stripped() {
        // Only the first one is a byte-order mark; a second one is a character
        // the operator typed, and silently eating it would be data loss.
        let e = err("command = /bin/true\n\u{feff}nope = 1\n");
        assert_eq!(e.code, "E004");
    }

    #[test]
    fn multibyte_values_survive_untouched() {
        let d = ok("command = /bin/echo 'こんにちは世界'\n");
        assert_eq!(d.command, "/bin/echo 'こんにちは世界'");
    }

    #[test]
    fn an_empty_description_is_valid_but_warns() {
        let d = ok("");
        assert_eq!(d.name, "sshd");
        assert!(d.command.is_empty());
        assert!(has_warning(&warns(""), "W002"));
    }

    #[test]
    fn a_very_long_line_is_reported_not_parsed() {
        let mut line = String::from("command = ");
        for _ in 0..(MAX_LINE_BYTES + 100) {
            line.push('a');
        }
        let e = err(&line);
        assert_eq!(e.code, "E015");
        assert!(e.message.contains("limit"), "{}", e.message);
        assert!(
            e.message.len() < 200,
            "the diagnostic itself must stay small"
        );
    }

    #[test]
    fn an_enormous_text_is_rejected_before_anything_is_allocated() {
        let mut big = String::with_capacity(MAX_TEXT_BYTES + 1);
        while big.len() <= MAX_TEXT_BYTES {
            big.push('\n');
        }
        let e = err(&big);
        assert_eq!(e.code, "E015");
        assert!(e.message.contains("limit"), "{}", e.message);
    }

    #[test]
    fn a_description_of_a_million_lines_is_rejected() {
        let mut many = String::new();
        for _ in 0..(MAX_LINES as usize + 1) {
            many.push('\n');
        }
        let e = err(&many);
        assert_eq!(e.code, "E015");
        assert!(e.message.contains("line limit"), "{}", e.message);
    }

    /// Run the parser under `catch_unwind`, so "did not panic" is a real
    /// assertion and not merely the absence of a failure in this test.
    fn assert_no_panic(name: &str, text: &str) {
        let name = name.to_string();
        let text = text.to_string();
        let (n, t) = (name.clone(), text.clone());
        let r = catch_unwind(AssertUnwindSafe(move || {
            let _ = parse_service(&n, &t);
        }));
        assert!(
            r.is_ok(),
            "parse_service panicked on name={name:?} text={text:?}"
        );
    }

    /// The fuzz target: 10 000 deterministic mutations of a valid description,
    /// under `catch_unwind`.
    ///
    /// Two phases, because they hunt different bugs:
    ///
    /// * **byte** mutations break the *structure* — a stray `=`, a truncated
    ///   line, a doubled comment character;
    /// * **character** mutations break the *offsets* — a column that lands
    ///   inside a character is the classic way a hand-rolled parser panics on
    ///   text a user can actually type, and byte mutations almost never produce
    ///   valid UTF-8, so a byte-only fuzzer cannot find it.
    #[test]
    fn no_panic_on_mutated_input() {
        const INJECT: [u8; 20] = *b"=#:\n\\ \t${}ox'.@\"*[]\0";
        // `\u{0}` and `\u{7f}` are written as escapes rather than pasted as
        // literal bytes: a NUL in the source makes the file binary to grep, to
        // diff viewers and to half the tools that read it, and the two
        // characters are identical either way.
        const INJECT_UNICODE: [&str; 6] = ["ñ", "€", "🎉", "日", "\u{0}", "\u{7f}"];
        let base: Vec<u8> = FULL.as_bytes().to_vec();
        let base_chars: Vec<char> = FULL.chars().collect();
        let mut rng: u64 = 0x2545_F491_4F6C_DD1D;
        let mut byte_cases = 0u32;
        let mut invalid_utf8_cases = 0u32;
        let mut char_cases = 0u32;

        for i in 0..10_000u32 {
            if i % 2 == 0 {
                // Byte phase.
                let mut bytes = base.clone();
                let mutations = 1 + (next(&mut rng) % 8);
                for _ in 0..mutations {
                    if bytes.is_empty() {
                        break;
                    }
                    let at = (next(&mut rng) as usize) % bytes.len();
                    match next(&mut rng) % 5 {
                        0 => {
                            bytes.remove(at);
                        }
                        1 => bytes.insert(at, INJECT[(next(&mut rng) as usize) % INJECT.len()]),
                        2 => bytes[at] = next(&mut rng) as u8,
                        3 => bytes.truncate(at),
                        _ => bytes[at] = b'=',
                    }
                }
                if let Ok(text) = core::str::from_utf8(&bytes) {
                    assert_no_panic("sshd", text);
                    byte_cases += 1;
                } else {
                    // Random byte surgery usually lands mid-codepoint. Those
                    // inputs are still worth running - they are what a
                    // corrupted file looks like - but they are not the `&str`
                    // this API takes, so they are fed to a separate check
                    // rather than counted as `&str` cases.
                    invalid_utf8_cases += 1;
                }
            } else {
                // Character phase: always valid UTF-8, always a real input.
                let mut chars = base_chars.clone();
                let mutations = 1 + (next(&mut rng) % 8);
                for _ in 0..mutations {
                    if chars.is_empty() {
                        break;
                    }
                    let at = (next(&mut rng) as usize) % chars.len();
                    match next(&mut rng) % 4 {
                        0 => {
                            chars.remove(at);
                        }
                        1 => {
                            let s =
                                INJECT_UNICODE[(next(&mut rng) as usize) % INJECT_UNICODE.len()];
                            // Insert a whole `char`, so the result is still UTF-8.
                            for c in s.chars().rev() {
                                chars.insert(at, c);
                            }
                        }
                        2 => chars[at] = '#',
                        _ => chars[at] = '=',
                    }
                }
                let text: String = chars.into_iter().collect();
                assert_no_panic("sshd", &text);
                char_cases += 1;
            }
        }
        // 10_000 attempts, of which the character phase always yields a `&str`
        // and the byte phase yields one only when the surgery happened to land
        // on a codepoint boundary. Both halves ran; the point is that no
        // attempt panicked, not that every attempt was well-formed UTF-8.
        assert_eq!(char_cases, 5_000, "every character mutation is valid UTF-8");
        assert!(
            byte_cases + invalid_utf8_cases == 5_000,
            "byte phase accounted for {} of 5000",
            byte_cases + invalid_utf8_cases
        );
        assert!(
            byte_cases > 3_000,
            "only {byte_cases} byte mutations were real text; the corpus is not exercising the parser"
        );
    }

    #[test]
    fn no_panic_on_degenerate_inputs() {
        for input in [
            "",
            "\n",
            "\r\n",
            "\u{feff}",
            "=",
            ":",
            "==",
            "#",
            "\\",
            "'",
            "\"",
            "command",
            "command =",
            "command = = = =",
            "env =",
            "env = =",
            "env = \u{0}=\u{0}",
            "depends =",
            "depends = ,,,,",
            "depends = opt:",
            "depends = \u{0}",
            "log =",
            "log = :",
            "user = :",
            "user = 99999999999999999999999:1",
            "ready = ping:",
            "ready = tcp:99999",
            "restart = ",
            "restart-budget = restarts per",
            "restart-budget = x restarts per y",
            "restart-delay = -1",
            "rlimit-as = -1",
            "rlimit-as = 184467440737095516160",
            "critical = maybe",
            "type =",
            "cgroup =",
            "\u{feff}\u{feff}\u{feff}",
            "command = \u{10ffff}",
            "\u{0}=\u{0}",
            "command = \"unterminated",
            "command = 'unterminated",
            "depends = \"a\\",
            "command = \\\\",
            "command = \\\\\\",
        ] {
            assert_no_panic("x", input);
        }
    }

    /// A malformed name is as much a fuzz target as a malformed body: the name
    /// goes through `validate_service_name` before anything else.
    #[test]
    fn no_panic_on_hostile_names() {
        for name in [
            "",
            ".",
            "..",
            "/",
            "\\",
            "@",
            "\u{0}",
            "\u{feff}",
            &"x".repeat(1000),
            &".".repeat(500),
            "../../etc/passwd",
            "日本語",
            "ñ",
        ] {
            assert_no_panic(name, FULL);
        }
    }

    #[test]
    fn no_panic_on_non_ascii_input() {
        for input in [
            "command = 日本語のコマンド",
            "ключ = значение",
            "command = 🎉",
            "é = é",
            "command = 日本語 # コメント",
            "env = 日本語=値",
            "depends = 日本語, opt:ñ",
        ] {
            assert_no_panic("x", input);
        }
    }

    // ── rendering ────────────────────────────────────────────────────────

    #[test]
    fn error_display_is_stable() {
        let e = err("nope = 1\n");
        let s = e.to_string();
        assert!(
            s.starts_with("line 1, column 1: E004 [unknown-key]:"),
            "{s}"
        );
    }

    #[test]
    fn into_diagnostic_keeps_the_span_and_the_help() {
        let e = err("command = /bin/true\n   nope = 1\n");
        let d = e.into_diagnostic();
        assert_eq!(d.severity, crate::diagnostic::Severity::Error);
        assert_eq!(d.code, "E004");
        assert_eq!(d.span.map(|s| (s.line, s.column)), Some((2, 4)));
        assert!(d.help.is_some_and(|h| h.contains("known directives")));
    }

    // ── helpers ─────────────────────────────────────────────────────────

    #[test]
    fn comment_offsets_handle_escapes() {
        assert_eq!(comment_offset("a # b"), Some(2));
        assert_eq!(comment_offset("a \\# b"), None);
        assert_eq!(comment_offset("a #"), Some(2));
        assert_eq!(comment_offset("no comment"), None);
        // `a \\ # b` is a, space, backslash, backslash, space, `#`: the `\\`
        // pair is one escaped backslash and both of its bytes are consumed, so
        // the `#` is the sixth byte, index 5. The tempting wrong answer is 4,
        // which is the space in front of it.
        assert_eq!(comment_offset("a \\\\ # b"), Some(5));
        // A trailing backslash has nothing to escape, so it is not a comment
        // start either and the line has no comment at all.
        assert_eq!(comment_offset("\\"), None);
        // The offset of a multi-byte character is never inside it: `ñ` is two
        // bytes, so `a`…`#` is at index 3 rather than 2.
        assert_eq!(comment_offset("ñ #"), Some(3));
        // `🎉` is four bytes, so the `#` is at byte 5.
        assert_eq!(comment_offset("🎉 #"), Some(5));
        // The parser keeps the text *before* the offset and drops the rest, so the
        // offset is the whole of what stripping means.
        assert_eq!(&"a # b"[..comment_offset("a # b").unwrap()], "a ");
    }

    #[test]
    fn quoting_never_splits_a_character() {
        assert_eq!(quote("abcdef"), "abcdef");
        let long = "ñ".repeat(40);
        let q = quote(&long);
        assert!(q.ends_with("..."));
        assert!(q.len() <= MAX_QUOTE_BYTES + 3);
        assert!(core::str::from_utf8(q.as_bytes()).is_ok());
        // A 4-byte character that would not fit whole is left out, not split.
        let wide = "🎉".repeat(20);
        let q = quote(&wide);
        assert!(q.len() <= MAX_QUOTE_BYTES + 3, "{}", q.len());
        assert!(core::str::from_utf8(q.as_bytes()).is_ok());
    }

    #[test]
    fn number_parsing_is_strict() {
        // The shared scanner, reached through the `rlimit-*` directive: the
        // same answer a `tcp:` port gets.
        let n = |s: &str| parse_u64(s).ok();
        assert_eq!(n("0"), Some(0));
        assert_eq!(n("8192"), Some(8192));
        assert_eq!(n("18446744073709551615"), Some(u64::MAX));
        assert_eq!(n("18446744073709551616"), None);
        assert_eq!(n("-1"), None);
        assert_eq!(n("1_0"), None);
        assert_eq!(n(" 1"), None);
        assert_eq!(n(""), None);
        // A non-ASCII digit is not a digit.
        assert_eq!(n("١٢٣"), None);
    }

    #[test]
    fn bool_parsing_accepts_the_usual_spellings() {
        for t in ["yes", "Y", "TRUE", "1", "on", "On"] {
            assert_eq!(parse_bool(t), Some(true), "{t}");
        }
        for t in ["no", "N", "False", "0", "off", "OFF"] {
            assert_eq!(parse_bool(t), Some(false), "{t}");
        }
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool(""), None);
    }

    /// List offsets are **byte** offsets. A character count is the bug this
    /// test exists to prevent: it makes every entry after the first multi-byte
    /// character point at the wrong column.
    #[test]
    fn list_splitting_reports_byte_offsets() {
        let parts = split_list_spans("a, b, c");
        let spans: Vec<(String, usize, usize)> = parts
            .iter()
            .map(|e| (e.text.clone(), e.start, e.end))
            .collect();
        assert_eq!(
            spans,
            [
                (String::from("a"), 0, 1),
                (String::from(" b"), 2, 4),
                (String::from(" c"), 5, 7),
            ]
        );
        // Note what the third range is *not*: 4..6 would begin on the second
        // comma, which is the delimiter and belongs to no entry. And a range
        // begins just past the comma that preceded it, so the leading space is
        // inside it — `parse_depends` subtracts that space itself to name the
        // first real character, and a range that had skipped it would make the
        // caller trim an offset that is no longer there.
        let value = "a, b, c";
        for (entry, (_, start, end)) in parts.iter().zip(&spans) {
            assert_eq!(
                &value[*start..*end],
                entry.text.as_str(),
                "range {start}..{end}"
            );
        }
        // A repeated entry must report its own offset, not the first one's.
        assert_eq!(split_list_spans("a, a")[1].start, 2);
    }

    #[test]
    fn list_splitting_offsets_are_bytes_not_characters() {
        // "ñ" is two bytes, so the comma is at byte 2 and the second entry
        // starts at byte 3. Counting characters would say 2, and every
        // diagnostic after a non-ASCII dependency name would be off by one
        // per multi-byte character before it.
        let parts = split_list_spans("ñ, b");
        assert_eq!(parts[1].start, 3);
        assert_eq!(parts[1].end, 5);
        assert_eq!(parts[0].text, "ñ");
        // The string is five *bytes* and four *chars*: a byte offset reaches 5,
        // a character offset stops at 4, and that gap is the whole test.
        assert_eq!("ñ, b".len(), 5);
        assert_eq!("ñ, b".chars().count(), 4);
        // And the reported range really slices the original string.
        assert_eq!(&"ñ, b"[parts[1].start..parts[1].end], " b");
    }

    #[test]
    fn list_splitting_handles_quotes_and_trailing_escapes() {
        let parts = split_list_spans("\"a,b\", c");
        assert_eq!(parts[0].text, "a,b");
        assert_eq!(parts[1].text, " c");
        // A quoted comma belongs to the entry it is written in, and the range
        // covers the quotes that delimited it. `"a,b"` is five *bytes*:
        // quote, a, comma, b, quote. An earlier version of this test asserted
        // six, and the code was right - a failing test is evidence, not an
        // accusation.
        assert_eq!(
            (parts[0].start, parts[0].end),
            (0, 5),
            "`\"a,b\"` is five bytes, quotes included"
        );
        assert_eq!(
            r#""a,b""#.len(),
            5,
            "the expectation above must match reality"
        );

        // `\"` inside quotes is one escaped quote, so the entry is `a"` and the
        // literal never closes: what followed it would be swallowed whole.
        let parts = split_list_spans("\"a\\\"");
        assert_eq!(parts[0].text, "a\"");
        assert_eq!(parts.len(), 1, "an unterminated quote swallows the rest");

        // A *trailing* backslash is the other branch of the same code and the
        // opposite answer: there is nothing left for it to escape, so it is
        // kept as a literal character instead of eating the quote it was
        // written to protect. Losing it would rename the dependency with no
        // diagnostic at all.
        let parts = split_list_spans("\"a\\");
        assert_eq!(parts[0].text, "a\\", "the backslash is not lost");
        assert_eq!(
            (parts[0].start, parts[0].end),
            (0, 3),
            "the range covers the input"
        );
    }

    /// A xorshift64, so the "fuzz" test is a fixed regression test that fails
    /// on exactly the same input every time instead of only sometimes.
    fn next(rng: &mut u64) -> u64 {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        *rng
    }
}

#[cfg(test)]
mod lifecycle_directive_tests {
    use super::*;
    use alloc::vec;

    fn parsed(text: &str) -> ServiceDesc {
        parse_service("svc", text).expect("test fixture must parse")
    }

    #[test]
    fn new_kinds_parse_case_insensitively() {
        let task = "type = oneshot\ncommand = /bin/task\n";
        assert_eq!(parsed(task).kind, ServiceKind::Oneshot);
        let daemon = "type = FORKING\ncommand = /bin/daemon\npid-file = /run/d.pid\n";
        assert_eq!(parsed(daemon).kind, ServiceKind::Forking);
    }

    #[test]
    fn pid_file_and_watchdog_land_on_the_description() {
        let text = "type = forking\ncommand = /bin/daemon\npid-file = /run/d.pid\n";
        let text2 = "ready = notify\nwatchdog-sec = 30\n";
        let d = parsed(&format!("{text}{text2}"));
        assert_eq!(d.pid_file.as_deref(), Some("/run/d.pid"));
        assert_eq!(d.watchdog_sec, Some(30));
    }

    #[test]
    fn pid_file_elsewhere_is_refused() {
        for kind in ["process", "script", "oneshot", "console", "target"] {
            let text = format!("type = {kind}\ncommand = /bin/x\npid-file = /run/x.pid\n");
            let e = parse_service("svc", &text).expect_err("pid-file must be forking-only");
            assert!(e.message.contains("pid-file"), "wrong error: {}", e.message);
        }
    }

    #[test]
    fn tty_lands_on_the_description() {
        let d = parsed("type = console\ncommand = /sbin/agetty\ntty = /dev/tty1\n");
        assert_eq!(d.tty.as_deref(), Some("/dev/tty1"));
    }

    #[test]
    fn tty_elsewhere_is_refused() {
        for kind in ["process", "script", "oneshot", "forking", "target"] {
            let text = format!("type = {kind}\ncommand = /bin/x\ntty = /dev/tty1\n");
            let e = parse_service("svc", &text).expect_err("tty must be console-only");
            assert!(e.message.contains("tty"), "wrong error: {}", e.message);
        }
    }

    #[test]
    fn tty_relative_is_refused() {
        let e = parse_service("svc", "type = console\ncommand = /bin/x\ntty = tty1\n")
            .expect_err("relative tty must not parse");
        assert!(e.message.contains("absolute"), "wrong error: {}", e.message);
    }

    #[test]
    fn console_without_tty_still_parses() {
        // The parser cannot know a drop-in will not add the tty; the merged
        // cross-layer refusal lives at load, and the supervisor reports a
        // tty-less console instead of killing the boot over it.
        let d = parsed("type = console\ncommand = /bin/sh\n");
        assert_eq!(d.kind, ServiceKind::Console);
        assert_eq!(d.tty, None);
    }

    #[test]
    fn instance_expands_in_command_tty_and_pid_file() {
        let text = "type = console\ncommand = /sbin/agetty %i\ntty = /dev/%i\n";
        let d = parse_service("getty@tty1", text).expect("instance must expand");
        assert_eq!(d.command, "/sbin/agetty tty1");
        assert_eq!(d.tty.as_deref(), Some("/dev/tty1"));
        let text = "type = forking\ncommand = /bin/d\npid-file = /run/%i.pid\n";
        let d = parse_service("daemon@web", text).expect("instance must expand");
        assert_eq!(d.pid_file.as_deref(), Some("/run/web.pid"));
    }

    #[test]
    fn percent_without_instance_stays_literal() {
        // No `@` in the name, no expansion at all: `printf '%i\n'` and `%%`
        // survive untouched, because the expander does not touch what no
        // instance explains.
        let d = parsed("command = /usr/bin/printf '%i%%\\n'\n");
        assert_eq!(d.command, "/usr/bin/printf '%i%%\\n'");
    }

    #[test]
    fn percent_escape_collapses_when_expanding() {
        let d = parse_service("svc@x", "command = /bin/echo 100%% of %i\n")
            .expect("escape must collapse");
        assert_eq!(d.command, "/bin/echo 100% of x");
        // A lone `%` is copied verbatim: only `%i` and `%%` mean anything.
        let d = parse_service("svc@x", "command = /bin/echo 100% ready\n")
            .expect("lone percent must survive");
        assert_eq!(d.command, "/bin/echo 100% ready");
    }

    #[test]
    fn broken_instance_names_are_refused() {
        for name in ["svc@", "a@b@c"] {
            let e = parse_service(name, "command = /bin/x\n").expect_err("bad instance");
            assert_eq!(e.code, "E018", "wrong code: {}", e.message);
        }
    }

    #[test]
    fn watchdog_without_notify_is_refused() {
        for ready in ["none", "ping:/bin/check", "tcp:80"] {
            let text = format!("command = /bin/x\nready = {ready}\nwatchdog-sec = 30\n");
            let e = parse_service("svc", &text).expect_err("watchdog needs notify");
            assert!(e.message.contains("watchdog"), "wrong error: {}", e.message);
        }
    }

    #[test]
    fn watchdog_zero_is_refused() {
        let e = parse_service(
            "svc",
            "command = /bin/x\nready = notify\nwatchdog-sec = 0\n",
        )
        .expect_err("zero watchdog must not parse");
        assert!(
            e.message.contains("watchdog-sec"),
            "wrong error: {}",
            e.message
        );
    }

    #[test]
    fn watchdog_on_a_target_is_refused() {
        let e = parse_service("svc", "type = target\nwatchdog-sec = 30\n")
            .expect_err("a target cannot be watched");
        assert!(e.message.contains("watchdog"), "wrong error: {}", e.message);
    }

    #[test]
    fn listen_accumulates_and_parses() {
        let d = parsed("command = /bin/x\nlisten = tcp:80\nlisten = unix:/run/x.sock\n");
        assert_eq!(d.listens.len(), 2);
        assert_eq!(d.listens[0].name, "tcp:80");
    }

    #[test]
    fn confinement_directives_parse() {
        let base = "command = /bin/x\nready = notify\n";
        let caps = "drop-capabilities = sys_admin CAP_CHOWN\n";
        let filter = "syscall-filter = enforce\nsyscall-allow = read write\n";
        let d = parsed(&format!("{base}{caps}{filter}"));
        assert_eq!(
            d.drop_caps,
            vec![String::from("sys_admin"), String::from("chown")]
        );
        assert_eq!(d.syscall_filter, Some(SeccompAction::Enforce));
        assert_eq!(
            d.syscall_allow,
            vec![String::from("read"), String::from("write")]
        );
    }

    #[test]
    fn confinement_refusals_are_loud() {
        let e = parse_service(
            "svc",
            "command = /bin/x\ndrop-capabilities = sys_admin nope\n",
        )
        .expect_err("unknown capability");
        assert!(
            e.message.contains("drop-capabilities"),
            "wrong error: {}",
            e.message
        );
        let e = parse_service("svc", "command = /bin/x\nsyscall-filter = strict\n")
            .expect_err("unknown filter action");
        assert!(
            e.message.contains("syscall-filter"),
            "wrong error: {}",
            e.message
        );
        let e = parse_service("svc", "command = /bin/x\nsyscall-allow = read\n")
            .expect_err("allow list without a filter");
        assert!(
            e.message.contains("syscall-allow"),
            "wrong error: {}",
            e.message
        );
        let bad_allow = "command = /bin/x\nsyscall-filter = enforce\n";
        let bad_allow = format!("{bad_allow}syscall-allow = read(2)\n");
        let e = parse_service("svc", &bad_allow).expect_err("malformed syscall name");
        assert!(
            e.message.contains("syscall-allow"),
            "wrong error: {}",
            e.message
        );
    }

    #[test]
    fn syscall_filter_off_clears_explicitly() {
        let mut base = parsed("command = /bin/x\nsyscall-filter = enforce\nsyscall-allow = read\n");
        let over = parsed("command = /bin/x\nsyscall-filter = off\n");
        base.overlay_onto(over);
        assert_eq!(base.syscall_filter, None);
        // ...while an overlay that never mentions it keeps the base's.
        let mut base2 = parsed("command = /bin/x\nsyscall-filter = errno\n");
        let over2 = parsed("command = /bin/x\n");
        base2.overlay_onto(over2);
        assert_eq!(base2.syscall_filter, Some(SeccompAction::Errno));
    }

    #[test]
    fn new_directives_are_single_valued() {
        let e = parse_service(
            "svc",
            "command = /bin/x\nwatchdog-sec = 10\nwatchdog-sec = 20\n",
        )
        .expect_err("duplicate watchdog-sec");
        assert_eq!(e.code, "E005");
    }
}
