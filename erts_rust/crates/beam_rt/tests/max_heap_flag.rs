// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! `erlang:process_flag(max_heap_size, …)` at the executable boundary (contracts A.6/A.8),
//! cross-checked against the system `erl` oracle.
//!
//! # What is pinned
//!
//! The flag's **observable** behaviour, exactly as a user sees it from `beam_rt -eval` and from
//! `erl -eval`:
//!
//! 1. the **previous value** `process_flag/2` answers on a **fresh** process — the C's four-key
//!    flag map `#{error_logger => …, include_shared_binaries => …, kill => …, size => …}` whose
//!    `size` is `0` (unlimited) and whose three flags are the emulator defaults
//!    (`true`/`false`/`true`),
//! 2. that the very same map is what `process_info(self(), max_heap_size)` reports, **after** a
//!    set (the set is recorded, not just answered),
//! 3. that a second set answers the **first** setting as its previous value, and that the map
//!    form's `kill`/`error_logger`/`include_shared_binaries` entries override the defaults while
//!    an absent entry keeps them,
//! 4. the two accepted **value shapes** — a bare non-negative integer and `#{size => 0}` ("no
//!    upper bound") — round-tripping through `process_info/2`,
//! 5. the `badarg` matrix (an empty map, a map without `size`, a `size` below the process's own
//!    `min_heap_size` floor, a negative integer, a non-integer) **leaves the previous setting
//!    untouched** — a rejected value must not be half-applied,
//! 6. the flag is **per-process**: a child spawned *after* the caller set one is born with the
//!    default map, not with the caller's.
//!
//! # The bound itself is **not** enforced here (documented residual)
//!
//! Neither `beam_process::Process` nor `beam_gc::AnyCollector` exposes a policy setter, so the
//! recorded bound is not acted upon: this runtime never kills a process at its limit and never
//! emits the `error_logger` report. That is the documented residual (`M1_GAPS.md` *Heap
//! switches*), and it is why every probe below picks a `size` far beyond the eval process's
//! reach. It is not cosmetic: the **oracle does** enforce the bound, and a small one kills its
//! own `-eval` process mid-probe — measured, `process_flag(max_heap_size, #{size => 1000})`
//! followed by an `io:format` exits **1** with no output at all, because the process exceeds
//! 1000 words while building the call. A test that exercised the bound would therefore compare
//! a successful run against a killed one; the kill is not what this suite asserts.
//!
//! # Why the probes never print the flag map as a term
//!
//! A map's printed key order is its **internal** (hash-derived) order, and that order is
//! runtime-specific by construction — the oracle prints the literal `#{a => 1, b => 2, c => 3}`
//! as `#{c => 3,a => 1,b => 2}`. Comparing printed maps would therefore compare hash layouts
//! rather than the flag. Every probe renders the four entries through an **explicit key list**
//! in the C's own construction order (`erts_max_heap_size_map`, `erl_gc.c:3807-3822`:
//! `error_logger`, `include_shared_binaries`, `kill`, `size`) and appends `maps:size/1`, so the
//! comparison pins the key **set**, the key count and each `{Key, Value}` pair while staying
//! byte-identical across runtimes.
//!
//! Two further harness details are load-bearing:
//!
//! * **One term per `io:format` call.** The registered `io:format/2` in this runtime raises
//!   `badarg` with **two** `~p`/`~w` arguments, so a probe prints one term at a time.
//! * **A distinct catch variable per `try`.** `erl_eval` rejects reusing one catch variable
//!   name across two `try` expressions (`Error! Failed to eval`, exit 1) while this runtime's
//!   `-eval` accepts it, so the `badarg` probe uses `B1`…`B5` — the shared source text has to
//!   be valid on both sides.
//!
//! # How the two sides are compared
//!
//! Both runtimes are driven with the **same `-eval` source text**: the Rust side is
//! `beam_rt -noshell -eval '<expr>'` through the built binary (`CARGO_BIN_EXE_beam_rt`), the
//! oracle is `support::oracle_eval` (`erl -noshell -eval '<expr>' -s init stop`). Trimmed
//! **stdout** is compared byte for byte, together with the exit code (0) and a silent stderr.
//! Every probe child is polled at [`support::PROBE_TIMEOUT`] and reaped, so a blocked probe can
//! never hang the suite, and `erl` is discovered by **filesystem only** — a missing toolchain is
//! a **loud** skip ([`support::require_oracle`]), never a silent pass.

