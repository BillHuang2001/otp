//! Shared support for the `differential` integration tests.
//!
//! Files under `tests/common/` are **not** compiled as their own test binary — each
//! integration test pulls this module in with `mod common;`.
//!
//! The one thing they need in common is a way to point the harness at the **freshly built**
//! `beam_rt` instead of the workspace default. [`differential::runtime::resolve_rust`]
//! derives `<workspace>/target/{release,debug}/beam_rt` and deliberately prefers the
//! `release` profile — which is right for the real CLI and for `gate.sh`, but wrong under
//! `cargo test`: only the **debug** artifacts are (re)built there, so a stale
//! `target/release/beam_rt` could shadow the fresh build and make a test observe a binary
//! that nobody just compiled (e.g. a stale release printing `ok ok` where the fresh debug
//! build prints `ok true`).
//!
//! Cargo builds the `differential` test binary and `beam_rt` into the *same* profile
//! directory, so the fresh runtime is simply the sibling of `CARGO_BIN_EXE_differential`.
//! `CARGO_BIN_EXE_beam_rt` would be the direct way to get it, but Cargo only defines that
//! for binaries of the *package under test* (not for another workspace package's binary), so
//! it is not available here; the sibling lookup is exact, not heuristic — it names the same
//! profile directory cargo just built both binaries into.

#![allow(dead_code)] // Each test binary includes this module; not every one uses every helper.

use std::path::{Path, PathBuf};

/// The freshly built `beam_rt` sitting next to this crate's test binary, if it exists.
///
/// Returned only when the file actually exists. The name uses
/// [`std::env::consts::EXE_SUFFIX`] so it stays correct if the tests are built on Windows.
/// `None` means the runtime was not built alongside this test binary (the caller must then
/// report the Rust side honestly as `unsupported`, never as a fabricated pass).
pub fn fresh_rust_runtime() -> Option<PathBuf> {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_differential"));
    let dir = exe.parent()?;
    let candidate = dir.join(format!("beam_rt{}", std::env::consts::EXE_SUFFIX));
    candidate.is_file().then_some(candidate)
}

/// A `RuntimeSpec` for the fresh runtime, or `None` when it was not built.
///
/// Resolved through [`differential::runtime::resolve_rust_from`] with the fresh path passed
/// as the `$BEAM_RT` value, so the spec reports `RuntimeSource::Env("$BEAM_RT")` — exactly as
/// a real injection would — and can never fall back to the workspace default (a stale
/// `release` build). The resolution cannot fail because [`fresh_rust_runtime`] only returns
/// an existing file.
pub fn fresh_rust_spec() -> Option<differential::runtime::RuntimeSpec> {
    let path = fresh_rust_runtime()?;
    differential::runtime::resolve_rust_from(
        Some(path.into_os_string()),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .expect("the fresh beam_rt exists, so $BEAM_RT resolution cannot fail")
}
