//! Diagnostics with positions: the only channel through which a bad service
//! description reaches the operator.
//!
//! This module exists because parsing an init description is not a
//! compile-time operation: by the time we notice `restrat = always` nobody is
//! holding a rustc pointer to blame. A diagnostic therefore has to carry its
//! own position, or the only honest thing left to do is point at the file.
//!
//! Two rules are encoded here, both learned from dinit's `dinit-check`:
//!
//! * **Only [`Severity::Error`] fails the build.** In dinit-check the lint
//!   severity `L` increments `errors_found` and exits 1, so a description that
//!   is merely *suspicious* is indistinguishable from one that is *broken*, and
//!   people learn to ignore the tool. Here [`DiagnosticBag::has_errors`] — the
//!   single predicate a caller is allowed to consult for the exit code — looks
//!   at errors and nothing else. A bag of a thousand warnings is a passing run.
//! * **A diagnostic names the thing it is about.** dinit's
//!   `validate_dep_name_x` shadows `name` and reports the *dependency's* name
//!   as if it were the service's, so the error points at the wrong service and
//!   the reader wastes a cycle on a file that is fine. Nothing in this module
//!   takes a `name` to report on: the caller builds the message from the
//!   description it is actually looking at.
//!
//! Everything here is `core::fmt` only — `no_std`, no `std::error::Error`
//! (that trait needs `std`), just `Display`.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
// `write!` into a `String` needs the trait itself in scope; the module path
// alone does not bring it.
use core::fmt::Write as _;

/// A source position, 1-based in both axes, like every editor and every
/// compiler humans have ever used.
///
/// `len` is the extent of the offending token in bytes, and is `0` when we
/// only know a point. It is a byte length, not a character count: it is fed to
/// a byte-offset highlighter, and mixing the two would mis-highlight any
/// description with a non-ASCII comment in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Span {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, counted in bytes from the start of the line.
    pub column: u32,
    /// Length of the offending run in bytes. `0` = point diagnostic.
    pub len: u32,
}

impl Span {
    /// A point span, for errors about a whole line or a whole file.
    pub const fn point(line: u32, column: u32) -> Span {
        Span {
            line,
            column,
            len: 0,
        }
    }

    /// A span covering `len` bytes starting at `column`.
    pub const fn range(line: u32, column: u32, len: u32) -> Span {
        Span { line, column, len }
    }

    /// The byte offset just past this span, measured from the start of the
    /// line. `0` (point) yields the column itself, so the common
    /// "take everything from here" case needs no special case.
    pub const fn end_column(&self) -> u32 {
        self.column.saturating_add(self.len)
    }
}

/// How much a diagnostic matters.
///
/// This enum is the *whole* severity policy of the crate, and the ordering is
/// load-bearing: only the first variant may influence the process exit code.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Severity {
    /// The description cannot be turned into a plan. This, and only this, makes
    /// `zcheck` exit non-zero.
    Error,
    /// The description is legal but probably not what the author meant. Printed,
    /// never fatal. Examples: a log path under a world-writable directory, a
    /// `ready` handshake on a service that ignores `ZINIT_NOTIFY_FD`.
    Warning,
    /// Context for a human, or a suppressed lint promoted to visible. Never
    /// fatal, never counted.
    Note,
}

impl Severity {
    /// The word used in the rendered line, lowercase, as compilers do.
    pub const fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }

    /// Whether a diagnostic of this severity may change the exit code.
    ///
    /// The name says *may*: it is the flag a CLI is allowed to read, and it is
    /// true for exactly one variant. Everything else — including every future
    /// variant someone adds to this enum — is inert with respect to the build.
    pub const fn is_fatal(self) -> bool {
        matches!(self, Severity::Error)
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One thing wrong, with enough information to fix it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    /// Decides fatality. See [`Severity`].
    pub severity: Severity,
    /// Where it is. `None` for file-level problems (duplicate service name,
    /// unreadable file) where pointing at a column would be a lie.
    pub span: Option<Span>,
    /// Stable machine-readable identifier, `"E001"` / `"W004"`. Stable because
    /// `zcheck` output gets grepped and referenced in bug reports; the number
    /// must not be reshuffled when new codes are added, so old codes are
    /// retired, never reused.
    pub code: &'static str,
    /// What went wrong, in the present tense, naming the offending value.
    pub message: String,
    /// What to write instead. Absent when we genuinely have no better idea;
    /// a `help` that only restates the message is noise, so we omit it.
    pub help: Option<String>,
}

