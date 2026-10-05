//! # `beam_rt` — Rust ERTS runtime binary (boot · CLI · `system_info` · runtime glue)
//!
//! The top of the workspace: the `beam_rt` executable, the boot sequence, the preloaded-module
//! registry skeleton, `system_info/1`, the CLI flag surface (contracts Part I §A.6/A.7/A.8) and the
//! concrete [`ExecutionContext`](beam_engine::ExecutionContext) glue that turns the Stage-1
//! interpreter into a runnable runtime. See `CONTEXT.md` for the frozen API surface.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod boot;
pub mod cli;
pub mod config;
pub mod context;
pub mod error;
pub mod eval;
pub mod format;
pub mod info;
pub mod runtime;

// NOTE: root re-exports and `prelude` are added by the implementation pass once the
// module items exist.
