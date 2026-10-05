//! The workspace-wide `unsafe` marker convention (documentation only).
//!
//! The Rust ERTS rewrite is memory-safe by construction: `unsafe` is confined to the
//! few modules whose job *is* to be unsafe — the term heap, the garbage collector,
//! the JIT code emitters and the NIF/driver FFI boundary. Everywhere else the crate
//! forbids it outright (`#![forbid(unsafe_code)]`, as in this crate).
//!
//! The convention, binding on every crate of the `erts_rust` workspace:
//!
//! 1. **Confined.** `unsafe` may only appear in a module that is explicitly marked as
//!    an unsafe module (its module docs say so), e.g. the term heap, GC, JIT and FFI
//!    modules. Everything else uses `#![forbid(unsafe_code)]`.
//! 2. **Rationale.** Every `unsafe` block and every `unsafe fn` carries a preceding
//!    `// SAFETY: <invariant/rationale>` comment that states *why* the operation is
//!    sound — the invariant that is relied upon and who maintains it.
//! 3. **Evidence.** Any `unsafe` gains a Miri run, a fuzz target or a differential
//!    test against the legacy C/C++ ERTS where feasible; the test is part of the
//!    change, not an optional extra.
//!
//! `beam_common` itself contains no `unsafe` code: it is the dependency sink that
//! defines the convention for the crates that must sometimes break it.
