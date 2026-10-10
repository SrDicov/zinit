//! Freezing: a set of [`ServiceDesc`] becomes an immutable, topologically
//! ordered [`zcore::Plan`].
//!
//! This module is deliberately the smallest of the three. Everything it does is
//! a decision that must be made **once**, at load, and never revisited:
//!
//! * names become [`Idx`],
//! * required edges become index lists,
//! * a total order over the services is computed and stored twice - forward for
//!   starting, reversed for stopping,
//! * every policy the operator wrote (`restart`, `ready`, `log`, `run_as`,
//!   timeouts) is resolved into a `zcore` type.
//!
//! After this, `reconcile` is two linear passes over `order_up` and
//! `order_down` with no queue, no recursion and no depth counter. See
//! `DESIGN.md` §4.2. That is the whole point: the ordering *is* the algorithm,
//! so a bug in the order is a bug that is caught here, at load, by a test,
//! instead of a machine that boots to nothing.
//!
//! # Invariants guaranteed by this module
//!
//! 1. `order_up.len() == services.len()`, and so does `order_down`.
//! 2. `order_up` is a permutation of `0..services.len()`.
//! 3. For every `i` and every `d` in `services[i].required`, `pos(d) < pos(i)`.
//! 4. `order_down` is the exact reverse of `order_up`.
//! 5. The result is a pure function of the input *set*. The same services in
//!    any input order produce byte-identical plans.
//!
//! Invariants 3 to 5 are the reason the order is computed here and not in the
//! runtime: a `Plan` that is immutable is a `Plan` whose edges can be trusted,
//! and "am I allowed to start" becomes an array comparison instead of a walk.
//!
//! There is deliberately no `dependents` reverse-edge list. Nothing in `zcore`
//! ever asked "who depends on me": `deps_satisfied` walks `required` forward,
//! one service at a time, and the cascade walks `order_up`/`order_down`
//! topologically. It used to be built here, handed to the runtime, and never
//! read once. `ponytail: precomputed inverse edges, drop for good unless a
//! caller needs O(1) dependents; the counting pass was 35 lines of freeze`.
//!
//! # Algorithm choice: Kahn, iteratively, with a heap
//!
//! The order is computed with Kahn's algorithm over a binary min-heap keyed by
//! index, with a *required-edges-only* indegree counter. Three deliberate
//! choices, each of which costs something:
//!
//! * **Iterative.** A recursive DFS visits a 1000-deep chain happily and then
//!   overflows the stack in the middle of a boot, and a chain that deep is not
//!   exotic on a machine with 4000 packages. The `find_cycle` walk that reports
//!   the offending loop is iterative for the same reason: the one graph worth
//!   being able to *read* is the deep one.
//! * **A min-heap, not a queue.** Among all the valid topological orders this
//!   returns the one that is smallest in index order at every step. A FIFO
//!   queue is also deterministic and one notch cheaper, but it produces an
//!   order that jumps around alphabetically, which makes the CLI's output and a
//!   test's expectation much harder to read for no benefit.
//! * **An explicit reverse index.** The release loop needs the dependents of
//!   the node it just placed. `Graph::dependents_of` is `O(V)` per call, so
//!   using it here would make the sort `O(V^2)` on a machine that is allowed to
//!   have 4096 services. The reverse CSR below is `O(V + E)` to build and turns
//!   the whole pass linear apart from the heap.
//!
//! Optional edges are deliberately **not** in the indegree. An optional
//! dependency is waited for but its absence is not fatal, so it must not be able
//! to make the graph unorderable; those edges are still emitted so they appear
//! in the plan, and they land as early as the required edges allow.
//!
//! # The order of the two orders
//!
//! [`Plan::order_up`] is **topological** - not alphabetical. The alphabetical
//! order is the *index* order, established back in `graph.rs`, and it exists so
//! that the tie-break above and every test expectation are reproducible. Only
//! `order_up` is ever walked by the runtime.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use zcore::{Idx, LogSink, Plan, Ready, ServicePlan};

use crate::desc::ServiceDesc;
use crate::diagnostic::{Diagnostic, Severity};
use crate::graph::{Graph, GraphError, finding};

/// Default `stop-timeout`, matching the table in `DESIGN.md` §5.
const DEFAULT_STOP_TIMEOUT_MS: u64 = 10_000;

/// Default `start-timeout`, matching the table in `DESIGN.md` §5.
const DEFAULT_START_TIMEOUT_MS: u64 = 60_000;
/// Readiness deadline, shorter than the start deadline on purpose: a service
/// that has not answered in 30 s has usually failed, and waiting a full minute
/// to say so only makes the outage harder to see.
const DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;

/// Default `log` path root: a symlink under `/var/log/zinit/<name>.log`.
///
/// A symlink rather than a real file, so the same policy works on tmpfs, on a
/// read-only `/var` and on syslog-only systems without `zinit` having to know
/// which one it is on. This is the one place a service name is used as a path
/// component, and it is exactly why `graph::validate_name` refuses `/` and
/// `..` at load time - and why that refusal has to happen *before* this code
/// runs, which it does, because `build_plan` links the graph first.
const DEFAULT_LOG_DIR: &str = "/var/log/zinit";

/// Build the frozen plan from a set of descriptions.
///
/// **The only way to make a `Plan`.** [`Graph::to_plan`] delegates here rather
/// than reimplementing the order, because two implementations of a topological
/// sort is one too many and they would eventually disagree.
///
/// Returns [`GraphError`] for anything structurally wrong: an unknown required
/// dependency, a malformed name, a self-dependency, a cycle, a duplicate, a bad
/// name, a `process` with no command, or more than `MAX_SERVICES` services.
/// A cycle is an `Err` here, **not** a panic: this crate is built
/// `panic = "abort"` because the supervisor is a child of PID 1, where a panic
/// takes the machine with it. There is no input that reaches a `panic!`.
///
/// Non-fatal findings come from [`build_plan_with_findings`], which cannot fail
/// because of one.
pub fn build_plan(descs: &[ServiceDesc]) -> core::result::Result<Plan, GraphError> {
    build_plan_with_findings(descs).map(|(plan, _)| plan)
}

/// Build the frozen plan and return the non-fatal findings with it.
///
/// For `zcheck`: the same plan, plus everything the operator should be told
/// about the descriptions that were accepted anyway. The two entry points share
/// one implementation, so the plan `zcheck` validates is by construction the
/// plan the supervisor would run.
///
/// The findings are [`Diagnostic`]s and are the *only* severity vocabulary in
/// the crate - see [`crate::Severity`]. None of them can be an error, so a
/// thousand of them is still a passing run.
///
/// A note on spans: [`Diagnostic`] deliberately stores no file name, because the
/// same description can be checked from a file, from `<stdin>` or from a string
/// literal in a test. A graph, though, is inherently multi-file, so each finding
/// here carries the *line* it was found on and the *service* it is about in its
/// message; the caller owns the file name and should render one bag per file
/// rather than pretending a whole directory is one document.
pub fn build_plan_with_findings(
    descs: &[ServiceDesc],
) -> core::result::Result<(Plan, Vec<Diagnostic>), GraphError> {
    let (graph, mut findings) = Graph::build_with_findings(descs)?;
    let plan = freeze(&graph, &mut findings)?;
    Ok((plan, findings))
}

