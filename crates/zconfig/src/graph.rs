//! Name resolution and structural validation — the linking pass.
//!
//! `&str` in, dense [`Idx`] out. This module owns every decision that can be
//! made *before* the graph is frozen, and nothing else: it never computes a
//! start order and never touches `zcore::Plan`. Freezing lives in
//! [`crate::plan`], and `Graph::to_plan` delegates there, so there is exactly
//! one implementation of the topological order.
//!
//! # What this pass exists to catch
//!
//! Every error below is one the operator can fix by editing one line of a
//! `.conf`. They are reported *at load time*, with a name, a file and a line,
//! because the alternative - discovering a bad graph after PID 1 has taken
//! over - means a machine that boots to nothing and says why only in dmesg.
//!
//! | Error | Why it must be fatal |
//! |---|---|
//! | [`GraphError::InvalidName`] | a name is a key everywhere: service names, log file paths, cgroup names. `/`, `..`, spaces and a leading `.`/`@` all let a description address something outside its own directory. This is the dinit `validate_service_name` `return true` bug. The *rules* live in [`crate::desc::validate_service_name`]; this module only decides that they are applied, and to which strings. |
//! | [`GraphError::InvalidDependencyName`] | the same rule, applied to the *other* end of an edge. `depends = ../../../etc/shadow` is the identical attack with a different entry point, and validating only the service's own name would leave it open. |
//! | [`GraphError::DuplicateName`] | two definitions, one key. Picking "the last one wins" hides a typo'd file. |
//! | [`GraphError::UnknownDependency`] | a required dependency that resolves to nothing means the machine silently boots without a service. |
//! | [`GraphError::SelfDependency`] | reported separately from [`GraphError::Cycle`] because `a depends on a` is a typo, not a design. |
//! | [`GraphError::Cycle`] | fatal, and the message carries the actual cycle. |
//! | [`GraphError::NoCommand`] | a `process`/`script`/`console` with no `command` cannot be spawned. A `target` legitimately has none. |
//! | [`GraphError::TooManyServices`] | every per-service allocation is sized from [`MAX_SERVICES`]; the cap is a bound on attacker-controlled input, not a style rule. |
//!
//! # Errors and findings are different types on purpose
//!
//! There is exactly one severity enum in this crate — [`crate::Severity`], in
//! `diagnostic.rs` — and it is the one that decides whether a run fails. A
//! *finding* here is a [`Diagnostic`] with a non-fatal severity, and
//! `finding` carries a `debug_assert` that says so: this module's promise is
//! that nothing it notices about a *survivable* problem can stop a boot, and an
//! assertion is the only way that promise survives a refactor.
//!
//! Two rules encode the rest of the policy:
//!
//! * an **unknown** dependency is an error if required, a warning if optional,
//!   because "wait for it if it is there" is what an optional edge means; but
//! * a **malformed** dependency *name* is an error either way. `opt:../../etc`
//!   is not a service that failed to be written down, it is a name that could
//!   never have been legal, and dropping it quietly would turn a mistake into
//!   a silent absence.
//!
//! # Ordering of indices
//!
//! Descriptions arrive in `readdir` order, which is filesystem-dependent and
//! changes between machines and between reboots. Indices are therefore assigned
//! by **byte-wise name order**, before any validation, so `a` always gets index
//! 0. Everything downstream of here (topological order, tie-breaking, tests,
//! the CLI's output) is then reproducible.
//!
//! Beware the two orders, they are not the same thing:
//!
//! * **indices are alphabetical** — a stable, meaningless key.
//! * **`Plan::order_up` is topological** — the only order the runtime walks.
//!
//! `graph.rs` deliberately knows nothing about the second one.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use zcore::{Idx, MAX_SERVICES, ServiceKind};

use crate::desc::{NameError, ServiceDesc, validate_service_name};
use crate::diagnostic::{Diagnostic, Severity, Span};

/// Maximum edit distance at which a "did you mean" suggestion is offered.
///
/// Typo distance is small (1–3). A larger threshold produces noise: at 5
/// edits "netowrk" and "boot" are equally "close", and a wrong suggestion is
/// worse than none, because the operator stops reading the rest of the message.
const SUGGEST_BUDGET: usize = 3;

/// Maximum relative length difference considered for a suggestion.
///
/// Pre-filter only: `levenshtein("a", "abcdefghij") == 9` is already excluded by
/// [`SUGGEST_BUDGET`], so this does not change results, it just avoids filling
/// two rows of the DP matrix for names that cannot possibly match.
const SUGGEST_LENGTH_SLACK: usize = 2;

