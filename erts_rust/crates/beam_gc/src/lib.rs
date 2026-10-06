#![doc = "Per-process generational semi-space copying GC, MSO sweep and heap sizing."]

//! Per-process garbage collector of the Rust ERTS rewrite (Stage 4).
//!
//! Owns the collector that the runtime drives at safepoints: a **generational
//! semi-space Cheney copying** collector over the per-process heap, **barrier-free
//! by construction** (Erlang terms are immutable, so a mature object can never refer
//! to a later-allocated young object and the mutator pays no per-store overhead),
//! the off-heap **refcounted large-object (MSO)** interface, and the process
//! **heap-size policy**. The root set is the flat contract of the crate layer:
//! `x_reg_array[0..Live)` plus the whole stack, with **no frame liveness maps**.
//! MMTk and Boehm are rejected; GenImmix is a measurement-gated mature-generation
//! option only. [`GenImmixCollector`] is the integrated, config-selectable form of that
//! option (copying nursery + mark-region mature), and [`AnyCollector`] dispatches between
//! it and the default copying [`Collector`] on the configured
//! [`MatureKind`](config::MatureKind).
//!
//! Implements the GC half of Part II §D and the Stage-4 row of Part II §H; the
//! legacy `erts/emulator/beam/erl_gc.c` + `copy.c` are the ground-truth behaviour.
//!
//! `unsafe` is confined to `copy.rs` and `immix_collector.rs` (the word-level copier and
//! the promotion driver), `immix/` (the mark-region space's word-level mark/evacuation
//! walks), the raw-pointer range tests in `space.rs` and the two narrow `NonNull`-deref /
//! `Box::from_raw` blocks in `mso.rs`; `policy.rs`, `stats.rs` and the collector driving
//! logic are safe Rust.

pub mod any_collector;
pub mod collector;
pub mod config;
pub mod copy;
pub mod immix;
pub mod immix_collector;
mod error; // pub(crate) helpers only — NOT public
pub mod mso;
pub mod policy;
pub mod space;
pub mod stats;

pub use crate::any_collector::*;
pub use crate::collector::*;
pub use crate::config::*;
pub use crate::copy::*;
pub use crate::immix::*;
pub use crate::immix_collector::*;
pub use crate::mso::*;
pub use crate::policy::*;
pub use crate::space::*;
pub use crate::stats::*;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::any_collector::*;
    pub use crate::collector::*;
    pub use crate::config::*;
    pub use crate::copy::*;
    pub use crate::immix::*;
    pub use crate::immix_collector::*;
    pub use crate::mso::*;
    pub use crate::policy::*;
    pub use crate::space::*;
    pub use crate::stats::*;
}