/// Freeze an already-validated graph.
///
/// Split out of [`build_plan_with_findings`] so the linking pass and the
/// freezing pass can be told apart in a backtrace, and so there is exactly one
/// implementation of the freeze. Private, because the *only* public door is
/// [`build_plan`].
///
/// The acyclicity requirement lives here, not in `Graph::build`: a graph with a
/// cycle in it is a perfectly good graph object (cyclicity is an ordering
/// question, not a linking one), and only a *plan* has to be acyclic.
///
/// Findings that can only be seen once the policy has been resolved - a
/// `ready` directive on a target, a `user` this crate cannot resolve - are
/// appended to `findings` here rather than in `graph.rs`, because they are
/// about the frozen values and not about the wiring.
fn freeze(graph: &Graph, findings: &mut Vec<Diagnostic>) -> core::result::Result<Plan, GraphError> {
    // Ordering first: a cyclic graph is not freezable, and building a thousand
    // `ServicePlan`s to throw them away would be the most expensive way to say
    // "a -> b -> a".
    let order_up = topological_order(graph)?;
    let mut order_down = order_up.clone();
    order_down.reverse();

    let n = graph.len();
    let mut services: Vec<ServicePlan> = Vec::with_capacity(n);
    for i in 0..n {
        // ── The identity trapdoor, enforced ─────────────────────────────────
        //
        // `zconfig` has no `/etc/passwd`, so `user = mysql` parses to an
        // unresolved *name*. Every consumer downstream reads
        // `ServicePlan::run_as`, and an `Option` that is `None` because nobody
        // looked the name up is indistinguishable from "run as root" — which
        // is exactly how a service that asked to drop privileges ends up not
        // dropping them.
        //
        // So the check lives here, in the one function that turns descriptions
        // into a plan, and it happens *before* anything is frozen. The runtime
        // resolves the names and calls again. A previous version of this file
        // documented this invariant in a doc-comment and did not enforce it;
        // the comment was the enforcement, which is how services ran as root.
        if let Some(name) = graph.desc(i).unresolved_identity() {
            return Err(GraphError::UnresolvedIdentity {
                service: String::from(&graph.desc(i).name),
                name: String::from(name),
            });
        }
        services.push(service_plan(graph, i, findings));
    }

    Ok(Plan {
        services,
        order_up,
        order_down,
    })
}

/// A deterministic topological order of `graph`: `pos(d) < pos(i)` for every
/// required edge `d -> i`.
///
/// Among the orders that satisfy the graph, this returns the one that is
/// smallest in index order at every step ("lexicographically least"). That is
/// what makes a frozen plan reproducible across runs, machines and filesystems,
/// and therefore what makes `assert_eq!` on two whole plans a meaningful test
/// rather than a coin flip.
///
/// `Err(GraphError::Cycle { path })` when the graph has a required-dependency
/// cycle, carrying the actual loop rather than a count.
pub fn topological_order(graph: &Graph) -> core::result::Result<Vec<Idx>, GraphError> {
    let (order, placed) = kahn(graph);
    if placed.iter().all(|&p| p) {
        return Ok(order);
    }
    // A node Kahn never released is on, or can only be reached from, a cycle.
    // `find_cycle` walks the residual set to find the loop itself, which is the
    // only part of the report an operator can act on.
    let path = find_cycle(graph, &placed)
        .into_iter()
        .map(|i| graph.desc(i).name.clone())
        .collect();
    Err(GraphError::Cycle { path })
}

/// `true` when `graph`'s required edges contain a cycle.
pub fn has_cycle(graph: &Graph) -> bool {
    check_acyclic(graph).is_some()
}

/// `Some(GraphError::Cycle { path })` if `graph` has a required-dependency
/// cycle, with the actual cycle in it.
///
/// A thin wrapper over [`topological_order`], for a caller that is asking the
/// question directly rather than wanting a plan. Returning the whole error
/// rather than a bool is deliberate: the cycle path is the only part of the
/// diagnostic an operator can use, and reconstructing it is most of the work.
pub fn check_acyclic(graph: &Graph) -> Option<GraphError> {
    topological_order(graph).err()
}

/// One Kahn pass, shared by every entry point above so there is exactly one
/// implementation of the order.
///
/// Returns the placement order (the Kahn sequence, *not* a full topological
/// order when the graph has a cycle) and a per-node "was placed" mask. The mask
/// doubles as the residual set for cycle extraction.
///
/// Complexity: `O(V + E log V)`. The `log V` is the heap. Everything else is
/// linear: the dependent enumeration walks a CSR reverse index built once by
/// [`reverse_edges`], not a scan of the whole graph per popped node.
fn kahn(graph: &Graph) -> (Vec<Idx>, Vec<bool>) {
    let n = graph.len();
    let (r_offsets, r_to) = reverse_edges(graph);

    let mut indegree: Vec<usize> = vec![0; n];
    for (i, slot) in indegree.iter_mut().enumerate() {
        *slot = graph.required_of(i).len();
    }

    // Min-heap of "ready" nodes, keyed by index: that single comparison is the
    // whole tie-break rule of the topological sort.
    let mut ready: Vec<Idx> = (0..n).filter(|&i| indegree[i] == 0).collect();
    heapify(&mut ready);

    let mut order: Vec<Idx> = Vec::with_capacity(n);
    let mut placed: Vec<bool> = vec![false; n];
    while let Some(i) = heap_pop(&mut ready) {
        placed[i] = true;
        order.push(i);
        for &d in &r_to[r_offsets[i]..r_offsets[i + 1]] {
            let slot = &mut indegree[d];
            // Required edges are deduplicated, so each of `d`'s requirements
            // releases it exactly once and this cannot hit zero twice.
            debug_assert!(
                *slot > 0,
                "a node's indegree cannot be decremented below zero"
            );
            *slot -= 1;
            if *slot == 0 {
                heap_push(&mut ready, d);
            }
        }
    }
    (order, placed)
}