/// Everything that can be structurally wrong with a set of descriptions.
///
/// Not `Clone`: a `GraphError` is produced once, at load, and immediately
/// rendered for the operator. Cloning one would be pure ceremony.
///
/// Every variant names **two** things where two are involved: the service the
/// problem belongs to, and the offending value. dinit's `validate_dep_name_x`
/// shadows `name` and reports the *dependency's* name as if it were the
/// service's, so the error points at a file that is perfectly fine; that shape
/// is not expressible here, because there is no single `name` field to shadow.
#[derive(Debug, PartialEq, Eq)]
pub enum GraphError {
    /// A name that could address something outside `services.d/`.
    InvalidName {
        /// The offending name.
        name: String,
        /// Why it was rejected. The crate's canonical verdict, carried rather
        /// than re-derived, so the graph cannot describe a rejection in words
        /// the validator would not agree with.
        reason: NameError,
        /// File the name came from, when the description carries one.
        source: String,
        /// Line within that file, when known.
        line: u32,
    },
    /// A dependency name that is not a legal service name.
    ///
    /// A separate variant from [`GraphError::InvalidName`] on purpose: the
    /// operator has to be told *which* name was rejected, and folding it into
    /// `InvalidName` would be exactly the variable-shadowing mistake described
    /// on the enum.
    InvalidDependencyName {
        /// The service that declares the edge. Never the dependency's name.
        service: String,
        /// The dependency name that was rejected.
        dep: String,
        /// Why it was rejected, from the same validator and therefore the same
        /// vocabulary as [`GraphError::InvalidName::reason`].
        reason: NameError,
        /// File the declaration came from.
        source: String,
        /// Line within that file, when known.
        line: u32,
    },
    /// Two descriptions claim the same name.
    DuplicateName {
        /// The name claimed twice.
        name: String,
        /// File of the definition that was accepted.
        first: String,
        /// File of the definition that was rejected.
        second: String,
    },
    /// A required dependency names a service that does not exist.
    ///
    /// An unknown *optional* edge is never this error: it is a `W100` finding,
    /// so a `true` on "the graph is being abandoned" is implied by the variant
    /// itself and there is no flag to get wrong.
    UnknownDependency {
        /// The service that carries the dependency.
        service: String,
        /// The name it depends on, which resolves to nothing.
        dep: String,
        /// Closest real service name, if one is near enough.
        suggestion: Option<String>,
    },
    /// A service depends on itself.
    ///
    /// Distinct from [`GraphError::Cycle`] on purpose: `a depends on a` is
    /// always a typo, and telling an operator "cycle: a -> a" about a single
    /// service is technically true and practically useless. Fatal for an
    /// *optional* self-edge too, for the same reason it is fatal when required.
    SelfDependency {
        /// The service that depends on itself.
        service: String,
        /// File the declaration came from.
        source: String,
        /// Line within that file, when known.
        line: u32,
    },
    /// A required dependency cycle. Always fatal: there is no correct order.
    Cycle {
        /// The actual cycle, first element repeated last: `["a", "b", "a"]`.
        ///
        /// The walk that produced it is deterministic, so this is a stable
        /// message, not one of a set.
        path: Vec<String>,
    },
    /// More services than [`MAX_SERVICES`].
    TooManyServices {
        /// How many descriptions were supplied.
        count: usize,
    },
    /// A service that forks has no `command` to fork.
    /// `user =` named an account that has not been resolved to a uid yet.
    ///
    /// The runtime (`zrt`) looks the name up in `/etc/passwd` and builds the
    /// plan again. Freezing a plan while the identity is still a string would
    /// mean every consumer downstream reads `ServicePlan::run_as` as `None` —
    /// which is indistinguishable from "run as root", and is the one outcome
    /// this whole mechanism exists to prevent.
    UnresolvedIdentity { service: String, name: String },
    NoCommand {
        /// The service with nothing to run.
        service: String,
        /// Its `type`, which is why it is required to have a command.
        kind: ServiceKind,
        /// File it came from.
        source: String,
        /// Line within that file, when known.
        line: u32,
    },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::UnresolvedIdentity { service, name } => write!(
                f,
                "`{service}` runs as `{name}`, which has not been resolved to a numeric id"
            ),
            GraphError::InvalidName {
                name,
                reason,
                source,
                line,
            } => {
                write!(
                    f,
                    "invalid service name {name:?}: {reason} ({source}:{line})"
                )
            }
            GraphError::InvalidDependencyName {
                service,
                dep,
                reason,
                source,
                line,
            } => write!(
                f,
                "{service}: invalid dependency name {dep:?}: {reason} ({source}:{line})"
            ),
            GraphError::DuplicateName {
                name,
                first,
                second,
            } => write!(
                f,
                "duplicate service {name:?}: defined in both {first} and {second}"
            ),
            GraphError::UnknownDependency {
                service,
                dep,
                suggestion,
            } => {
                write!(f, "{service}: unknown required dependency {dep:?}")?;
                if let Some(s) = suggestion {
                    write!(f, "; did you mean {s:?}?")?;
                }
                Ok(())
            }
            GraphError::SelfDependency {
                service,
                source,
                line,
            } => {
                write!(f, "{service}: service depends on itself ({source}:{line})")
            }
            GraphError::Cycle { path } => write!(f, "dependency cycle: {}", render_cycle(path)),
            GraphError::TooManyServices { count } => write!(
                f,
                "too many services: {count} exceeds the maximum of {MAX_SERVICES}"
            ),
            GraphError::NoCommand {
                service,
                kind,
                source,
                line,
            } => write!(
                f,
                "{service}: type = {} requires a `command` ({source}:{line})",
                kind.name()
            ),
        }
    }
}

impl GraphError {
    /// Where in the description this error was found, when that is known.
    ///
    /// `None` for the errors that are about the *set* rather than about a line
    /// (a duplicate name, the service cap), because pointing at a column there
    /// would be a lie.
    ///
    /// `source_line == 0` means the caller never supplied a line, and is
    /// reported the same way.
    pub fn span(&self) -> Option<Span> {
        let line = match self {
            GraphError::UnresolvedIdentity { .. } => 0,
            GraphError::InvalidName { line, .. }
            | GraphError::InvalidDependencyName { line, .. }
            | GraphError::SelfDependency { line, .. }
            | GraphError::NoCommand { line, .. } => *line,
            GraphError::DuplicateName { .. }
            | GraphError::UnknownDependency { .. }
            | GraphError::Cycle { .. }
            | GraphError::TooManyServices { .. } => return None,
        };
        (line != 0).then(|| Span::point(line, 1))
    }
}

/// Join a cycle into `a -> b -> a`.
fn render_cycle(path: &[String]) -> String {
    let mut out = String::new();
    for (i, node) in path.iter().enumerate() {
        if i > 0 {
            out.push_str(" -> ");
        }
        out.push_str(node);
    }
    out
}

/// Build one non-fatal finding about `desc`.
///
/// The single funnel every non-fatal finding in this crate goes through, from
/// both `graph.rs` and `plan.rs`, for two reasons.
///
/// First, `Severity` is taken as a parameter rather than hard-coded at each
/// call site, so the *choice* of level is visible at the call and the *shape*
/// of the diagnostic is decided once. A second copy of this function - one per
/// module - is two places to forget the assertion below, which is the only
/// thing mechanically enforcing the rule.
///
/// Second, the assertion turns "a finding must never stop a boot" from a
/// convention into something a refactor cannot quietly break: promoting one of
/// these to an error is a one-word change, and this is where the reviewer
/// looks. It is a `debug_assert` and not a runtime check because the two doors
/// that can reach it are a test and a load-time validation, and neither is on
/// the supervisor's hot path.
pub(crate) fn finding(
    severity: Severity,
    desc: &ServiceDesc,
    code: &'static str,
    message: String,
    help: Option<String>,
) -> Diagnostic {
    debug_assert!(
        !severity.is_fatal(),
        "a finding must not be an error: only `Severity::Error` stops a run, and \
         this module's promise is that nothing here can stop a boot"
    );
    Diagnostic {
        severity,
        // `source_line == 0` means "the caller never told us", which is the
        // same as "there is no honest position to report".
        span: (desc.source_line != 0).then(|| Span::point(desc.source_line, 1)),
        code,
        message,
        help,
    }
}

