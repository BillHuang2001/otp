//! Differential-test harness: the compatibility gate.
//!
//! Runs a module under the real system ERTS (the oracle, `ERTS_ORACLE`) and under
//! the Rust runtime, then compares observable results — stdout, `term_to_binary`,
//! exception reason and `system_info` (Part I §C and the test strategy in
//! `../../CONTEXT.md` §6). No behavior change ships without a differential case.
//! Placeholder entry point until the harness child architect lands the runner.

fn main() {
    println!(
        "differential {} — Rust ERTS vs system ERTS compatibility gate (placeholder)",
        env!("CARGO_PKG_VERSION"),
    );
}