/// The required edges, reversed, as a CSR.
///
/// `dependents(i) == to[offsets[i]..offsets[i + 1]]`, ascending, because the
/// counting pass fills it in ascending `i`. Built in `O(V + E)` with no
/// per-node `Vec`.
///
/// The alternative, `Graph::dependents_of(i)` in a loop, is `O(V^2)` overall
/// with an allocation per node - 16M comparisons and 4096 allocations on a
/// machine that is explicitly allowed that many services.
fn reverse_edges(graph: &Graph) -> (Vec<usize>, Vec<Idx>) {
    let n = graph.len();

    // Pass 1: in-degrees. Row `d` is sized by how many services *require* `d`,
    // which is not the same number as how many services `d` itself requires.
    // Summing the out-degrees instead produces a CSR that looks plausible and
    // is wrong for every node at once - and the sort then never releases
    // anything, so every graph looks cyclic. This is the bug the test at the
    // bottom of the file exists to catch, and it caught it.
    let mut counts: Vec<usize> = vec![0; n];
    for i in 0..n {
        for &d in graph.required_of(i) {
            debug_assert!(d < n, "an edge points outside the graph");
            counts[d] += 1;
        }
    }

    // Pass 2: prefix sums. `offsets[d]` is where row `d` starts and
    // `offsets[d + 1]` where it ends, so a node with no dependents gets an
    // empty row rather than a special case.
    let mut offsets: Vec<usize> = Vec::with_capacity(n + 1);
    let mut total = 0usize;
    offsets.push(0);
    for &c in &counts {
        total += c;
        offsets.push(total);
    }

    // Pass 3: fill. `cursor[d]` is the next free slot in row `d`, and it starts
    // at the row's beginning; visiting `i` in ascending order is what makes
    // each row come out sorted, so no post-sort is needed.
    let mut cursor: Vec<usize> = offsets[..n].to_vec();
    let mut to: Vec<Idx> = vec![0; total];
    for i in 0..n {
        for &d in graph.required_of(i) {
            debug_assert!(d < n, "an edge points outside the graph");
            to[cursor[d]] = i;
            cursor[d] += 1;
        }
    }
    (offsets, to)
}

/// The actual cycle behind a failed Kahn pass, as indices: `[a, b, a]`.
///
/// Iterative, deliberately: a DFS that recurses would put its own stack budget
/// in competition with the depth of the graph it is diagnosing, and the one
/// graph worth being able to read is the deep one.
///
/// Three phases, because they answer different questions:
///
/// 1. **Prune.** Everything Kahn never released is on, or downstream of, a
///    cycle. Restricting the walk to that set is what keeps the reported path
///    short and relevant instead of an arbitrary walk through 4000 nodes.
/// 2. **Walk.** Iterative DFS inside that set, recording the first step out of
///    the start node and closing the loop when a node is revisited.
/// 3. **Trim.** Everything before the repeated node led *into* the cycle, so it
///    is dropped: the message is the cycle itself, which is what the operator
///    has to edit.
///
/// `on_stack` is the DFS's own marker, and it is not redundant with `placed`:
/// the residual set alone cannot close a loop. A node that is merely downstream
/// of a cycle is residual too, so a naive walk would report a path that leaves
/// the cycle and never comes back.
///
/// The walk is by *index*, not by name. Names would work today, because
/// duplicates are rejected before this point, but identity-by-name is a
/// coincidence of another check and a cycle is exactly the place where two
/// things being conflated is most expensive.
fn find_cycle(graph: &Graph, placed: &[bool]) -> Vec<Idx> {
    let n = graph.len();
    // `unplaced(i)` = "Kahn never released i" = candidate cycle member.
    let unplaced = |i: Idx| i < n && !placed[i];

    // Start at the lowest unplaced index, so the same graph always yields the
    // same message.
    let start = match (0..n).find(|&i| unplaced(i)) {
        Some(i) => i,
        // Unreachable: `find_cycle` is only called when Kahn placed fewer than
        // `n` nodes. A defensive `None` would be a lie; an empty path is not.
        None => return Vec::new(),
    };

    let mut on_stack: Vec<bool> = vec![false; n];
    let mut step: Vec<usize> = vec![0; n];
    let mut stack: Vec<Idx> = Vec::new();
    let mut path: Vec<Idx> = Vec::new();

    stack.push(start);
    on_stack[start] = true;
    path.push(start);

    while let Some(&i) = stack.last() {
        let deps = graph.required_of(i);
        if step[i] < deps.len() {
            let d = deps[step[i]];
            step[i] += 1;
            if !unplaced(d) {
                continue;
            }
            if on_stack[d] {
                // Closed a loop.
                let at = path.iter().position(|&p| p == d).unwrap_or(0);
                let mut cycle: Vec<Idx> = path[at..].to_vec();
                cycle.push(d);
                return cycle;
            }
            on_stack[d] = true;
            stack.push(d);
            path.push(d);
        } else {
            on_stack[i] = false;
            stack.pop();
            path.pop();
        }
    }

    // Unreachable in practice: an unplaced node always has an unplaced
    // dependency, so the walk always closes a loop. Naming the start beats
    // naming nothing to an operator staring at a boot log.
    vec![start]
}

/// Freeze one service: description + resolved indices + resolved policy.
fn service_plan(graph: &Graph, i: Idx, findings: &mut Vec<Diagnostic>) -> ServicePlan {
    let d = graph.desc(i);

    // Copy the edge lists out first, so the borrow of `graph` ends before the
    // owned `ServicePlan` is built.
    let required: Vec<Idx> = graph.required_of(i).to_vec();

    let mut sp = ServicePlan::new(d.name.clone());
    sp.kind = d.kind;
    sp.required = required;
    sp.restart = d.restart.policy;
    sp.ready = d.ready.into_ready();
    sp.start_timeout_ms = timeout_or(d.start_timeout_ms, DEFAULT_START_TIMEOUT_MS);
    sp.ready_timeout_ms = timeout_or(d.ready_timeout_ms, DEFAULT_READY_TIMEOUT_MS);
    sp.stop_timeout_ms = timeout_or(d.stop_timeout_ms, DEFAULT_STOP_TIMEOUT_MS);
    sp.restart_budget = d.restart.budget;
    sp.log = log_of(d);
    sp.run_as = d.run_as.map(|r| (r.uid, r.gid));
    sp.pid_file = d.pid_file.clone();
    sp.watchdog_sec = d.watchdog_sec;
    sp.listens = d.listens.clone();
    sp.seccomp = d.syscall_filter.map(|action| zcore::SeccompPolicy {
        action,
        allow: d.syscall_allow.clone(),
    });
    sp.drop_caps = d.drop_caps.clone();
    sp.enabled = d.enabled;

    // A target has no process, so it has no handshake to wait for. Forcing
    // `Ready::None` here is what lets the runtime treat targets uniformly
    // instead of special-casing them at every readiness site - and it makes
    // this the single place where a nonsense `ready` on a target is dropped.
    // The parser is not allowed to do it: a description read on its own, with
    // no graph around it, is exactly the case where the operator's `ready =`
    // has to be preserved verbatim so that `zcheck` can say why it was
    // ignored.
    if !d.kind.has_process() {
        if !matches!(sp.ready, Ready::None) {
            let message = format!(
                "{}: type = {} has no process, so `ready = {}` is ignored",
                d.name,
                d.kind.name(),
                d.ready.describe()
            );
            findings.push(finding(
                Severity::Warning,
                d,
                "W110",
                message,
                Some("remove the `ready` directive from a target".to_string()),
            ));
        }
        sp.ready = Ready::None;
    }

    sp
}

/// `0` means "not specified", because a duration of zero is not a useful
/// timeout: an instant `stop-timeout` would `SIGKILL` every service and an
/// instant `start-timeout` would fail every service. The parser leaves the
/// default in place, so this only ever fires for a hand-built description, and
/// it is the belt to that braces.
fn timeout_or(value: u64, default: u64) -> u64 {
    if value == 0 { default } else { value }
}