/// Which of the two edge classes a resolution pass is walking.
///
/// Private, and local on purpose. `ServiceDesc` carries two `Vec<String>` and
/// deliberately no enum over them, so this is a parameter of one private
/// function rather than a type anyone outside this module can depend on - and
/// therefore nothing that can disagree with `desc.rs` about what "optional"
/// means. Making it public would be the first step towards the two-word
/// duplication the whole `desc`/`graph` split exists to prevent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    /// Gates startup. An unresolved edge here is fatal.
    Required,
    /// Waited for, but its absence is tolerated. An unresolved edge here is a
    /// finding and the edge is dropped.
    Optional,
}

/// Deduplicated edges, in index order.
///
/// Stored as a CSR payload (`offsets[i]..offsets[i + 1]`) rather than a
/// `Vec<Vec<Idx>>` because a 4096-node dense graph with duplicates collapsed is
/// 16M edges, and a dense graph is exactly the case where the per-node
/// allocation overhead matters most.
#[derive(Clone, Debug)]
struct Edges {
    /// `n + 1` offsets into `to`; edge *e* of node *i* is `to[e]`.
    offsets: Vec<usize>,
    /// Adjacency, grouped by node.
    to: Vec<Idx>,
}

impl Edges {
    /// Edges of node `i`, as a slice.
    ///
    /// # Panics
    /// If `i` is at or past `offsets.len() - 1`, i.e. the caller passed a
    /// foreign index. Every index in the plan comes from this graph, so that
    /// is a bug rather than a runtime condition.
    fn of(&self, i: Idx) -> &[Idx] {
        &self.to[self.offsets[i]..self.offsets[i + 1]]
    }
}

/// The resolved dependency graph.
///
/// Owns the descriptions, so a `ServiceDesc` reference stays valid for as long
/// as the graph does — which is what lets [`Graph::desc`] hand out `&` without
/// copying strings at every call site.
///
/// `Clone` and `Debug` are derived, never `PartialEq`: equality is a property
/// of the *frozen plan* (`zcore::Plan` derives it), and giving a builder two
/// notions of "equal" is how a test ends up asserting the wrong one.
#[derive(Clone, Debug)]
pub struct Graph {
    /// Descriptions sorted by name, densely indexed.
    descs: Vec<ServiceDesc>,
    /// Aligned with `descs`: index of the description with that name.
    by_name: Vec<(String, Idx)>,
    /// Required edges, CSR, each row sorted ascending and deduplicated.
    required: Edges,
    /// Optional edges, CSR, built the same way.
    ///
    /// Kept beside `required` rather than merged into it because "which edges
    /// gate my start" and "which edges do I wait for but survive" are different
    /// questions, and merging them would make the answer unrecoverable.
    optional: Edges,
}

impl Graph {
    /// Build and validate a graph from a set of descriptions.
    ///
    /// See [`Graph::build_with_findings`] for the full list of checks. This form
    /// drops the findings: a supervisor that cannot show a message to anybody
    /// has nothing to do with them, and dropping them here is what guarantees
    /// an unknown *optional* dependency can never stop a boot.
    pub fn build(descs: &[ServiceDesc]) -> core::result::Result<Graph, GraphError> {
        Graph::build_with_findings(descs).map(|(g, _)| g)
    }

    /// Build, validate, and return the non-fatal findings alongside the graph.
    ///
    /// The graph itself does not store findings: there is exactly one way to
    /// obtain them, so no caller can read a half-populated list.
    ///
    /// Order of operations, and why:
    ///
    /// 1. **Count.** Over the cap before allocating anything sized from the
    ///    input, so a 10^9-entry description costs one comparison and no index
    ///    is ever computed past the end of anything.
    /// 2. **Sort by name.** Assigning indices by `readdir` order would make
    ///    every downstream artefact - the plan, the tie-break, a test
    ///    expectation - depend on the filesystem.
    /// 3. **Resolve names.** Deduplicating during resolution rather than
    ///    detecting duplicates afterwards means "a depends on a, a" is accepted
    ///    *silently*, which is correct: it is one edge, not two.
    pub fn build_with_findings(
        descs: &[ServiceDesc],
    ) -> core::result::Result<(Graph, Vec<Diagnostic>), GraphError> {
        if descs.len() > MAX_SERVICES {
            return Err(GraphError::TooManyServices { count: descs.len() });
        }

        let mut sorted: Vec<&ServiceDesc> = descs.iter().collect();
        sorted.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        // A stable tie-break on the *source* keeps the "which definition won"
        // message stable when two files declare the same name. Both keys
        // together are a total order, so the sort is deterministic whatever the
        // readdir handed us.
        sorted.sort_by(|a, b| a.source.cmp(&b.source));

        let n = sorted.len();
        let mut owned: Vec<ServiceDesc> = Vec::with_capacity(n);
        for &d in &sorted {
            if let Err(reason) = validate_service_name(&d.name) {
                return Err(GraphError::InvalidName {
                    name: d.name.clone(),
                    reason,
                    source: d.source.clone(),
                    line: d.source_line,
                });
            }
            // `owned` is the accepted prefix, sorted by name, so the only
            // possible duplicate is the entry right before this one.
            if let Some(prev) = owned.last()
                && prev.name == d.name
            {
                return Err(GraphError::DuplicateName {
                    name: d.name.clone(),
                    first: prev.source.clone(),
                    second: d.source.clone(),
                });
            }
            // Rule 6 of the review: anything that forks needs something to
            // fork, and `has_process()` is false for exactly one kind, so this
            // is "kind != Target" without hard-coding the enum. A
            // whitespace-only command is as useless as none - `execve("  ")` is
            // a valid syscall and a guaranteed failure - and the parser is the
            // other half of this check, so this is the belt to its braces.
            if d.kind.has_process() && d.command.trim().is_empty() {
                return Err(GraphError::NoCommand {
                    service: d.name.clone(),
                    kind: d.kind,
                    source: d.source.clone(),
                    line: d.source_line,
                });
            }
            owned.push(d.clone());
        }

        let by_name: Vec<(String, Idx)> = owned
            .iter()
            .enumerate()
            .map(|(i, d)| (d.name.clone(), i))
            .collect();

        let mut findings: Vec<Diagnostic> = Vec::new();
        let required = resolve(&owned, &by_name, Class::Required, &mut findings)?;
        let optional = resolve(&owned, &by_name, Class::Optional, &mut findings)?;
        report_double_qualified_edges(&owned, &required, &optional, &mut findings);

        let graph = Graph {
            descs: owned,
            by_name,
            required,
            optional,
        };
        debug_assert_eq!(graph.required.offsets.len(), graph.optional.offsets.len());
        Ok((graph, findings))
    }

