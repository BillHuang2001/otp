//! Rust ERTS runtime binary: boot sequence, preloaded modules, CLI flags and `system_info`.
//!
//! Implements compatibility contracts Part I §A.6 (CLI/configuration) and §A.7
//! (preloaded modules, boot scripts, `erts_internal`/`prim_*`), and owns the
//! runtime glue described in `../../CONTEXT.md`. Placeholder entry point until the
//! child architects land the real boot sequence.

fn main() {
    println!(
        "beam_rt {} — Rust ERTS rewrite (target: OTP 30.0-rc0 / erts-17.1)",
        env!("CARGO_PKG_VERSION"),
    );
}