mod support;

use std::path::Path;
use std::process::Output;

/// The executable under test, exactly as the sibling contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The banner the loud skip prints.
const BANNER: &str = "max_heap_flag";

/// The four flag keys, in the C's own construction order (`erts_max_heap_size_map`,
/// `erl_gc.c:3807-3822`, verified against OTP 29.1.1 / `erts-17.1`).
const FLAG_KEYS: &str = "error_logger, include_shared_binaries, kill, size";

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    /// The child's exit code (`-1` when it was signalled).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Trimmed stderr.
    err: String,
}

/// Runs `program args` under the shared bounded runner and fails loudly when it could not run.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// `[{K, maps:get(K, <map_expr>)} || K <- [<the four flag keys>]]` — the flag map's entries in
/// the C's own key order, independent of the map's internal (hash-derived) order.
fn flag_pairs(map_expr: &str) -> String {
    format!("[{{K, maps:get(K, {map_expr})}} || K <- [{FLAG_KEYS}]]")
}

/// The same entries plus `maps:size/1`, so a missing or an extra key cannot hide behind the
/// explicit key list.
fn flag_dump(map_expr: &str) -> String {
    format!("{} ++ [maps:size({map_expr})]", flag_pairs(map_expr))
}

/// `io:format("~w~n", [<term>])` — **one** term per call (the registered printer refuses two).
fn print(term: &str) -> String {
    format!(r#"io:format("~w~n", [{term}])"#)
}

/// The caller's own `max_heap_size` map, as `process_info(self(), max_heap_size)` renders it.
const SELF_MAP: &str = "element(2, process_info(self(), max_heap_size))";

/// The pinned probes: `(label, the expression both runtimes evaluate, the exact stdout)`.
///
/// Each `size` is orders of magnitude beyond what the eval process allocates, so the **oracle's**
/// enforcement of the bound never fires (see the module docs) and the comparison stays
/// apples-to-apples with this runtime, which does not enforce it.
fn probes() -> [(&'static str, String, &'static str); 5] {
    [
        (
            "a fresh process's previous value, and the item after the set",
            format!(
                "P = process_flag(max_heap_size, #{{size => 1000000}}), {}, \
                 I = {SELF_MAP}, {}, ok.",
                print(&flag_dump("P")),
                print(&flag_dump("I")),
            ),
            "[{error_logger,true},{include_shared_binaries,false},{kill,true},{size,0},4]\n\
             [{error_logger,true},{include_shared_binaries,false},{kill,true},{size,1000000},4]",
        ),
        (
            "a second set answers the first setting, and its flags override the defaults",
            format!(
                "_ = process_flag(max_heap_size, #{{size => 2000000, kill => false, \
                 error_logger => false, include_shared_binaries => true}}), \
                 Prev = process_flag(max_heap_size, #{{size => 3000000}}), {}, \
                 I = {SELF_MAP}, {}, ok.",
                print(&flag_dump("Prev")),
                print(&flag_dump("I")),
            ),
            "[{error_logger,false},{include_shared_binaries,true},{kill,false},{size,2000000},4]\n\
             [{error_logger,true},{include_shared_binaries,false},{kill,true},{size,3000000},4]",
        ),
        (
            "a bare integer and `#{size => 0}` (no upper bound) are both accepted",
            format!(
                "M1 = process_flag(max_heap_size, 5000000), {}, {}, \
                 M2 = process_flag(max_heap_size, #{{size => 0}}), {}, {}, ok.",
                print(&flag_dump("M1")),
                print(&format!("maps:get(size, {SELF_MAP})")),
                print(&format!("maps:get(size, M2)")),
                print(&format!("maps:get(size, {SELF_MAP})")),
            ),
            "[{error_logger,true},{include_shared_binaries,false},{kill,true},{size,0},4]\n\
             5000000\n\
             5000000\n\
             0",
        ),
        (
            "every rejected value is a badarg and leaves the setting untouched",
            format!(
                "_ = process_flag(max_heap_size, #{{size => 2000000}}), \
                 R1 = try process_flag(max_heap_size, #{{}}) of _ -> accepted \
                      catch error:B1 -> B1 end, \
                 R2 = try process_flag(max_heap_size, #{{kill => true}}) of _ -> accepted \
                      catch error:B2 -> B2 end, \
                 R3 = try process_flag(max_heap_size, #{{size => 232}}) of _ -> accepted \
                      catch error:B3 -> B3 end, \
                 R4 = try process_flag(max_heap_size, -1) of _ -> accepted \
                      catch error:B4 -> B4 end, \
                 R5 = try process_flag(max_heap_size, foo) of _ -> accepted \
                      catch error:B5 -> B5 end, {}, \
                 I = {SELF_MAP}, {}, ok.",
                print("[R1, R2, R3, R4, R5]"),
                print(&flag_dump("I")),
            ),
            "[badarg,badarg,badarg,badarg,badarg]\n\
             [{error_logger,true},{include_shared_binaries,false},{kill,true},{size,2000000},4]",
        ),
        (
            "the flag is per-process: a child spawned afterwards keeps the default",
            format!(
                "_ = process_flag(max_heap_size, #{{size => 4000000}}), \
                 Child = spawn(fun() -> receive stop -> ok end end), \
                 C = element(2, process_info(Child, max_heap_size)), {}, {}, \
                 Child ! stop, ok.",
                print(&flag_dump("C")),
                print(&format!("maps:get(size, {SELF_MAP})")),
            ),
            "[{error_logger,true},{include_shared_binaries,false},{kill,true},{size,0},4]\n4000000",
        ),
    ]
}

/// This runtime answers every probe with exactly the recorded stdout, exit code `0` and a silent
/// stderr.
///
/// The recorded values are the oracle's own answers (cross-checked live by the sibling test).
#[test]
fn the_flag_answers_the_recorded_values() {
    for (label, expr, expected) in probes() {
        let ours = beam_rt_eval(&expr);
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt -eval {expr:?}` must exit 0, stderr — {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's answer");
        assert!(
            ours.err.is_empty(),
            "{label}: a successful probe is silent on stderr — {}",
            ours.err
        );
    }
}

/// The system `erl` runs the **same** source text and must print the **same** bytes, so the
/// recorded answers are the oracle's and not just this runtime's habit.
///
/// `erl` is discovered by filesystem only; a missing toolchain is a loud skip. The first probe
/// doubles as the harness's liveness check: if the oracle cannot answer it, this comparison
/// cannot run and is skipped loudly rather than passed silently.
#[test]
fn the_system_oracle_answers_the_same_bytes() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    for (index, (label, expr, expected)) in probes().into_iter().enumerate() {
        let Some(theirs) = support::oracle_eval(&oracle, None, &expr) else {
            if index == 0 {
                eprintln!(
                    "SKIP the live-oracle comparison: the erl probe could not answer {label}"
                );
                return;
            }
            panic!("the oracle could not answer {label} — the probe is {expr:?}");
        };
        assert_eq!(
            theirs, expected,
            "{label}: the system erl must answer the recorded value"
        );
        let ours = beam_rt_eval(&expr);
        assert_eq!(
            ours.code, 0,
            "{label}: beam_rt exited {}: {}",
            ours.code, ours.err
        );
        assert_eq!(
            ours.out, theirs,
            "{label}: the two runtimes must print the same bytes"
        );
    }
}