    /// Number of services.
    pub fn len(&self) -> usize {
        self.descs.len()
    }

    /// True when the graph holds no services.
    pub fn is_empty(&self) -> bool {
        self.descs.is_empty()
    }

    /// Resolve a service name to its dense index.
    ///
    /// Binary search, because the key vector is sorted by construction; the
    /// supervisor calls this once per name at load and the CLI once per
    /// invocation. O(n) would also be defensible, and is not worth a hash map
    /// in a crate whose only dependency is `zcore`.
    pub fn index_of(&self, name: &str) -> Option<Idx> {
        lookup(&self.by_name, name)
    }

    /// The description behind an index.
    ///
    /// Panics on an out-of-range index, like every slice index: an index out of
    /// range here means a bug in the caller, and `no_std` has no way to log one.
    pub fn desc(&self, idx: Idx) -> &ServiceDesc {
        &self.descs[idx]
    }

    /// All descriptions, in index order (alphabetical).
    pub fn descs(&self) -> &[ServiceDesc] {
        &self.descs
    }

    /// Required edges of a service, deduplicated, ascending.
    pub fn required_of(&self, idx: Idx) -> &[Idx] {
        self.required.of(idx)
    }

    /// Optional edges of a service, deduplicated, ascending.
    pub fn optional_of(&self, idx: Idx) -> &[Idx] {
        self.optional.of(idx)
    }

    /// Every service that declares `dep` as a required dependency.
    ///
    /// Linear per query, which is fine because it is a load-time diagnostic
    /// helper. The topological sort builds its own CSR reverse index rather
    /// than calling this in a loop, because `V` calls to an `O(V)` function is
    /// the one quadratic that a 4096-service boot would actually feel.
    pub fn dependents_of(&self, dep: Idx) -> Vec<Idx> {
        let mut out: Vec<Idx> = (0..self.len())
            .filter(|&i| self.required.of(i).contains(&dep))
            .collect();
        out.sort_unstable();
        out
    }

    /// Freeze this graph into a topologically ordered `zcore::Plan`.
    ///
    /// A delegation, not a second implementation: [`crate::plan::build_plan`]
    /// is the only place that computes an order or fills a `ServicePlan`, and
    /// the descriptions this graph owns are exactly what it takes.
    ///
    /// It therefore re-runs validation. That costs one more `O(n log n)` pass
    /// at load and buys two things worth more: there is no second code path
    /// that can disagree with the supervisor's, and **there is no panic**. A
    /// cycle is an `Err` here, not a `panic!` — this crate is built
    /// `panic = "abort"` because the supervisor is a child of PID 1, where a
    /// panic takes the machine with it.
    pub fn to_plan(&self) -> core::result::Result<zcore::Plan, GraphError> {
        crate::plan::build_plan(&self.descs)
    }
}

/// Binary search a sorted `(name, idx)` table.
fn lookup(by_name: &[(String, Idx)], name: &str) -> Option<Idx> {
    by_name
        .binary_search_by(|probe| probe.0.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|pos| by_name[pos].1)
}

/// Resolve every name in `descs[i]` into a dense index, returning CSR edges.
///
/// One pass over the sorted description vector per dependency class, so the
/// staging buffer is allocated once and reused for all nodes instead of once
/// per node.
fn resolve(
    descs: &[ServiceDesc],
    by_name: &[(String, Idx)],
    class: Class,
    findings: &mut Vec<Diagnostic>,
) -> core::result::Result<Edges, GraphError> {
    let n = descs.len();
    let optional = class == Class::Optional;

    // `offsets` has `n + 1` entries: the leading 0 makes `of(0)` well defined
    // even when service 0 has no edges at all.
    let mut offsets: Vec<usize> = Vec::with_capacity(n + 1);
    let mut to: Vec<Idx> = Vec::new();
    let mut row: Vec<Idx> = Vec::new();
    offsets.push(0);

    for (i, d) in descs.iter().enumerate() {
        let names: &Vec<String> = if optional {
            &d.depends_optional
        } else {
            &d.depends_required
        };
        row.clear();
        for dep in names {
            // The *name* is checked, not just the lookup. This is the second
            // half of the dinit bug: an unvalidated dependency name is the same
            // path traversal as an unvalidated service name, reached through a
            // different door. It is fatal for an optional edge too - `opt:../..`
            // is not a service that is absent, it is a name that was never
            // legal, and warning about it would teach the operator to ignore
            // warnings.
            if let Err(reason) = validate_service_name(dep) {
                return Err(GraphError::InvalidDependencyName {
                    service: d.name.clone(),
                    dep: dep.clone(),
                    reason,
                    source: d.source.clone(),
                    line: d.source_line,
                });
            }

            match lookup(by_name, dep) {
                Some(target) if target == i => {
                    return Err(GraphError::SelfDependency {
                        service: d.name.clone(),
                        source: d.source.clone(),
                        line: d.source_line,
                    });
                }
                Some(target) => row.push(target),
                None if optional => {
                    // Optional and unresolvable: tolerable by design, dropped
                    // from the plan, reported to the operator.
                    let suggestion = suggest(dep, by_name);
                    let message = match &suggestion {
                        Some(s) => format!(
                            "{}: unknown optional dependency {dep:?}, ignored; did you mean {s:?}?",
                            d.name
                        ),
                        None => format!("{}: unknown optional dependency {dep:?}, ignored", d.name),
                    };
                    let help = Some(
                        "declare the dependency without the `opt:` prefix, or remove \
                         it if it is genuinely optional"
                            .to_string(),
                    );
                    findings.push(finding(Severity::Warning, d, "W100", message, help));
                }
                None => {
                    // Required and unresolvable: the boot would silently lose a
                    // service, so this is fatal.
                    return Err(GraphError::UnknownDependency {
                        service: d.name.clone(),
                        dep: dep.clone(),
                        suggestion: suggest(dep, by_name),
                    });
                }
            }
        }

        // Ascending + dedup makes the CSR canonical, which is what makes the
        // tie-break in the topological order reproducible, lets the
        // double-qualification check below be a linear merge, and lets the
        // tests compare whole `ServicePlan`s for equality.
        row.sort_unstable();
        row.dedup();
        to.extend_from_slice(&row);
        offsets.push(to.len());
    }

    Ok(Edges { offsets, to })
}