impl Diagnostic {
    /// Build an error. Fatal — this is the only constructor a caller should
    /// reach for when it wants the check to fail.
    pub fn error(
        span: Option<Span>,
        code: &'static str,
        message: impl Into<String>,
        help: Option<String>,
    ) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            span,
            code,
            message: message.into(),
            help,
        }
    }

    /// Build a warning. Never fatal. See [`DiagnosticBag::has_errors`].
    pub fn warning(
        span: Option<Span>,
        code: &'static str,
        message: impl Into<String>,
        help: Option<String>,
    ) -> Diagnostic {
        Diagnostic {
            severity: Severity::Warning,
            span,
            code,
            message: message.into(),
            help,
        }
    }

    /// Build a note. Never fatal, and never counted in [`DiagnosticBag::warnings`].
    pub fn note(
        span: Option<Span>,
        code: &'static str,
        message: impl Into<String>,
        help: Option<String>,
    ) -> Diagnostic {
        Diagnostic {
            severity: Severity::Note,
            span,
            code,
            message: message.into(),
            help,
        }
    }

    /// Whether this diagnostic may change the exit code.
    pub const fn is_error(&self) -> bool {
        self.severity.is_fatal()
    }

    /// Pair this diagnostic with the file it came from, for full rendering.
    ///
    /// A diagnostic does not store a path: the same description can be checked
    /// from a file, from `<stdin>` or from a string literal in a test, and the
    /// name is the caller's knowledge, not the parser's.
    pub fn located<'a>(&'a self, file: &'a str) -> Located<'a> {
        Located {
            file,
            diagnostic: self,
        }
    }

    /// Full rustc-style rendering, including the file name and the help line.
    pub fn render(&self, file: &str) -> String {
        let mut out = String::new();
        render_body(&mut out, Some(file), self);
        out
    }
}

/// A [`Diagnostic`] together with the file it was parsed from.
///
/// This is the type that renders `archivo:línea:col: error[E001]: mensaje`,
/// because the file name lives here and not in [`Diagnostic`]. Print one with
/// `println!("{}", d.located(path))`, or render the whole bag with
/// [`DiagnosticBag::render`].
#[derive(Clone, Copy, Debug)]
pub struct Located<'a> {
    /// Path as the operator knows it. `"<stdin>"` for inline descriptions.
    pub file: &'a str,
    /// The diagnostic being located.
    pub diagnostic: &'a Diagnostic,
}

impl fmt::Display for Located<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = String::new();
        render_body(&mut buf, Some(self.file), self.diagnostic);
        f.write_str(&buf)
    }
}

impl fmt::Display for Diagnostic {
    /// Renders `<line>:<col>: <severity>[<code>]: <message>`, plus a
    /// `  = help: <help>` line when there is one.
    ///
    /// The file prefix of the canonical `archivo:línea:col:` form is *absent*
    /// here because `core::fmt::Display` cannot be handed extra data and
    /// [`Diagnostic`] does not store a path. Use [`Diagnostic::render`] or
    /// [`Diagnostic::located`] when you have the file name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = String::new();
        render_body(&mut buf, None, self);
        f.write_str(&buf)
    }
}

