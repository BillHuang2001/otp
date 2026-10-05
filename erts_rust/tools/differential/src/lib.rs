//! The differential-test harness: the **compatibility gate** of the Rust ERTS rewrite.
//!
//! For a given Erlang expression (or corpus case) the harness runs the **real system
//! ERTS** (the *oracle*) and the **Rust runtime** (`beam_rt`) with **identical argv** as a
//! drop-in `erl` (compatibility contract A.6), then compares the **observable results**:
//! exact stdout bytes, exit status, stderr, and the case's own golden expectation.
//!
//! This crate is a pure **driver/observer**: it never modifies a runtime and never
//! inspects its internals — the only contract with `beam_rt` is its documented external
//! CLI. It depends on no workspace crate internals (only `serde`/`serde_json`) and
//! contains no `unsafe` code.
//!
//! # Example
//!
//! ```
//! use differential::corpus::{find_case, probe_expr};
//! use differential::Probe;
//!
//! // A case is a plain Erlang expression plus how to probe it and what the oracle must print.
//! let case = find_case("etf/term_to_binary_tuple").expect("built-in case");
//! assert_eq!(case.expr, "{ok,42}");
//!
//! // The probe wrapper makes a BEAM exception observable (erl -noshell prints nothing).
//! let probe = probe_expr("length([a,b,c])", Probe::Value);
//! assert!(probe.starts_with("try (length([a,b,c])) of V"));
//! assert!(probe.ends_with("halt(1) end."));
//! ```
//!
//! Running is what the binary does (`differential suite`, `differential run <case-id>`,
//! `differential compare <expr>`), and it needs an oracle on the machine:
//!
//! ```no_run
//! use differential::{corpus, harness, report};
//!
//! let case = corpus::find_case("etf/term_to_binary_tuple").expect("case");
//! let outcome = harness::run_case(case).expect("oracle must be resolvable");
//! // The Rust side is `unsupported` while `beam_rt` is a stub — never "agree".
//! let json = report::RunReport::from_case("run", &outcome)
//!     .to_json_string()
//!     .unwrap();
//! assert!(json.contains("\"schema\": \"differential/1\""));
//! ```
//!
//! # Exit codes
//!
//! * `0` — every executed case agreed (unsupported/skipped cases are reported, not fatal)
//! * `1` — at least one mismatch (including oracle-vs-its-own-golden corpus drift)
//! * `2` — usage, environment or IO error (see [`Error::exit_code`])

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod cli;
pub mod compare;
pub mod corpus;
pub mod error;
pub mod harness;
pub mod report;
pub mod runtime;

pub use compare::{compare_observations, FieldDiff, GoldenCheck, SideResult, Verdict};
pub use corpus::{find_case, probe_expr, Case, Contract, Golden, Probe};
pub use error::{Error, Result};
pub use harness::{
    compare_expr, run_case, run_suite, CaseOutcome, RuntimeContext, SuiteOutcome,
};
pub use report::{CaseReport, ListReport, RunReport, Summary};
pub use runtime::{
    eval_argv, resolve_oracle, resolve_rust, Observation, RuntimeId, RuntimeSource, RuntimeSpec,
};