/// `W101`: a name declared both required and optional is two instructions for
/// one edge, and only one of them can be obeyed.
///
/// Both rows are sorted and deduplicated, so this is a merge join rather than
/// a nested scan, and it runs once per service over its own edges.
///
/// The rule applied is that the **required** edge wins: an operator who wrote
/// the name twice clearly wanted the service to wait for it, and silently
/// downgrading that to "optional" is the failure mode where a typo turns into
/// an outage that only shows up under load.
fn report_double_qualified_edges(
    descs: &[ServiceDesc],
    required: &Edges,
    optional: &Edges,
    findings: &mut Vec<Diagnostic>,
) {
    for (i, d) in descs.iter().enumerate() {
        let req = required.of(i);
        let opt = optional.of(i);
        let (mut a, mut b) = (0usize, 0usize);
        while a < req.len() && b < opt.len() {
            match req[a].cmp(&opt[b]) {
                core::cmp::Ordering::Less => a += 1,
                core::cmp::Ordering::Greater => b += 1,
                core::cmp::Ordering::Equal => {
                    // Two distinct names, both spelled out: the service that
                    // made the mistake and the dependency it is about.
                    let message = format!(
                        "{}: dependency `{}` is declared both required and optional; \
                         treating it as required",
                        d.name, descs[req[a]].name
                    );
                    findings.push(finding(
                        Severity::Warning,
                        d,
                        "W101",
                        message,
                        Some("declare the dependency once, with or without `opt:`".to_string()),
                    ));
                    a += 1;
                    b += 1;
                }
            }
        }
    }
}

