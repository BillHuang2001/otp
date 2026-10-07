//! `erts_alloc:info/0` at the executable boundary.
//!
//! The BIF is served by the runtime-native floor, so it must be reachable from a plain
//! `-eval` through the built binary — not just from the library API the floor's unit tests
//! drive. The report's *values* are this runtime's own live counters, so this test pins the
//! observable **shape** (the alloc-util allocator count and names) rather than any number.
//!
//! The oracle has no `erts_alloc` module at all (see `native/alloc.rs` for the pinned
//! evidence and the test that keeps it honest), so there is deliberately **no** oracle
//! comparison here: this BIF is a documented superset, and its shape is the one the oracle
//! uses for `erlang:system_info(allocator)`'s settings list.

use std::process::{Command, Output};

/// Runs the built `beam_rt` binary with `args`.
fn beam_rt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// Runs `-eval Expr` and returns its stdout.
fn eval(expr: &str) -> String {
    let output = beam_rt(&["-noshell", "-eval", expr]);
    assert!(
        output.status.success(),
        "beam_rt -eval {expr} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn eval_sees_the_ten_alloc_util_sections() {
    // `engine.A.8.erts_alloc_info` — the report is one section per alloc-util allocator.
    assert_eq!(
        eval("io:format(\"~p~n\", [length(erts_alloc:info())]), halt()."),
        "10\n"
    );
}

#[test]
fn the_first_section_is_the_first_alloc_util_allocator() {
    // `temp_alloc` heads `beam_alloc::registry::alloc_util_allocators()` — the same order
    // `erlang:system_info(alloc_util_allocators)` reports.
    assert_eq!(
        eval(
            "io:format(\"~p~n\", [element(1, hd(erts_alloc:info()))]), halt()."
        ),
        "temp_alloc\n"
    );
}

#[test]
fn every_section_carries_its_counters_and_policy() {
    // Each section is a `{Name, Items}` proplist with the thirteen counter labels, the
    // `+M<alloc><opt>` policy items and the per-type rows, so `blocks` and `sbct` are
    // always among its keys.
    assert_eq!(
        eval(
            "S = hd(erts_alloc:info()), K = [X || {X, _} <- element(2, S)], \
             io:format(\"~p~n\", [lists:member(blocks, K) andalso lists:member(sbct, K)]), \
             halt()."
        ),
        "true\n"
    );
}

#[test]
fn an_arity_the_floor_does_not_serve_is_still_undef() {
    // The floor only claims `info/0`: the module does not exist in real ERTS, so `info/1`
    // must stay the oracle's bare `error:undef` (exit 1), never a fabricated answer.
    let output = beam_rt(&["-noshell", "-eval", "erts_alloc:info(std_alloc)."]);
    assert_eq!(output.status.code(), Some(1), "an undef call must exit 1");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("undef"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