/// The single rendering routine, so every entry point agrees byte for byte.
fn render_body(out: &mut String, file: Option<&str>, d: &Diagnostic) {
    // The location prefix is optional as a whole and each half of it is
    // independently optional, so all four combinations have to be spelled out
    // rather than bolted together with separators that may not be wanted.
    match (file, d.span) {
        (Some(file), Some(s)) => {
            let _ = write!(out, "{file}:{}:{}: ", s.line, s.column);
        }
        (Some(file), None) => {
            // File-level problem: a dash is the only honest position.
            let _ = write!(out, "{file}:-: ");
        }
        (None, Some(s)) => {
            let _ = write!(out, "{}:{}: ", s.line, s.column);
        }
        (None, None) => {}
    }
    out.push_str(d.severity.label());
    out.push('[');
    out.push_str(d.code);
    out.push_str("]: ");
    out.push_str(&d.message);
    if let Some(help) = &d.help {
        out.push_str("\n  = help: ");
        out.push_str(help);
    }
}

/// An ordered collection of diagnostics.
///
/// Order is insertion order and is never sorted: the parser emits in source
/// order, and that is the order a human reads the file in. Sorting by
/// severity would bury the first error, which is the one the author needs.
///
/// The bag is the *only* thing a caller should ask for the exit code. There is
/// deliberately no `errors_found()`-style counter that gets bumped by
/// everything, because that is the dinit-check bug.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DiagnosticBag {
    items: Vec<Diagnostic>,
}

impl DiagnosticBag {
    /// An empty bag.
    pub const fn new() -> DiagnosticBag {
        DiagnosticBag { items: Vec::new() }
    }

    /// Append a diagnostic, preserving order.
    pub fn push(&mut self, d: Diagnostic) {
        self.items.push(d);
    }

    /// Number of diagnostics of any severity, in insertion order.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when nothing at all was reported. Equivalent to `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Iterates in insertion order.
    pub fn iter(&self) -> core::slice::Iter<'_, Diagnostic> {
        self.items.iter()
    }

    /// Number of `Error` diagnostics. *Not* the exit code on its own — see
    /// [`DiagnosticBag::has_errors`], which is the predicate to use.
    pub fn errors(&self) -> usize {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    /// Number of `Warning` diagnostics. Notes are excluded on purpose: a note
    /// is context, not something to nag about.
    pub fn warnings(&self) -> usize {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count()
    }

    /// **The exit-code predicate.** True if and only if at least one
    /// diagnostic has [`Severity::Error`].
    ///
    /// Warnings and notes are inert. A thousand warnings is a passing
    /// `zcheck`. If you need warnings to fail a run, that run is a lint gate,
    /// not a validity check, and it belongs behind an explicit flag in the
    /// caller — never smuggled in here, where a parser and a linter cannot be
    /// told apart.
    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity.is_fatal())
    }

    /// Consume the bag, yielding the diagnostics in insertion order.
    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.items
    }

    /// Render every diagnostic, one per diagnostic, separated by newlines, in
    /// insertion order and each prefixed with `file`. Empty string when the bag
    /// is empty, so `zcheck` can print it unconditionally.
    pub fn render(&self, file: &str) -> String {
        let mut out = String::new();
        for (i, d) in self.items.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&d.render(file));
        }
        out
    }

    /// The first `Error`, if any. Handy for `zcheck` when it wants to exit
    /// with a one-line reason instead of the full report.
    pub fn first_error(&self) -> Option<&Diagnostic> {
        self.items.iter().find(|d| d.severity.is_fatal())
    }
}