/// The log sink, with the documented default resolved to a real path.
///
/// A `log = file` that arrives without a path means "the default path for this
/// service", which is the only reading that does not open the empty string. The
/// name is interpolated into a path here, which is the whole reason
/// `graph::validate_name` rejects `/`, `..` and leading dots at load time -
/// before this function can run, because `build_plan` links the graph first.
///
/// Note the ordering guarantee that makes this safe: this module is the *only*
/// consumer of service names, and it runs strictly after the check. A
/// `ServicePlan` can never contain a name that did not pass validation.
fn log_of(d: &ServiceDesc) -> LogSink {
    match d.log.clone().into_sink() {
        LogSink::File {
            path,
            max_bytes,
            backups,
        } if path.is_empty() => LogSink::File {
            path: default_log_path(&d.name),
            max_bytes,
            backups,
        },
        sink => sink,
    }
}

/// `/var/log/zinit/<name>.log`.
fn default_log_path(name: &str) -> String {
    let mut s = String::with_capacity(DEFAULT_LOG_DIR.len() + name.len() + 5);
    s.push_str(DEFAULT_LOG_DIR);
    s.push('/');
    s.push_str(name);
    s.push_str(".log");
    s
}

/// Assertions shared by every success-path test, so no test can quietly produce
/// a plan the runtime would mis-walk.
///
/// Test-only on purpose. These invariants are the module's contract with the
/// runtime, and the strongest available statement of that contract is a
/// function the production build does not carry: every success-path test calls
/// it, and it checks the numbered properties at the top of this file
/// directly rather than trusting the code that claims to establish them.
///
/// Written to be linear. The obvious formulation - `order_up.iter().position()`
/// inside a loop over every edge - is `O(V * E)`, which is fine for a
/// ten-service test and a minute and a half for the ten-thousand-service one
/// that exists precisely to prove the sort does not recurse.
#[cfg(test)]
fn assert_invariants(plan: &Plan) {
    let n = plan.services.len();
    assert_eq!(plan.order_up.len(), n, "order_up must cover every service");
    assert_eq!(
        plan.order_down.len(),
        n,
        "order_down must cover every service"
    );

    // Invert the order once, then every position query is a lookup.
    let mut pos: Vec<usize> = vec![0; n];
    let mut seen: Vec<bool> = vec![false; n];
    for (p, &i) in plan.order_up.iter().enumerate() {
        assert!(i < n, "order_up contains a foreign index {i}");
        assert!(!seen[i], "order_up repeats index {i}");
        seen[i] = true;
        pos[i] = p;
    }

    for (i, sp) in plan.services.iter().enumerate() {
        for &d in &sp.required {
            assert!(d < n, "{} requires a foreign index {d}", sp.name);
            assert!(d != i, "{} must not require itself", sp.name);
            assert!(
                pos[d] < pos[i],
                "{} must start before {}",
                plan.services[d].name,
                sp.name
            );
        }
    }

    let reversed: Vec<Idx> = plan.order_up.iter().rev().copied().collect();
    assert_eq!(
        plan.order_down, reversed,
        "order_down must be the exact reverse"
    );
}

// ── binary min-heap over `Idx` ──────────────────────────────────────────────
//
// Hand-rolled because the only dependency this crate is allowed is `zcore`. It
// is a min-heap on the value, and for `Idx` that means "lowest index first",
// which is the entire tie-break rule of the topological sort.

/// Sift the element at `parent` down until the heap invariant holds.
///
/// A slice, not a `Vec`: the only thing done to it is `swap`, which a slice has.
/// Taking `&mut Vec` would let a future caller push into a heap mid-operation.
fn sift_down(heap: &mut [Idx], mut parent: usize) {
    let n = heap.len();
    loop {
        let left = parent * 2 + 1;
        if left >= n {
            return;
        }
        let right = left + 1;
        let mut best = left;
        if right < n && heap[right] < heap[left] {
            best = right;
        }
        if heap[parent] <= heap[best] {
            return;
        }
        heap.swap(parent, best);
        parent = best;
    }
}

/// Turn an arbitrary slice into a valid min-heap (bottom-up, `O(n)`).
///
/// A slice, like [`sift_down`]: heapify only permutes, and a `&mut Vec` would
/// let a caller append to a heap that is not a heap.
fn heapify(heap: &mut [Idx]) {
    if heap.is_empty() {
        return;
    }
    for parent in (0..heap.len().div_ceil(2)).rev() {
        sift_down(heap, parent);
    }
}

/// Push one element, restoring the invariant upwards.
fn heap_push(heap: &mut Vec<Idx>, value: Idx) {
    heap.push(value);
    let mut child = heap.len() - 1;
    while child > 0 {
        let parent = (child - 1) / 2;
        if heap[parent] <= heap[child] {
            return;
        }
        heap.swap(parent, child);
        child = parent;
    }
}