/// Edit distance between two byte strings (Levenshtein, unit costs).
///
/// Two rolling rows instead of a full `n * m` matrix: the graph builder calls
/// this once per unresolved dependency, and a description with a typo in every
/// service would otherwise allocate a matrix per call. Iterative, so a long
/// chain of near-identical names cannot blow the stack.
fn levenshtein(a: &[u8], b: &[u8]) -> usize {
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur: Vec<usize> = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        // Deleting `ca` from `a` costs i + 1 to align against the empty prefix.
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            let del = prev[j + 1] + 1;
            let ins = cur[j] + 1;
            cur[j + 1] = sub.min(del).min(ins);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Closest known service name to `name`, if one is within [`SUGGEST_BUDGET`].
///
/// Ties break towards the lexicographically smaller name so the message is
/// stable across filesystems and runs. Candidates are considered in that same
/// order, so the first strict improvement wins and equal distances keep the
/// earlier (smaller) name.
fn suggest(name: &str, by_name: &[(String, Idx)]) -> Option<String> {
    let needle = name.as_bytes();
    let mut best: Option<(usize, &str)> = None;
    for (candidate, _) in by_name {
        let bytes = candidate.as_bytes();
        let len_gap = if bytes.len() > needle.len() {
            bytes.len() - needle.len()
        } else {
            needle.len() - bytes.len()
        };
        if len_gap > SUGGEST_LENGTH_SLACK {
            continue;
        }
        let d = levenshtein(needle, bytes);
        if d > SUGGEST_BUDGET {
            continue;
        }
        best = match best {
            Some((bd, _)) if bd <= d => best,
            _ => Some((d, candidate.as_str())),
        };
    }
    best.map(|(_, s)| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::MAX_SERVICE_NAME_BYTES;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A minimal, valid description: a process service with a command and no
    /// dependencies, which is the degenerate graph the linker should accept.
    fn desc(name: &str) -> ServiceDesc {
        let mut d = ServiceDesc::new(String::from(name));
        d.command = String::from("/usr/bin/true");
        d.source = format!("/etc/zinit/services.d/{name}.conf");
        d.source_line = 1;
        d
    }

    fn desc_with(name: &str, kind: ServiceKind, deps: &[&str], opt: &[&str]) -> ServiceDesc {
        let mut d = desc(name);
        d.kind = kind;
        d.command = if kind.has_process() {
            String::from("/usr/bin/true")
        } else {
            String::new()
        };
        d.depends_required = deps.iter().map(|s| String::from(*s)).collect();
        d.depends_optional = opt.iter().map(|s| String::from(*s)).collect();
        d
    }

    fn names(g: &Graph) -> Vec<&str> {
        g.descs().iter().map(|d| d.name.as_str()).collect()
    }

    fn edge_names(g: &Graph, idx: Idx) -> Vec<&str> {
        g.required_of(idx)
            .iter()
            .map(|&t| g.desc(t).name.as_str())
            .collect()
    }

    /// Every finding's code, for asserting *which* check fired.
    fn codes(findings: &[Diagnostic]) -> Vec<&'static str> {
        findings.iter().map(|d| d.code).collect()
    }

    // ── names ───────────────────────────────────────────────────────────────

    #[test]
    fn indices_are_alphabetical_regardless_of_input_order() {
        let descs = vec![desc("zebra"), desc("alpha"), desc("mid")];
        let g = Graph::build(&descs).expect("valid graph");
        assert_eq!(names(&g), ["alpha", "mid", "zebra"]);
        assert_eq!(g.index_of("alpha"), Some(0));
        assert_eq!(g.index_of("zebra"), Some(2));
        assert_eq!(g.index_of("nope"), None);
    }

    #[test]
    fn duplicate_names_are_rejected_with_both_files() {
        let mut a = desc("sshd");
        let mut b = desc("sshd");
        a.source = "/etc/zinit/services.d/sshd.conf".into();
        b.source = "/etc/zinit/zinit.d/sshd.conf".into();
        match Graph::build(&[a, b]) {
            Err(GraphError::DuplicateName {
                name,
                first,
                second,
            }) => {
                assert_eq!(name, "sshd");
                assert_eq!(first, "/etc/zinit/services.d/sshd.conf");
                assert_eq!(second, "/etc/zinit/zinit.d/sshd.conf");
            }
            other => panic!("expected DuplicateName, got {other:?}"),
        }
    }

    /// The dinit `validate_service_name` bug, exhaustively.
    ///
    /// A `return true` placed before the traversal checks let every one of these
    /// through, and each became a real log path or cgroup name. The list is the
    /// regression, not an example: a future relaxation of the rules has to come
    /// with a line removed from here, which is the point.
    ///
    /// The rules themselves live in [`validate_service_name`], one function,
    /// one owner. This list only pins that the graph *uses* that function for
    /// both ends of an edge - see [`a_dependency_name_is_validated_too`] - and
    /// not what the rules are.
    const TRAVERSAL_NAMES: &[&str] = &[
        "",
        ".",
        "..",
        "../escape",
        "../../etc/shadow",
        "a/b",
        "a\\b",
        ".hidden",
        "@templ",
        "a b",
        "a\tb",
        "a\nb",
        "a\u{1}b",
        "a\0b",
        " leading",
        "trailing ",
        // An interior `..` is refused too, not only a leading one: the leading
        // rule already covers `.` and `..`, so this is the case that would
        // otherwise slip through and still read like a path fragment.
        "a..b",
        "a..",
    ];

    #[test]
    fn a_name_that_is_a_path_is_rejected() {
        for bad in TRAVERSAL_NAMES {
            assert!(
                validate_service_name(bad).is_err(),
                "{bad:?} must not be a valid service name"
            );
            let mut d = desc("ok");
            d.name = String::from(*bad);
            match Graph::build(&[d]) {
                Err(GraphError::InvalidName { name, reason, .. }) => {
                    assert_eq!(name, *bad);
                    // The reason is carried, not re-worded, so the graph cannot
                    // describe a rejection in words the validator would not
                    // agree with.
                    assert_eq!(reason, validate_service_name(bad).unwrap_err());
                }
                other => panic!("{bad:?} must be rejected by the graph builder, got {other:?}"),
            }
        }
    }

    /// `=`, `,` and `:` are legal inside a service name, and this is why that is
    /// safe rather than an oversight.
    ///
    /// A service name is derived from a *file name* by the caller, never from
    /// the right-hand side of a directive, so there is no grammar for it to
    /// escape from: `a=b` is a file called `a=b`, and nothing re-reads it as
    /// `a` assigned the value `b`. Rejecting them anyway would forbid a
    /// legitimate file name to defend against an attack that has no path here.
    ///
    /// Pinned as a test so that adding them to the rules is a deliberate,
    /// visible change rather than a drive-by edit in `desc.rs`.
    #[test]
    fn names_with_directive_punctuation_are_legal_on_purpose() {
        for ok in ["a=b", "a,b", "a:b", "sshd.conf", "50%.cache"] {
            assert!(
                validate_service_name(ok).is_ok(),
                "{ok:?} is a legal file name and must be accepted"
            );
            let mut d = desc("x");
            d.name = String::from(ok);
            assert!(Graph::build(&[d]).is_ok(), "{ok:?} must reach the plan");
        }
    }

    /// The whole point of validating a *dependency* name too.
    ///
    /// `../escape` as a service name is a typo; `depends = ../../etc/shadow` is
    /// the same traversal through a different door, and a validator that only
    /// checked `ServiceDesc::name` would let it through.
    #[test]
    fn dependency_names_are_validated_too() {
        for bad in TRAVERSAL_NAMES {
            let mut d = desc_with("app", ServiceKind::Process, &[bad], &[]);
            d.source_line = 42;
            match Graph::build(&[d]) {
                Err(GraphError::InvalidDependencyName {
                    service,
                    dep,
                    reason,
                    line,
                    ..
                }) => {
                    assert_eq!(service, "app", "the error must name the service");
                    assert_eq!(dep, *bad, "the error must name the dependency too");
                    assert_eq!(line, 42);
                    // Same validator, same verdict, on both ends of the edge.
                    assert_eq!(reason, validate_service_name(bad).unwrap_err());
                }
                other => {
                    panic!("required dep {bad:?}: expected InvalidDependencyName, got {other:?}")
                }
            }
            // And the optional door, which must be just as closed: dropping
            // `opt:../../etc` with a warning would teach operators to ignore
            // warnings.
            let d = desc_with("app", ServiceKind::Process, &[], &[bad]);
            assert!(
                matches!(
                    Graph::build(&[d]),
                    Err(GraphError::InvalidDependencyName { .. })
                ),
                "optional dep {bad:?} must be rejected just as hard"
            );
        }
    }

    /// dinit's linter shadows `name` and reports the *dependency's* name as if
    /// it were the service's, so the reader goes looking in a file that is
    /// fine. Here the two names are separate fields of separate variants, and
    /// this pins the rendered text so the distinction survives an edit.
    #[test]
    fn a_rejected_dependency_is_reported_against_the_right_name() {
        let d = desc_with("sshd", ServiceKind::Process, &["../escape"], &[]);
        let err = Graph::build(&[d]).expect_err("must be rejected");
        let text = err.to_string();
        assert!(text.contains("sshd:"), "must name the service: {text}");
        assert!(
            text.contains("\"../escape\""),
            "must name the dependency: {text}"
        );
        // The dependency name is never presented as the thing to go and fix.
        assert!(!text.starts_with("invalid service name \"../escape\""));
    }

    /// The graph's safety rests on the *first* character being checked too, and
    /// it did not always.
    ///
    /// An earlier `validate_service_name` pulled the first character out to test
    /// it against `.` and `@`, and its character loop therefore started at the
    /// second one. `"/"`, `" leading"` and `"\\absolute"` all sailed through,
    /// and a service name that is a path is precisely the traversal the function
    /// exists to prevent. The graph caught it and carried a four-line shim until
    /// the rule was fixed where it belongs; the shim is gone, and this test is
    /// what took its place.
    ///
    /// Each of these is a *one-character* or *leading-whitespace* name, which is
    /// the shape the old bug had. A regression reintroduces it silently, because
    /// every other test in this file uses names long enough to trip the second
    /// character instead.
    #[test]
    fn the_first_character_is_validated_like_every_other() {
        for bad in [
            "/",
            "//",
            "/etc",
            "\\",
            "\\absolute",
            " leading",
            "\tleading",
            "\u{1}x",
        ] {
            assert!(
                validate_service_name(bad).is_err(),
                "{bad:?} must be rejected: the first character is not special"
            );
            let mut d = desc("ok");
            d.name = String::from(bad);
            assert!(
                matches!(Graph::build(&[d]), Err(GraphError::InvalidName { .. })),
                "{bad:?} must not reach a plan"
            );
        }
    }

    #[test]
    fn ordinary_names_are_accepted() {
        for good in [
            "a",
            "sshd",
            "db-postgres",
            "x.target",
            "db_postgres",
            "s0",
            "ñandú",
        ] {
            assert!(
                validate_service_name(good).is_ok(),
                "{good:?} should be a valid name"
            );
        }
    }

    #[test]
    fn a_name_at_the_length_limit_is_accepted_and_one_byte_over_is_not() {
        let ok = "a".repeat(MAX_SERVICE_NAME_BYTES);
        let too_long = "a".repeat(MAX_SERVICE_NAME_BYTES + 1);
        assert!(validate_service_name(&ok).is_ok(), "the cap is inclusive");
        assert!(matches!(
            validate_service_name(&too_long).unwrap_err(),
            NameError::TooLong { .. }
        ));
        // Measured in bytes, not characters: 255 two-byte characters is a
        // 510-byte name and must not sneak through.
        let wide = "é".repeat(MAX_SERVICE_NAME_BYTES);
        assert!(
            validate_service_name(&wide).is_err(),
            "the limit is bytes, not chars"
        );
        let mut d = desc("ok");
        d.name = too_long;
        assert!(matches!(
            Graph::build(&[d]),
            Err(GraphError::InvalidName { .. })
        ));
    }

    // ── resolution ──────────────────────────────────────────────────────────

    #[test]
    fn names_resolve_to_indices_in_both_directions() {
        let g = Graph::build(&[
            desc_with("base", ServiceKind::Target, &[], &[]),
            desc_with("app", ServiceKind::Process, &["base"], &["cache"]),
            desc_with("cache", ServiceKind::Process, &[], &[]),
        ])
        .expect("valid graph");
        let app = g.index_of("app").unwrap();
        assert_eq!(g.required_of(app), &[g.index_of("base").unwrap()]);
        assert_eq!(g.optional_of(app), &[g.index_of("cache").unwrap()]);
        assert_eq!(g.dependents_of(g.index_of("base").unwrap()), vec![app]);
        assert_eq!(g.desc(app).name, "app");
    }

    #[test]
    fn a_repeated_dependency_is_one_edge() {
        let g = Graph::build(&[
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with("b", ServiceKind::Process, &["a", "a", "a"], &["a", "a"]),
        ])
        .expect("valid graph");
        let b = g.index_of("b").unwrap();
        assert_eq!(g.required_of(b), &[0]);
        assert_eq!(g.optional_of(b), &[0]);
    }

    #[test]
    fn edges_come_out_sorted() {
        let g = Graph::build(&[
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with("b", ServiceKind::Process, &[], &[]),
            desc_with("c", ServiceKind::Process, &[], &[]),
            desc_with("d", ServiceKind::Process, &["c", "a", "b"], &[]),
        ])
        .expect("valid graph");
        assert_eq!(edge_names(&g, g.index_of("d").unwrap()), ["a", "b", "c"]);
    }

    #[test]
    fn an_unknown_required_dependency_suggests_the_nearest_name() {
        match Graph::build(&[
            desc_with("networkd", ServiceKind::Process, &[], &[]),
            desc_with("sshd", ServiceKind::Process, &["netwrok"], &[]),
        ]) {
            Err(GraphError::UnknownDependency {
                service,
                dep,
                suggestion,
            }) => {
                assert_eq!(service, "sshd");
                assert_eq!(dep, "netwrok");
                assert_eq!(suggestion.as_deref(), Some("networkd"));
            }
            other => panic!("expected UnknownDependency, got {other:?}"),
        }
    }

    #[test]
    fn a_wildly_wrong_name_gets_no_suggestion() {
        match Graph::build(&[
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with(
                "b",
                ServiceKind::Process,
                &["completely_different_thing"],
                &[],
            ),
        ]) {
            Err(GraphError::UnknownDependency { suggestion, .. }) => assert_eq!(suggestion, None),
            other => panic!("expected UnknownDependency, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_optional_dependency_is_a_warning_not_an_error() {
        let (g, findings) = Graph::build_with_findings(&[
            desc_with("app", ServiceKind::Process, &[], &["redis"]),
            desc_with("redis2", ServiceKind::Process, &[], &[]),
        ])
        .expect("an unknown optional edge must not fail the build");
        let app = g.index_of("app").unwrap();
        assert!(
            g.optional_of(app).is_empty(),
            "the dangling edge is dropped"
        );
        assert_eq!(codes(&findings), ["W100"]);
        let w = &findings[0];
        assert_eq!(w.severity, Severity::Warning);
        assert!(
            !w.severity.is_fatal(),
            "a finding must never be able to stop a boot"
        );
        let rendered = w.render("app.conf");
        assert!(rendered.contains("app"), "names the service: {rendered}");
        assert!(
            rendered.contains("redis"),
            "names the dependency: {rendered}"
        );
        assert!(
            rendered.contains("did you mean"),
            "offers the fix: {rendered}"
        );
    }

    #[test]
    fn a_finding_carries_the_line_it_was_found_on() {
        let mut d = desc_with("app", ServiceKind::Process, &[], &["ghost"]);
        d.source_line = 17;
        let (_, findings) = Graph::build_with_findings(&[d]).expect("only a warning");
        assert_eq!(findings[0].span, Some(Span::point(17, 1)));
        assert!(
            findings[0]
                .render("app.conf")
                .starts_with("app.conf:17:1: warning[W100]")
        );
    }

    #[test]
    fn a_description_with_no_known_line_gets_no_span() {
        // A span pointing at line 0 would be a lie; `Diagnostic` says `None`
        // means "file-level problem".
        let mut d = desc_with("app", ServiceKind::Process, &[], &["ghost"]);
        d.source_line = 0;
        let (_, findings) = Graph::build_with_findings(&[d]).expect("only a warning");
        assert_eq!(findings[0].span, None);
        assert!(
            findings[0]
                .render("app.conf")
                .starts_with("app.conf:-: warning")
        );
    }

    #[test]
    fn a_dependency_declared_both_ways_warns_and_the_required_edge_wins() {
        let (g, findings) = Graph::build_with_findings(&[
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with("b", ServiceKind::Process, &["a"], &["a"]),
        ])
        .expect("linkable");
        assert_eq!(codes(&findings), ["W101"]);
        let b = g.index_of("b").unwrap();
        assert_eq!(g.required_of(b), &[0], "the strict reading is kept");
        let text = findings[0].render("b.conf");
        assert!(text.contains("required"), "explains the resolution: {text}");
    }

    #[test]
    fn build_accepts_what_build_with_findings_only_warns_about() {
        let g = Graph::build(&[desc_with("app", ServiceKind::Process, &[], &["ghost"])])
            .expect("valid graph");
        assert_eq!(g.len(), 1);
    }

    // ── commands and size ───────────────────────────────────────────────────

    #[test]
    fn a_process_without_a_command_is_rejected_but_a_target_is_not() {
        let mut p = desc("sshd");
        p.command = String::new();
        match Graph::build(&[p]) {
            Err(GraphError::NoCommand { service, kind, .. }) => {
                assert_eq!(service, "sshd");
                assert_eq!(kind, ServiceKind::Process);
            }
            other => panic!("expected NoCommand, got {other:?}"),
        }
        for kind in [ServiceKind::Script, ServiceKind::Console] {
            let mut d = desc_with("x", kind, &[], &[]);
            d.command = String::new();
            assert!(
                matches!(Graph::build(&[d]), Err(GraphError::NoCommand { .. })),
                "type = {} also needs a command",
                kind.name()
            );
        }
        let t = desc_with("base.target", ServiceKind::Target, &[], &[]);
        assert!(t.command.is_empty());
        assert!(
            Graph::build(&[t]).is_ok(),
            "a target has no command by design"
        );
    }

    #[test]
    fn a_whitespace_only_command_is_as_useless_as_none() {
        // `execve("   ")` is a valid syscall and a guaranteed failure, so this
        // is the same defect as an empty command, not a special case.
        for blank in ["", " ", "   ", "\t", " \t\n "] {
            let mut d = desc("sshd");
            d.command = String::from(blank);
            assert!(
                matches!(Graph::build(&[d]), Err(GraphError::NoCommand { .. })),
                "{blank:?} must not count as a command"
            );
        }
    }

    #[test]
    fn the_service_cap_is_enforced_before_anything_is_allocated() {
        let over: Vec<ServiceDesc> = (0..=MAX_SERVICES)
            .map(|i| {
                let mut d = desc("s");
                d.name = format!("s{i}");
                d
            })
            .collect();
        assert_eq!(
            Graph::build(&over).err(),
            Some(GraphError::TooManyServices {
                count: MAX_SERVICES + 1
            })
        );
        // One over the cap is an error, not an out-of-bounds index somewhere
        // further down: the count is checked before a single index exists.
        assert!(Graph::build(&over).unwrap_err().span().is_none());
    }

    #[test]
    fn exactly_the_cap_is_accepted() {
        let at: Vec<ServiceDesc> = (0..MAX_SERVICES)
            .map(|i| {
                let mut d = desc("s");
                d.name = format!("s{i:05}");
                d
            })
            .collect();
        let g = Graph::build(&at).expect("the cap is inclusive");
        assert_eq!(g.len(), MAX_SERVICES);
        assert_eq!(g.index_of("s04000"), Some(4000));
    }

    // ── the self-dependency ─────────────────────────────────────────────────

    #[test]
    fn a_service_depending_on_itself_is_its_own_diagnosis() {
        for opt in [false, true] {
            let d = desc_with(
                "a",
                ServiceKind::Process,
                if opt { &[] } else { &["a"] },
                if opt { &["a"] } else { &[] },
            );
            match Graph::build(&[d]) {
                Err(GraphError::SelfDependency { service, .. }) => assert_eq!(service, "a"),
                other => panic!("expected SelfDependency, got {other:?}"),
            }
        }
    }

    // ── the suggestion engine ───────────────────────────────────────────────

    #[test]
    fn levenshtein_is_correct_on_known_pairs() {
        assert_eq!(levenshtein(b"", b""), 0);
        assert_eq!(levenshtein(b"abc", b"abc"), 0);
        assert_eq!(levenshtein(b"", b"abc"), 3);
        assert_eq!(levenshtein(b"abc", b""), 3);
        assert_eq!(levenshtein(b"kitten", b"sitting"), 3);
        assert_eq!(levenshtein(b"flaw", b"lawn"), 2);
        assert_eq!(levenshtein(b"a", b"b"), 1);
    }

    #[test]
    fn levenshtein_is_symmetric() {
        let pairs: [(&str, &str); 5] = [
            ("network.target", "netwrok.target"),
            ("a", "abcdefghij"),
            ("x", "xx"),
            ("", "q"),
            ("postgres", "postgresql"),
        ];
        for (a, b) in pairs {
            assert_eq!(
                levenshtein(a.as_bytes(), b.as_bytes()),
                levenshtein(b.as_bytes(), a.as_bytes()),
                "{a} vs {b}"
            );
        }
    }

    #[test]
    fn suggestions_are_stable_on_ties() {
        let names: Vec<(String, Idx)> =
            vec![("alpha".into(), 0), ("beta".into(), 1), ("gamma".into(), 2)];
        // "delta" is one edit from both "beta" and "gamma": the smaller name
        // must win, on every run, whatever order the caller passes.
        assert_eq!(suggest("delta", &names).as_deref(), Some("beta"));
        // Three edits is still inside the budget, four is not.
        assert_eq!(suggest("delt", &names).as_deref(), Some("beta"));
        assert_eq!(suggest("zzz", &names), None);
        assert_eq!(
            suggest("", &names),
            None,
            "an empty needle suggests nothing"
        );
    }

    // ── the degenerate graph ────────────────────────────────────────────────

    #[test]
    fn an_empty_description_set_is_an_empty_graph() {
        let (g, findings) = Graph::build_with_findings(&[]).expect("empty is valid");
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);
        assert_eq!(g.index_of("anything"), None);
        assert!(findings.is_empty());
    }

    #[test]
    fn a_service_with_no_edges_has_an_empty_edge_list() {
        // The degenerate CSR: one node, `offsets == [0, 0]`, nothing to slice.
        let g = Graph::build(&[desc("a"), desc("b")]).expect("valid graph");
        assert_eq!(g.required_of(0), &[] as &[Idx]);
        assert_eq!(g.optional_of(1), &[] as &[Idx]);
        assert!(g.dependents_of(0).is_empty());
    }

    #[test]
    fn dependents_are_reported_in_index_order() {
        let g = Graph::build(&[
            desc_with("a", ServiceKind::Process, &["base"], &[]),
            desc_with("b", ServiceKind::Process, &["base"], &[]),
            desc_with("base", ServiceKind::Process, &[], &[]),
        ])
        .expect("valid graph");
        let base = g.index_of("base").unwrap();
        assert_eq!(g.dependents_of(base), vec![0, 1]);
    }
}