impl<'a> IntoIterator for &'a DiagnosticBag {
    type Item = &'a Diagnostic;
    type IntoIter = core::slice::Iter<'a, Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl From<Vec<Diagnostic>> for DiagnosticBag {
    fn from(items: Vec<Diagnostic>) -> DiagnosticBag {
        DiagnosticBag { items }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn at(line: u32) -> Option<Span> {
        Some(Span::range(line, 1, 4))
    }

    #[test]
    fn render_has_file_line_col_severity_code_and_message() {
        let d = Diagnostic::error(at(12), "E001", "unknown directive `restrat`", None);
        assert_eq!(
            d.render("sshd.conf"),
            "sshd.conf:12:1: error[E001]: unknown directive `restrat`"
        );
    }

    #[test]
    fn located_display_matches_render() {
        let d = Diagnostic::error(at(2), "E001", "broken", None);
        assert_eq!(d.located("a.conf").to_string(), d.render("a.conf"));
    }

    #[test]
    fn bare_display_omits_the_file_prefix() {
        let d = Diagnostic::error(at(5), "E002", "bad value", None);
        assert_eq!(d.to_string(), "5:1: error[E002]: bad value");
    }

    #[test]
    fn filelevel_diagnostic_prints_a_dash_instead_of_a_line() {
        let d = Diagnostic::error(None, "E010", "duplicate service name `sshd`", None);
        assert_eq!(
            d.render("services.d/sshd.conf"),
            "services.d/sshd.conf:-: error[E010]: duplicate service name `sshd`"
        );
    }

    #[test]
    fn help_is_rendered_on_its_own_line() {
        let d = Diagnostic::error(
            at(3),
            "W004",
            "log path is world-writable",
            Some("use /var/log/zinit/<svc>.log".to_string()),
        );
        let s = d.render("f.conf");
        assert!(
            s.contains("\n  = help: use /var/log/zinit/<svc>.log"),
            "got: {s}"
        );
    }

    #[test]
    fn warning_and_note_render_their_own_labels() {
        let w = Diagnostic::warning(at(1), "W001", "deprecated", None);
        assert!(w.render("f.conf").contains("warning[W001]"));
        let n = Diagnostic::note(at(1), "N001", "context", None);
        assert!(n.render("f.conf").contains("note[N001]"));
    }

    #[test]
    fn bag_render_joins_with_newlines_in_order() {
        let mut bag = DiagnosticBag::new();
        bag.push(Diagnostic::error(at(1), "E001", "first", None));
        bag.push(Diagnostic::warning(at(2), "W001", "second", None));
        let out = bag.render("f.conf");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("f.conf:1:1: error[E001]: first"));
        assert!(lines[1].starts_with("f.conf:2:1: warning[W001]: second"));
        assert_eq!(DiagnosticBag::new().render("f.conf"), "");
    }

    #[test]
    fn severity_labels_and_ordering() {
        assert_eq!(Severity::Error.label(), "error");
        assert_eq!(Severity::Warning.label(), "warning");
        assert_eq!(Severity::Note.label(), "note");
        // Error must sort below warning: "first error" reporting relies on it.
        assert!(Severity::Error < Severity::Warning);
        assert!(Severity::Warning < Severity::Note);
    }

    #[test]
    fn only_error_is_fatal() {
        assert!(Severity::Error.is_fatal());
        assert!(!Severity::Warning.is_fatal());
        assert!(!Severity::Note.is_fatal());
    }

    #[test]
    fn a_hundred_warnings_do_not_fail_the_build() {
        let mut bag = DiagnosticBag::new();
        for i in 0..100 {
            bag.push(Diagnostic::warning(
                at(i + 1),
                "W002",
                "suspicious but legal",
                None,
            ));
        }
        assert_eq!(bag.len(), 100);
        assert_eq!(bag.warnings(), 100);
        assert_eq!(bag.errors(), 0);
        assert!(
            !bag.has_errors(),
            "warnings must never influence the exit code - this is the dinit-check bug"
        );
        assert!(bag.first_error().is_none());
    }

    #[test]
    fn notes_are_not_counted_as_warnings() {
        let mut bag = DiagnosticBag::new();
        bag.push(Diagnostic::note(at(1), "N001", "context", None));
        bag.push(Diagnostic::warning(at(2), "W001", "meh", None));
        assert_eq!(bag.warnings(), 1);
        assert_eq!(bag.len(), 2);
    }