/// Pop the smallest element, restoring the invariant downwards.
fn heap_pop(heap: &mut Vec<Idx>) -> Option<Idx> {
    match heap.len() {
        0 => None,
        1 => heap.pop(),
        _ => {
            let top = heap[0];
            let last = heap.len() - 1;
            heap.swap(0, last);
            heap.pop();
            sift_down(heap, 0);
            Some(top)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use zcore::ServiceKind;
    use zcore::{Budget, MAX_SERVICES, Restart, StrictReady};

    use crate::value::{LogSpec, ReadySpec, RestartSpec, RunAs};

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

    /// A description that specified no timeouts, which is what a directive-less
    /// `.conf` looks like once someone has zeroed the fields.
    fn bare(name: &str) -> ServiceDesc {
        let mut d = desc(name);
        d.start_timeout_ms = 0;
        d.stop_timeout_ms = 0;
        d
    }

    fn plan_of(descs: Vec<ServiceDesc>) -> Plan {
        let p = build_plan(&descs).expect("expected a valid plan");
        assert_invariants(&p);
        p
    }

    /// Every required edge, as `(dependency, dependent)` name pairs.
    fn required_pairs(plan: &Plan) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for sp in &plan.services {
            for &d in &sp.required {
                out.push((plan.services[d].name.clone(), sp.name.clone()));
            }
        }
        out
    }

    fn codes(findings: &[Diagnostic]) -> Vec<&'static str> {
        findings.iter().map(|d| d.code).collect()
    }

    // ── the trivial cases ───────────────────────────────────────────────────

    #[test]
    fn enabled_travels_from_description_to_plan() {
        let mut off = desc("off");
        off.enabled = false;
        let p = plan_of(vec![desc("on"), off]);
        let on = &p.services[p.index_of("on").expect("on")];
        let off = &p.services[p.index_of("off").expect("off")];
        assert!(on.enabled, "omitted means yes");
        assert!(!off.enabled);
    }

    #[test]
    fn an_empty_description_set_freezes_to_an_empty_plan() {
        let p = plan_of(vec![]);
        assert_eq!(p.services.len(), 0);
        assert_eq!(p.services.len(), 0);
        assert!(p.order_up.is_empty());
        assert!(p.order_down.is_empty());
    }

    #[test]
    fn a_lone_service_is_its_own_order() {
        let p = plan_of(vec![desc("sshd")]);
        assert_eq!(p.services.len(), 1);
        assert_eq!(p.order_up, vec![0]);
        assert_eq!(p.order_down, vec![0]);
        assert_eq!(p.services[0].name, "sshd");
    }

    #[test]
    fn unrelated_services_come_out_in_index_order() {
        let p = plan_of(vec![desc("c"), desc("a"), desc("b")]);
        assert_eq!(p.order_up, vec![0, 1, 2], "alphabetical indices, no edges");
        assert_eq!(
            p.order_up
                .iter()
                .map(|&i| p.services[i].name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    // ── the topological order ───────────────────────────────────────────────

    #[test]
    fn a_chain_is_ordered_even_when_letters_disagree() {
        // `a` is alphabetically last, so only a real topological sort can put
        // it first: index order alone would fail this.
        let p = plan_of(vec![
            desc_with("c", ServiceKind::Process, &["b"], &[]),
            desc_with("b", ServiceKind::Process, &["a"], &[]),
            desc_with("a", ServiceKind::Process, &[], &[]),
        ]);
        assert_eq!(p.order_up, vec![0, 1, 2]);
        assert_eq!(
            p.order_up
                .iter()
                .map(|&i| p.services[i].name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn a_diamond_starts_its_base_first() {
        //        a
        //       / \
        //      b   c
        //       \ /
        //        d
        let p = plan_of(vec![
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with("b", ServiceKind::Process, &["a"], &[]),
            desc_with("c", ServiceKind::Process, &["a"], &[]),
            desc_with("d", ServiceKind::Process, &["b", "c"], &[]),
        ]);
        let names: Vec<&str> = p
            .order_up
            .iter()
            .map(|&i| p.services[i].name.as_str())
            .collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
    }

    #[test]
    fn a_thousand_deep_chain_neither_recurses_nor_misses_a_node() {
        const N: usize = 1000;
        let descs = chain(N);
        let p = plan_of(descs);
        assert_eq!(p.order_up, (0..N).collect::<Vec<Idx>>());
        assert_eq!(required_pairs(&p).len(), N - 1);
    }

    /// The reason Kahn is iterative: a recursive DFS handles this happily, and
    /// then overflows the stack on a real machine.
    ///
    /// `MAX_SERVICES` is 4096, so a longer chain cannot be a graph at all - the
    /// cap is rejected before a single index exists - which makes 4096 the
    /// deepest ordering this crate can ever be asked to do. It runs on a thread
    /// with a deliberately small stack, so "it did not recurse" is a fact about
    /// the algorithm instead of a fact about how much room this machine has.
    #[test]
    fn the_deepest_permitted_chain_does_not_touch_the_stack() {
        const STACK: usize = 64 * 1024;
        let n = MAX_SERVICES;
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || {
                let g = Graph::build(&chain(n)).expect("linkable");
                assert_eq!(g.len(), n);
                assert_eq!(
                    topological_order(&g).expect("a chain is acyclic"),
                    (0..n).collect::<Vec<Idx>>()
                );
                let p = build_plan(&chain(n)).expect("freezable");
                assert_eq!(p.order_up, (0..n).collect::<Vec<Idx>>());
            })
            .expect("spawn")
            .join()
            .expect("a stack overflow aborts the process rather than unwinding");
    }

    /// `s0000` depends on `s0000-1`, and so on. Zero-padded so byte order is
    /// numeric order and the expected result is the identity.
    fn chain(n: usize) -> Vec<ServiceDesc> {
        (0..n)
            .map(|i| {
                let mut d = desc(&format!("s{i:05}"));
                if i > 0 {
                    d.depends_required = vec![format!("s{:05}", i - 1)];
                }
                d
            })
            .collect()
    }

    #[test]
    fn a_dense_graph_is_ordered_in_one_pass() {
        // Every service depends on every *earlier* one: the maximum number of
        // distinct edges a 300-node DAG can hold.
        const N: usize = 300;
        let descs: Vec<ServiceDesc> = (0..N)
            .map(|i| {
                let mut d = desc(&format!("s{i:04}"));
                d.depends_required = (0..i).map(|j| format!("s{j:04}")).collect();
                d
            })
            .collect();
        let p = plan_of(descs);
        assert_eq!(p.order_up, (0..N).collect::<Vec<Idx>>());
        // Reverse edges: service 0 is required by all the others.
        assert_eq!(required_pairs(&p).len(), N * (N - 1) / 2);
    }

    #[test]
    fn an_optional_edge_orders_without_becoming_a_blocker() {
        // The plan does not store optional edges: `Runtime::deps_satisfied`
        // reads `required` only, which is exactly what makes `Optional` never
        // block (DESIGN §4.1, invariant 5). The ordering still honours the
        // optional edge, and that comes from the graph's indegree, not from the
        // service's own edge list.
        let p = plan_of(vec![
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with("b", ServiceKind::Process, &[], &["a"]),
        ]);
        let b = p.index_of("b").unwrap();
        assert!(
            p.services[b].required.is_empty(),
            "optional is not required"
        );
        assert_eq!(p.order_up, vec![0, 1]);
    }

    /// A cycle made *entirely* of optional edges is not a cycle: optional edges
    /// are not in the indegree, precisely so that "wait for it if it is there"
    /// can never make a plan unorderable.
    #[test]
    fn an_optional_cycle_still_yields_a_plan() {
        let p = plan_of(vec![
            desc_with("a", ServiceKind::Process, &[], &["b"]),
            desc_with("b", ServiceKind::Process, &[], &["a"]),
        ]);
        assert_eq!(p.order_up, vec![0, 1]);
    }

    #[test]
    fn an_unknown_optional_edge_disappears_from_the_plan() {
        let p = plan_of(vec![desc_with("b", ServiceKind::Process, &[], &["ghost"])]);
        assert!(
            p.services[0].required.is_empty(),
            "a dangling edge is not an index"
        );
    }

    #[test]
    fn a_target_graph_freezes_like_any_other() {
        // A target is a vertex in the DAG like any other; only its readiness
        // and command are special.
        let p = plan_of(vec![
            desc_with("app", ServiceKind::Process, &["base.target"], &[]),
            desc_with("base.target", ServiceKind::Target, &[], &[]),
        ]);
        let names: Vec<&str> = p
            .order_up
            .iter()
            .map(|&i| p.services[i].name.as_str())
            .collect();
        assert_eq!(names, ["base.target", "app"]);
    }

    // ── the frozen fields ───────────────────────────────────────────────────

    #[test]
    fn every_field_is_carried_over_from_the_description() {
        let mut d = desc("sshd");
        d.restart = RestartSpec::parse("always").expect("valid");
        d.start_timeout_ms = 12_345;
        d.stop_timeout_ms = 6_789;
        d.log = LogSpec::parse("syslog").expect("valid");
        d.run_as = Some(RunAs {
            uid: 1000,
            gid: 1001,
        });
        d.ready = ReadySpec::parse("tcp:8443").expect("valid");
        let p = plan_of(vec![d]);
        let sp = &p.services[0];
        assert_eq!(sp.name, "sshd");
        assert_eq!(sp.kind, ServiceKind::Process);
        assert_eq!(sp.restart, Restart::Always);
        assert_eq!(sp.start_timeout_ms, 12_345);
        assert_eq!(sp.stop_timeout_ms, 6_789);
        assert_eq!(sp.log, LogSink::Syslog);
        assert_eq!(sp.run_as, Some((1000, 1001)));
        assert_eq!(sp.ready, Ready::Strict(StrictReady::Tcp(8443)));
    }

    #[test]
    fn a_target_has_no_readiness_and_no_command() {
        let p = plan_of(vec![desc_with(
            "base.target",
            ServiceKind::Target,
            &[],
            &[],
        )]);
        let sp = &p.services[0];
        assert_eq!(sp.kind, ServiceKind::Target);
        assert!(!sp.kind.has_process());
        assert_eq!(sp.ready, Ready::None, "a target has no process to wait for");
    }

    /// Rule 5 of the review, and the reason the override lives in `build_plan`
    /// and not in the parser: a description read on its own must keep the
    /// operator's `ready =` verbatim, because that is the only way `zcheck` can
    /// explain why it was ignored.
    #[test]
    fn a_target_loses_a_readiness_spec_it_never_had_a_right_to() {
        let mut d = desc_with("base.target", ServiceKind::Target, &[], &[]);
        d.ready = ReadySpec::parse("notify").expect("valid");
        assert_ne!(
            d.ready.into_ready(),
            Ready::None,
            "the description keeps it"
        );
        let (p, findings) =
            build_plan_with_findings(&[d]).expect("a warning never fails the build");
        assert_eq!(p.services[0].ready, Ready::None, "the plan drops it");
        assert_eq!(codes(&findings), ["W110"]);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(
            findings[0]
                .render("base.target.conf")
                .contains("has no process"),
            "explains why: {}",
            findings[0].render("base.target.conf")
        );
    }

    #[test]
    fn a_target_that_asked_for_nothing_is_not_warned_about() {
        let mut d = desc_with("base.target", ServiceKind::Target, &[], &[]);
        d.ready = ReadySpec::parse("none").expect("valid");
        let (p, findings) = build_plan_with_findings(&[d]).expect("valid");
        assert_eq!(p.services[0].ready, Ready::None);
        assert!(
            findings.is_empty(),
            "a redundant `ready = none` is not a finding"
        );
    }

    #[test]
    fn a_numeric_user_is_frozen_verbatim() {
        // `desc.rs` refuses a symbolic `user =` outright rather than deferring
        // it, so by the time a description reaches the freeze a `None` run_as
        // means "no drop requested" and never "drop requested but unresolved".
        // Inventing a uid here would be a silent privilege bug.
        let mut numeric = desc("sshd");
        numeric.run_as = Some(RunAs { uid: 33, gid: 0 });
        let p = plan_of(vec![numeric]);
        assert_eq!(p.services[0].run_as, Some((33, 0)));

        let plain = plan_of(vec![desc("sshd")]);
        assert_eq!(plain.services[0].run_as, None);
    }

    #[test]
    fn an_unspecified_timeout_falls_back_to_the_documented_default() {
        let p = plan_of(vec![bare("sshd")]);
        assert_eq!(p.services[0].start_timeout_ms, DEFAULT_START_TIMEOUT_MS);
        assert_eq!(p.services[0].stop_timeout_ms, DEFAULT_STOP_TIMEOUT_MS);
    }

    #[test]
    fn a_zero_restart_budget_means_never_not_the_default() {
        // `restart = never` parses to `Budget::never()`. The freeze passes it
        // through untouched, so an operator who switched a service off does not
        // get it silently resurrected as "5 restarts a minute".
        let d = ServiceDesc {
            restart: RestartSpec::parse("never").expect("valid"),
            ..desc("flappy")
        };
        let p = plan_of(vec![d]);
        assert_eq!(p.services[0].restart, Restart::Never);
        assert_eq!(p.services[0].restart_budget, Budget::never());
        assert_eq!(p.services[0].restart_budget.capacity, 0);
    }

    #[test]
    fn the_restart_delay_reaches_the_frozen_budget() {
        // Freezing the wrong number is a bug that only shows up as a service
        // that ignores its own `restart-delay`, so it is pinned here.
        let mut spec = RestartSpec::parse("3 restarts per 10s").expect("valid");
        spec.budget.delay_ms = 1500;
        assert_eq!(spec.budget.capacity, 3);
        assert_eq!(spec.budget.window_ms, 10_000);
        assert_eq!(spec.budget.delay_ms, 1500);
        let p = plan_of(vec![ServiceDesc {
            restart: spec,
            ..desc("flappy")
        }]);
        assert_eq!(
            p.services[0].restart_budget,
            Budget {
                capacity: 3,
                window_ms: 10_000,
                delay_ms: 1500
            }
        );
    }

    #[test]
    fn the_default_log_path_contains_the_service_name() {
        let p = plan_of(vec![desc("sshd")]);
        match &p.services[0].log {
            LogSink::File { path, .. } => assert_eq!(path, "/var/log/zinit/sshd.log"),
            other => panic!("expected a file sink, got {other:?}"),
        }
        assert_eq!(default_log_path("a.b-c"), "/var/log/zinit/a.b-c.log");
    }

    #[test]
    fn an_explicit_log_path_is_left_alone() {
        let mut d = desc("sshd");
        d.log = LogSpec::parse("file:/tmp/sshd.log:4096/2").expect("valid");
        let p = plan_of(vec![d]);
        assert_eq!(
            p.services[0].log,
            LogSink::File {
                path: String::from("/tmp/sshd.log"),
                max_bytes: 4096,
                backups: 2
            }
        );
    }

    /// A log path is built out of the service name, so a name that escaped
    /// `services.d` would escape `/var/log/zinit` too. The two checks are not
    /// independent and this pins that they are wired in the right order.
    #[test]
    fn no_service_name_reaches_a_path_component_unvalidated() {
        assert!(matches!(
            build_plan(&[ServiceDesc {
                name: String::from("../evil"),
                ..desc("x")
            }]),
            Err(GraphError::InvalidName { .. })
        ));
        let p = plan_of(vec![desc("a")]);
        let LogSink::File { path, .. } = &p.services[0].log else {
            panic!("expected a file sink")
        };
        assert!(path.starts_with("/var/log/zinit/"), "got {path}");
        // Four separators, three of them ours, and only one component of it the
        // service name: a name that could escape its own directory would add a
        // fifth.
        assert_eq!(
            path.matches('/').count(),
            4,
            "unexpected path shape: {path}"
        );
        assert_eq!(path, "/var/log/zinit/a.log");
    }

    // ── cycles ──────────────────────────────────────────────────────────────

    /// Every cycle door. Both the error the caller gets and the shape of the
    /// path it carries, because the path is the only actionable part.
    fn cycle_error(edges: &[(&str, &[&str])]) -> GraphError {
        let g = Graph::build(
            &edges
                .iter()
                .map(|(n, d)| cycle_desc(n, d))
                .collect::<Vec<_>>(),
        )
        .expect("descriptions are linkable: a cycle is a graph, not a parse error");
        assert!(has_cycle(&g), "the boolean must agree with the error");
        match check_acyclic(&g).expect("this graph has a cycle") {
            GraphError::Cycle { path } => {
                assert!(
                    path.len() >= 3,
                    "a self-dependency is a different error: {path:?}"
                );
                assert_eq!(path.first(), path.last(), "a cycle must close on itself");
                // Every consecutive pair must be a real edge, or the report is
                // a pretty lie.
                for w in path.windows(2) {
                    let (from, to) = (w[0].as_str(), w[1].as_str());
                    let from_idx = g.index_of(from).unwrap();
                    let to_idx = g.index_of(to).unwrap();
                    assert!(
                        g.required_of(from_idx).contains(&to_idx),
                        "the report claims {from} -> {to}, which is not an edge"
                    );
                }
                // And the plan builder reports the identical thing.
                assert_eq!(
                    build_plan(
                        &edges
                            .iter()
                            .map(|(n, d)| cycle_desc(n, d))
                            .collect::<Vec<_>>()
                    )
                    .err(),
                    Some(GraphError::Cycle { path: path.clone() })
                );
                GraphError::Cycle { path }
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    fn cycle_desc(name: &str, deps: &[&str]) -> ServiceDesc {
        desc_with(name, ServiceKind::Process, deps, &[])
    }

    /// `a -> a` is a typo, not a design, and gets its own variant with its own
    /// message. It must never surface as "dependency cycle: a -> a".
    #[test]
    fn a_self_dependency_is_its_own_diagnosis() {
        match build_plan(&[cycle_desc("a", &["a"])]) {
            Err(GraphError::SelfDependency { service, .. }) => assert_eq!(service, "a"),
            other => panic!("expected SelfDependency, got {other:?}"),
        }
    }

    #[test]
    fn a_two_node_cycle_names_both_nodes() {
        match cycle_error(&[("a", &["b"]), ("b", &["a"])]) {
            GraphError::Cycle { path } => assert_eq!(path, ["a", "b", "a"]),
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn a_three_node_cycle_is_reported_in_order() {
        match cycle_error(&[("a", &["b"]), ("b", &["c"]), ("c", &["a"])]) {
            GraphError::Cycle { path } => assert_eq!(path, ["a", "b", "c", "a"]),
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn two_disjoint_cycles_are_reported_one_at_a_time() {
        match cycle_error(&[("a", &["b"]), ("b", &["a"]), ("x", &["y"]), ("y", &["x"])]) {
            GraphError::Cycle { path } => {
                // Exactly one of the two, and a real cycle: not a path that
                // merely walks out of the first into the second.
                assert!(path.len() >= 3, "too short to be a cycle: {path:?}");
                assert!(
                    path.iter().all(|n| n == "a" || n == "b"),
                    "the reported path must stay inside one cycle: {path:?}"
                );
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn a_cycle_through_a_target_is_still_a_cycle() {
        // Targets are ordering devices, so a cycle through one is a boot loop
        // waiting to happen, not a special case.
        let descs = vec![
            desc_with("a", ServiceKind::Process, &["base.target"], &[]),
            desc_with("base.target", ServiceKind::Target, &["a"], &[]),
        ];
        match build_plan(&descs) {
            Err(GraphError::Cycle { path }) => assert_eq!(path, ["a", "base.target", "a"]),
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn a_cycle_reached_through_an_acyclic_prefix_still_closes() {
        // `root -> a`, and `a` loops with `b`. The reported path must be the
        // loop, not `root -> a -> b -> a`.
        match cycle_error(&[("root", &["a"]), ("a", &["b"]), ("b", &["a"])]) {
            GraphError::Cycle { path } => {
                assert_eq!(path, ["a", "b", "a"]);
                assert!(
                    !path.iter().any(|n| n == "root"),
                    "the prefix must be trimmed"
                );
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn a_long_cycle_is_reported_in_full() {
        const N: usize = 40;
        let descs: Vec<ServiceDesc> = (0..N)
            .map(|i| {
                let mut d = desc(&format!("c{i:02}"));
                d.depends_required = vec![format!("c{:02}", (i + 1) % N)];
                d
            })
            .collect();
        match build_plan(&descs) {
            Err(GraphError::Cycle { path }) => {
                assert_eq!(path.len(), N + 1, "the whole loop must be reported");
                assert_eq!(path.first(), path.last());
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn an_acyclic_graph_passes_the_check() {
        let g = Graph::build(&[
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with("b", ServiceKind::Process, &["a"], &[]),
        ])
        .expect("valid graph");
        assert_eq!(check_acyclic(&g), None);
        assert!(!has_cycle(&g));
        assert!(topological_order(&g).is_ok());
    }

    /// A cycle is an `Err`, never a panic. This crate is built
    /// `panic = "abort"`: a supervisor that is a child of PID 1 and aborts on a
    /// bad `.conf` takes the machine down instead of reporting the file that
    /// caused it.
    #[test]
    fn a_cycle_is_an_error_and_not_a_panic() {
        for descs in [
            vec![cycle_desc("a", &["b"]), cycle_desc("b", &["a"])],
            vec![
                cycle_desc("a", &["b"]),
                cycle_desc("b", &["c"]),
                cycle_desc("c", &["a"]),
            ],
        ] {
            let err = build_plan(&descs).expect_err("a cycle cannot be frozen");
            assert!(matches!(err, GraphError::Cycle { .. }));
            // Same through the delegating door.
            let g = Graph::build(&descs).expect("linkable");
            assert!(matches!(g.to_plan(), Err(GraphError::Cycle { .. })));
        }
    }

    // ── determinism ─────────────────────────────────────────────────────────

    /// Every rotation of the input, so a passing check cannot be luck about a
    /// particular arrival order.
    fn rotations(descs: Vec<ServiceDesc>) -> Vec<Vec<ServiceDesc>> {
        (0..descs.len())
            .map(|k| {
                let mut v = descs.clone();
                v.rotate_left(k);
                v
            })
            .collect()
    }

    #[test]
    fn the_plan_does_not_depend_on_the_order_of_the_descriptions() {
        let base = vec![
            desc_with("base.target", ServiceKind::Target, &[], &[]),
            desc_with("alpha", ServiceKind::Process, &["base.target"], &["ghost"]),
            desc_with("beta", ServiceKind::Process, &["alpha"], &[]),
            desc_with("gamma", ServiceKind::Script, &["alpha", "base.target"], &[]),
            desc_with("delta", ServiceKind::Console, &[], &[]),
        ];
        let reference = plan_of(base.clone());
        for rotated in rotations(base) {
            assert_eq!(
                plan_of(rotated),
                reference,
                "the frozen plan must be input-order agnostic"
            );
        }
    }

    #[test]
    fn a_reversed_chain_gives_the_same_plan_as_a_forward_one() {
        let forward = plan_of(vec![
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with("b", ServiceKind::Process, &["a"], &[]),
            desc_with("c", ServiceKind::Process, &["b"], &[]),
        ]);
        let backward = plan_of(vec![
            desc_with("c", ServiceKind::Process, &["b"], &[]),
            desc_with("b", ServiceKind::Process, &["a"], &[]),
            desc_with("a", ServiceKind::Process, &[], &[]),
        ]);
        assert_eq!(forward, backward);
    }

    #[test]
    fn findings_are_in_the_same_order_whatever_the_input_order() {
        let base = vec![
            desc_with("a", ServiceKind::Process, &[], &["ghost-a"]),
            desc_with("b", ServiceKind::Process, &[], &["ghost-b"]),
            desc_with("c", ServiceKind::Process, &[], &["ghost-c"]),
        ];
        let reference = build_plan_with_findings(&base).expect("valid").1;
        assert_eq!(reference.len(), 3);
        for rotated in rotations(base) {
            let other = build_plan_with_findings(&rotated).expect("valid").1;
            assert_eq!(other, reference, "the report must be reproducible too");
        }
    }

    #[test]
    fn the_heap_picks_the_smallest_index_first() {
        let mut h: Vec<Idx> = vec![9, 3, 7, 1, 5];
        heapify(&mut h);
        let mut out = Vec::new();
        while let Some(v) = heap_pop(&mut h) {
            out.push(v);
        }
        assert_eq!(out, [1, 3, 5, 7, 9]);

        let mut h2: Vec<Idx> = Vec::new();
        for v in [4usize, 8, 2, 6, 0, 3] {
            heap_push(&mut h2, v);
        }
        let mut out2 = Vec::new();
        while let Some(v) = heap_pop(&mut h2) {
            out2.push(v);
        }
        assert_eq!(out2, [0, 2, 3, 4, 6, 8]);
        assert_eq!(heap_pop(&mut Vec::new()), None);
    }

    /// The reverse CSR the sort walks, checked against the obvious O(V^2)
    /// formulation. They have to agree, because one of them is only ever
    /// inspected by tests.
    #[test]
    fn the_reverse_index_is_the_exact_inverse_of_the_edges() {
        let g = Graph::build(&[
            desc_with("a", ServiceKind::Target, &[], &[]),
            desc_with("b", ServiceKind::Process, &["a"], &[]),
            desc_with("c", ServiceKind::Process, &["a", "b"], &[]),
            desc_with("d", ServiceKind::Process, &["c"], &["a"]),
        ])
        .expect("valid graph");
        let (offsets, to) = reverse_edges(&g);
        assert_eq!(offsets.len(), g.len() + 1);
        for i in 0..g.len() {
            let via_csr: Vec<Idx> = to[offsets[i]..offsets[i + 1]].to_vec();
            let mut via_scan = g.dependents_of(i);
            via_scan.sort_unstable();
            assert_eq!(via_csr, via_scan, "row {i}");
        }
    }

    // ── delegation ──────────────────────────────────────────────────────────

    #[test]
    fn to_plan_and_build_plan_agree() {
        let descs = vec![
            desc_with("a", ServiceKind::Process, &[], &[]),
            desc_with("b", ServiceKind::Process, &["a"], &["nope"]),
        ];
        let via_build = build_plan(&descs).expect("valid plan");
        let via_graph = Graph::build(&descs)
            .expect("valid graph")
            .to_plan()
            .expect("valid plan");
        assert_eq!(via_build, via_graph, "one implementation, two doors");
    }

    #[test]
    fn findings_do_not_change_the_plan() {
        let descs = vec![desc_with("app", ServiceKind::Process, &[], &["redis"])];
        let (with_findings, findings) = build_plan_with_findings(&descs).expect("valid plan");
        assert_eq!(with_findings, build_plan(&descs).expect("valid plan"));
        assert_eq!(codes(&findings), ["W100"]);
        assert!(findings[0].render("app.conf").contains("app"));
    }

    #[test]
    fn a_build_error_never_yields_a_plan() {
        let descs = vec![desc_with("a", ServiceKind::Process, &["ghost"], &[])];
        assert!(matches!(
            build_plan(&descs),
            Err(GraphError::UnknownDependency { .. })
        ));
        assert!(build_plan_with_findings(&descs).is_err());
    }

    /// Rule 6: at the cap it works, one over it is an error naming the count -
    /// never an out-of-bounds index and never a panic.
    #[test]
    fn the_service_cap_is_enforced_without_panicking() {
        let at: Vec<ServiceDesc> = (0..MAX_SERVICES)
            .map(|i| {
                let mut d = desc("s");
                d.name = format!("s{i:05}");
                d
            })
            .collect();
        let p = build_plan(&at).expect("the cap is inclusive");
        assert_eq!(p.services.len(), MAX_SERVICES);
        assert_eq!(p.order_up.len(), MAX_SERVICES);

        let over: Vec<ServiceDesc> = (0..=MAX_SERVICES)
            .map(|i| {
                let mut d = desc("s");
                d.name = format!("s{i:05}");
                d
            })
            .collect();
        assert_eq!(
            build_plan(&over).err(),
            Some(GraphError::TooManyServices {
                count: MAX_SERVICES + 1
            })
        );
    }

    // ── the identity trapdoor, enforced where it can be ───────────────────

    /// The other half of the rule tested in `parser.rs`.
    ///
    /// `zconfig` keeps an unresolved `user =` name so the parser can accept
    /// the spelling an operator actually writes. This is where that debt is
    /// collected: **no plan can exist while an identity is still a string**,
    /// because every consumer of `ServicePlan::run_as` would read `None` and
    /// run the service as root.
    ///
    /// An earlier version documented this in a doc-comment and enforced
    /// nothing. The comment was the enforcement, and services ran as root.
    #[test]
    fn an_unresolved_identity_cannot_become_a_plan() {
        let mut d = ServiceDesc::new("mysql");
        d.command = String::from("/usr/sbin/mysqld");
        d.unresolved_run_as = Some(String::from("mysql"));

        match build_plan(std::slice::from_ref(&d)) {
            Err(GraphError::UnresolvedIdentity { service, name }) => {
                assert_eq!(service, "mysql");
                assert_eq!(name, "mysql");
            }
            Err(other) => panic!("expected UnresolvedIdentity, got {other:?}"),
            Ok(p) => panic!(
                "a plan was built with an unresolved identity; run_as is {:?}, which means root",
                p.services[0].run_as
            ),
        }

        // And the way out is the runtime resolving it, not a flag.
        let resolved = d
            .with_resolved_identity((108, 108))
            .expect("the description does have an unresolved name");
        assert!(!resolved.identity_is_unresolved());
        let p = build_plan(&[resolved]).expect("now it is resolvable");
        assert_eq!(p.services[0].run_as, Some((108, 108)));
    }

    /// A description that never asked to drop privileges must not be blocked:
    /// the trapdoor closes on an unresolved name, never on the absence of one.
    #[test]
    fn no_identity_is_not_an_unresolved_identity() {
        let mut d = ServiceDesc::new("plain");
        d.command = String::from("/usr/bin/plain");
        let p = build_plan(&[d]).expect("a service with no `user` line is fine");
        assert_eq!(p.services[0].run_as, None);
    }
}
