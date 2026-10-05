//! # `beam_rt` — Rust ERTS runtime binary (boot · CLI · `system_info` · runtime glue)
//!
//! The top of the workspace: the `beam_rt` executable, the boot sequence, the preloaded-module
//! registry skeleton, `system_info/1`, the CLI flag surface (contracts Part I §A.6/A.7/A.8) and the
//! concrete [`ExecutionContext`](beam_engine::ExecutionContext) glue that turns the Stage-1
//! interpreter into a runnable runtime. See `CONTEXT.md` for the frozen API surface.
//!
//! # Layout
//!
//! * [`runtime`] — the [`Runtime`] façade: boot-wiring, the module cache and the **vertical slice**
//!   ([`Runtime::run_function`]/[`Runtime::run_module`]) that loads a real `.beam` and runs it.
//! * [`context`] — [`RuntimeContext`], the concrete `ExecutionContext` over the real BIF registry.
//! * [`config`] — the parsed [`RuntimeConfig`] and the selected [`Action`].
//! * [`cli`] — the A.6 parser ([`cli::parse`]), the usage text and the [`cli::run`] driver.
//! * [`boot`] — the preloaded-module registry skeleton (A.7), the atom seed and the version banner.
//! * [`info`] — `system_info/1` (A.8) as a heap-free [`InfoValue`] plus its materialization.
//! * [`eval`] — the documented `-eval` integer subset, evaluated through the `erlang` BIFs.
//! * [`format`] — term printing, which drives the registered `io:format/2` `~p` printer.
//!
//! # Contracts
//!
//! * **Errors are return-based** (workspace `CONTEXT.md` §3): every fallible API returns
//!   [`beam_common::Result`]; there is no crate-local error enum and nothing is swallowed.
//! * **`unsafe`-free**: the crate is `#![deny(unsafe_code)]`. The glue is built only on the safe
//!   peripheries of the lower crates.
//! * **Call down, never duplicate**: BIFs come from `beam_bifs`, terms/heaps from `beam_term`/
//!   `beam_alloc`, pids from `beam_process`, modules from `beam_loader` and execution from
//!   `beam_engine`. The `~p` printer is the registered `io:format/2` BIF, not a second printer.
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

pub use crate::boot::{boot_banner, PreloadedModules};
pub use crate::cli::{parse, run, EXIT_OK, EXIT_RUNTIME, EXIT_USAGE, USAGE};
pub use crate::config::{Action, EmulatorConfig, RuntimeConfig, DEFAULT_HEAP_WORDS};
pub use crate::context::RuntimeContext;
pub use crate::eval::eval;
pub use crate::format::term_to_string;
pub use crate::info::{system_info_value, InfoValue, SUPPORTED_KEYS};
pub use crate::runtime::Runtime;

/// The conventional glob-import surface for downstream crates.
///
/// `use beam_rt::prelude::*;` brings the runtime façade ([`Runtime`]), its context
/// ([`RuntimeContext`]), the configuration ([`RuntimeConfig`]/[`Action`]) and the CLI entry points
/// into scope.
pub mod prelude {
    pub use crate::boot::{boot_banner, PreloadedModules};
    pub use crate::cli::{parse, run, USAGE};
    pub use crate::config::{Action, EmulatorConfig, RuntimeConfig};
    pub use crate::context::RuntimeContext;
    pub use crate::eval::eval;
    pub use crate::format::term_to_string;
    pub use crate::info::{system_info_value, InfoValue};
    pub use crate::runtime::Runtime;
}
