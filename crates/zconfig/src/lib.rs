//! Service description parsing, dependency graph construction and the frozen
//! [`Plan`](zcore::Plan).
//!
//! `zconfig` turns text into a `zcore::Plan`. It never opens a file and never
//! reads a clock: the caller supplies the text. That is what lets:
//!
//! * `zcheck` validate a description with no filesystem access,
//! * `zctl` parse an inline description supplied on stdin,
//! * the test suite drive thousands of cases with plain `&str` literals.
//!
//! It is `no_std` and has no dependencies beyond `zcore`, so the parsing,
//! graph and diagnostics are testable with the same rigor as the core.
//!
//! # Layering
//!
//! ```text
//!   &str  ──parse──▶  Vec<ServiceDesc>  ──link──▶  Graph  ──order──▶  Plan
//!   (unresolved)                          (edges)            (frozen, indices)
//! ```
//!
//! * [`ServiceDesc`] is unresolved: `depends` holds *names*.
//! * [`Graph`] resolves names to edges and reports unknown/duplicate names.
//! * [`Plan`](zcore::Plan) is frozen: `Idx` everywhere, topologically ordered, reverse edges
//!   precomputed, every cycle rejected.
//!
//! Nothing downstream of `Plan` ever sees a name or an unresolved edge.

#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod desc;
pub mod diagnostic;
pub mod graph;
pub mod parser;
pub mod plan;
pub mod value;

pub use desc::ServiceDesc;
pub use diagnostic::{Diagnostic, DiagnosticBag, Severity, Span};
pub use graph::{Graph, GraphError};
pub use parser::{ParseError, parse_service};
pub use plan::build_plan;
pub use value::{
    LogSpec, ReadySpec, RestartSpec, RunAs, ValueError, parse_capability, parse_duration,
    parse_listen,
};