    #[test]
    fn one_error_among_many_warnings_fails() {
        let mut bag = DiagnosticBag::new();
        bag.push(Diagnostic::warning(at(1), "W001", "meh", None));
        bag.push(Diagnostic::error(at(2), "E001", "broken", None));
        bag.push(Diagnostic::note(at(3), "N001", "context", None));
        assert!(bag.has_errors());
        assert_eq!(bag.errors(), 1);
        assert_eq!(bag.warnings(), 1);
    }

    #[test]
    fn insertion_order_is_preserved() {
        let mut bag = DiagnosticBag::new();
        // Inserted back-to-front: a bag that sorts would expose the bug.
        for line in [7u32, 3, 9, 1, 5] {
            bag.push(Diagnostic::error(at(line), "E001", "x", None));
        }
        let lines: Vec<u32> = bag.iter().filter_map(|d| d.span).map(|s| s.line).collect();
        assert_eq!(lines, alloc::vec![7, 3, 9, 1, 5]);
    }

    #[test]
    fn counts_and_emptiness() {
        let mut bag = DiagnosticBag::new();
        assert!(bag.is_empty());
        assert_eq!(bag.len(), 0);
        assert!(!bag.has_errors());
        bag.push(Diagnostic::warning(at(1), "W001", "meh", None));
        assert!(!bag.is_empty());
        assert_eq!(bag.len(), 1);
        assert_eq!(bag.into_vec().len(), 1);
    }

    #[test]
    fn first_error_is_the_first_fatal_one_not_the_first_item() {
        let mut bag = DiagnosticBag::new();
        bag.push(Diagnostic::note(at(1), "N001", "context", None));
        bag.push(Diagnostic::warning(at(2), "W001", "meh", None));
        bag.push(Diagnostic::error(at(3), "E001", "first problem", None));
        bag.push(Diagnostic::error(at(4), "E002", "second problem", None));
        assert_eq!(bag.first_error().map(|d| d.code), Some("E001"));
    }

    #[test]
    fn span_helpers() {
        let p = Span::point(4, 9);
        assert_eq!((p.line, p.column, p.len), (4, 9, 0));
        assert_eq!(p.end_column(), 9);
        let r = Span::range(4, 9, 3);
        assert_eq!(r.end_column(), 12);
        // Saturating: a span never wraps into a bogus column.
        assert_eq!(Span::range(1, 1, u32::MAX).end_column(), u32::MAX);
    }

    #[test]
    fn iter_and_into_iterator_agree() {
        let bag: DiagnosticBag = alloc::vec![
            Diagnostic::warning(at(1), "W001", "a", None),
            Diagnostic::error(at(2), "E001", "b", None),
        ]
        .into();
        let via_ref: Vec<&str> = bag.iter().map(|d| d.code).collect();
        let via_into: Vec<&str> = (&bag).into_iter().map(|d| d.code).collect();
        assert_eq!(via_ref, via_into);
        assert_eq!(bag.len(), 2);
    }

    #[test]
    fn display_of_severity_is_the_label() {
        assert_eq!(Severity::Warning.to_string(), "warning");
    }

    #[test]
    fn exit_code_is_false_for_every_single_non_error_diagnostic() {
        // The strongest form of the dinit-check rule, stated one diagnostic at a
        // time so it cannot be passed by a bag whose *totals* happen to work
        // out. Any one of these bags, printed in full, is a passing run.
        for d in [
            Diagnostic::warning(at(1), "W001", "suspicious", None),
            Diagnostic::note(at(1), "N001", "context", None),
            Diagnostic::warning(None, "W001", "filelevel warning", None),
        ] {
            let mut bag = DiagnosticBag::new();
            bag.push(d.clone());
            assert!(!bag.has_errors(), "{d} must not fail the build");
            assert_eq!(bag.errors(), 0);
            assert_eq!(bag.len(), 1, "it is still reported, just not fatal");
            assert!(bag.first_error().is_none());
            assert!(!d.is_error());
        }
    }

    #[test]
    fn a_warning_after_the_error_does_not_un_fail_the_run() {
        // A counter that gets decremented, or a bag whose `has_errors` is
        // recomputed by mutation order, would pass here and fail in a tool.
        let mut bag = DiagnosticBag::new();
        bag.push(Diagnostic::error(at(1), "E001", "broken", None));
        for i in 0..10 {
            bag.push(Diagnostic::warning(at(i + 2), "W001", "meh", None));
        }
        assert!(bag.has_errors());
        assert_eq!(bag.errors(), 1);
        assert_eq!(bag.warnings(), 10);
    }

    #[test]
    fn an_empty_bag_renders_to_nothing_and_passes() {
        let bag = DiagnosticBag::default();
        assert_eq!(bag, DiagnosticBag::new());
        assert!(bag.is_empty());
        assert!(!bag.has_errors());
        assert_eq!(bag.render("f.conf"), "");
        assert_eq!(bag.first_error(), None);
        assert_eq!(bag.into_vec().len(), 0);
    }

    #[test]
    fn located_carries_the_file_the_caller_supplies() {
        // A diagnostic does not store a path, and this is the type that does.
        let d = Diagnostic::error(at(7), "E001", "x", None);
        let loc = d.located("/etc/zinit/sshd.conf");
        assert_eq!(loc.file, "/etc/zinit/sshd.conf");
        assert_eq!(loc.diagnostic.code, "E001");
        assert!(
            loc.to_string()
                .starts_with("/etc/zinit/sshd.conf:7:1: error[E001]")
        );
    }

    #[test]
    fn a_bag_mixes_pointed_and_filelevel_diagnostics_in_one_render() {
        let mut bag = DiagnosticBag::new();
        bag.push(Diagnostic::error(at(3), "E001", "bad line", None));
        bag.push(Diagnostic::error(None, "E010", "duplicate name", None));
        let out = bag.render("f.conf");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(lines[0].starts_with("f.conf:3:1: error[E001]:"), "{out}");
        assert!(lines[1].starts_with("f.conf:-: error[E010]:"), "{out}");
        // Both are errors, so both count: "no position" is not "no severity".
        assert_eq!(bag.errors(), 2);
        assert!(bag.has_errors());
    }

    #[test]
    fn span_len_is_a_byte_length_not_a_character_count() {
        // Feeding a character count to a byte highlighter mis-highlights every
        // description with a comment in it, so the two must not be conflated.
        let s = Span::range(1, 1, 5);
        assert_eq!(s.end_column(), 6);
        // A 2-byte character advances the column by 2, which is what makes the
        // field a byte offset in the first place.
        let multibyte = Span::range(1, 1, 4);
        assert_eq!(multibyte.end_column(), 5);
        assert_eq!(Span::default(), Span::point(0, 0));
    }

    #[test]
    fn a_note_is_never_counted_as_a_warning_or_an_error() {
        let mut bag = DiagnosticBag::new();
        for i in 0..5u32 {
            bag.push(Diagnostic::note(at(i + 1), "N001", "context", None));
        }
        assert_eq!(bag.len(), 5);
        assert_eq!(bag.warnings(), 0);
        assert_eq!(bag.errors(), 0);
        assert!(!bag.has_errors());
    }

    #[test]
    fn codes_are_carried_through_untouched_by_rendering() {
        // `zcheck` output is grepped. The code has to survive Display, because
        // the rendered line is the only form most callers ever see.
        let d = Diagnostic::warning(at(2), "W017", "unused thing", Some("drop it".to_string()));
        let line = d.render("f.conf");
        assert!(
            line.starts_with("f.conf:2:1: warning[W017]: unused thing"),
            "{line}"
        );
        assert_eq!(d.code, "W017");
    }
}
