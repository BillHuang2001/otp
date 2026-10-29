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

//! **The heap-sizing flags have two halves — the stored policy and the heap it sizes — and this
//! suite pins how much of the second half a *live* `process_flag/2` write reaches today**
//! (contracts A.6/A.8).
//!
//! # Row 1 — birth-time adoption already works on both sides
//!
//! `spawn_opt(fun() -> receive stop -> ok end end, [{min_heap_size, 200000}])` then
//! `process_info(P, [heap_size, min_heap_size])` answers `[{heap_size,318187},
//! {min_heap_size,318187}]` under the oracle **and** under this runtime: the spawn rule
//! (`erts/emulator/beam/erl_process.c:12586-12592`) sizes the child's nursery from the option, and
//! `HEAP_SIZE(p)` is what `erl_bif_info.c:936` reports.
//!
//! # Row 2 — the live write reaches the policy, never the heap (the residual)
//!
//! `process_flag(min_heap_size, 200000)` on the **running** process splits into the two halves
//! that the flag has:
//!
//! * the **stored policy** — already parity: the old value is returned (`233`, the emulator's
//!   `H_DEFAULT_SIZE`), and the read-back is `318187`, the growth table's grain
//!   (`beam_gc::next_heap_size`), never the `200000` that was asked for;
//! * the **heap it sizes** — the residual: the oracle's next collection installs the new minimum
//!   (`heap_size` reaches `318187`), while this runtime's `heap_size` stays exactly at its pre-set
//!   capacity, before and after `erlang:garbage_collect/0`.
//!
//! The only missing half is the live policy write. `beam_gc` already exposes the write side
//! (`beam_gc::HeapPolicy::{set_min_heap_words, set_min_bin_vheap_words, set_fullsweep_after}`,
//! `crates/beam_gc/src/policy.rs`, and every sizing rule passes through `next_words` at the next
//! collection). What is missing is the **route** to it: `beam_process::Process` exposes
//! `collector()` and no mutable sibling, so no store site can reach the collector's policy — the
//! implementer needs `beam_process::Process::collector_mut(&mut self) -> &mut AnyCollector`
//! beside `collector()` (`crates/beam_process/src/process.rs:1453`). Today the flag is stored as
//! per-process bookkeeping instead (`Runtime::heap_flags`,
//! `beam_rt/src/context/process_bridge.rs`), which is why the read-back is right and the heap is
//! not. **When that seam lands, this suite's two `gc_*` rows become equalities with the oracle
//! and this table is what says so.**
//!
//! # Rows 3 and 4 — the two narrowings are parity today
//!
//! * `fullsweep_after`: the C stores a `Uint16` (`erl_process.h:1145`), so
//!   `process_flag(fullsweep_after, 100000)` returns the old `65535` and reads back `34464`.
//! * `min_bin_vheap_size`: the `BIN_VH_MIN_SIZE` floor (`46422`), so both `0` and `500` read back
//!   `46422`, and the old value is `46422` either way.
//!
//! # What is deliberately **not** pinned
//!
//! An absolute `heap_size` is **own-state**: it is the process's own nursery, and even the
//! oracle's pre-set figure moves between runs and hosts (measured 610 / 2586 / 4185 here for the
//! same probe). So the heap rows are asserted as **relations** (`unchanged by the set`, `sized to
//! the policy by the following collection`) and never as a cross-runtime equality on the number.
//! No GC counter is compared either — no `minor_gcs`, no `erlang:memory/0` (both are host- and
//! cadence-dependent, and this crate's strategy forbids cross-runtime comparison of them).
use std::path::Path;

mod support;

/// The banner every loud skip names.
const BANNER: &str = "heap-sizing flags";
/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The fixture: one probe per row, each reporting a shape that is comparable **across** runtimes.
///
/// `live_shape/0` reduces the live write to relations (`set_left_heap_unchanged`,
/// `heap_below_policy_before_set`, `gc_sized_heap_to_policy`, `gc_left_heap_unchanged`) beside the
/// two figures that are genuinely identical on both sides (the old value and the read-back), so no
/// own-state `heap_size` has to be compared between the runtimes. `live_raw/0` carries the same
/// timeline as raw numbers, for a reader who wants the absolute figures; it is not asserted.
const FIXTURE_SRC: &str = r#"
-module(hsf).
-export([birth/0, live_shape/0, live_raw/0, readbacks/0]).

%% Row 1: a freshly spawned child, sized by its own spawn option. The parent does the reading, so
%% the subject has neither been stepped nor asked to build anything.
birth() ->
    P = spawn_opt(fun() -> receive stop -> ok end end, [{min_heap_size, 200000}]),
    Answer = process_info(P, [heap_size, min_heap_size]),
    P ! stop,
    Answer.

%% Row 2: one process's timeline through the live write, reduced to relations.
live_shape() ->
    H0 = hsize(),
    Old = process_flag(min_heap_size, 200000),
    H1 = hsize(),
    Readback = mhs(),
    _ = erlang:garbage_collect(),
    H2 = hsize(),
    [{old, Old},
     {readback, Readback},
     {set_left_heap_unchanged, H0 =:= H1},
     {heap_below_policy_before_set, H0 < Readback},
     {gc_sized_heap_to_policy, H2 >= Readback},
     {gc_left_heap_unchanged, H2 =:= H0}].

%% Row 2 again, as the raw figures (own-state: never compared between runtimes).
live_raw() ->
    H0 = hsize(),
    Old = process_flag(min_heap_size, 200000),
    H1 = hsize(),
    Readback = mhs(),
    _ = erlang:garbage_collect(),
    H2 = hsize(),
    [{heap_size_before, H0}, {old, Old}, {heap_size_after_set, H1},
     {readback, Readback}, {heap_size_after_gc, H2}].

%% Rows 3 and 4: each flag as `{tag, OldValue, process_info/2 Answer}`.
readbacks() ->
    FsaOld = process_flag(fullsweep_after, 100000),
    Fsa = process_info(self(), fullsweep_after),
    BinZeroOld = process_flag(min_bin_vheap_size, 0),
    BinZero = process_info(self(), min_bin_vheap_size),
    BinFiveOld = process_flag(min_bin_vheap_size, 500),
    BinFive = process_info(self(), min_bin_vheap_size),
    [{fullsweep_after, FsaOld, Fsa},
     {min_bin_vheap_zero, BinZeroOld, BinZero},
     {min_bin_vheap_500, BinFiveOld, BinFive}].

hsize() -> {heap_size, H} = process_info(self(), heap_size), H.
mhs() -> {min_heap_size, M} = process_info(self(), min_heap_size), M.
"#;

/// Row 1's recorded golden (measured under OTP 29.1.2 / erts-17.1.1 and asserted on **both**
/// sides, so the row is pinned even before the oracle leg looks at it).
const BIRTH_GOLDEN: &str = "[{heap_size,318187},{min_heap_size,318187}]";

/// The four live-write rows both runtimes already agree on: the old value (the emulator's
/// `H_DEFAULT_SIZE` floor), the read-back (the growth table's grain), and the two relations that
/// say the write is **policy only** (`heap_size` unchanged by the set, and the pre-set heap below
/// the new minimum — the premise that makes the fourth row a divergence rather than a tautology).
const LIVE_SHARED: &str = "[{old,233},{readback,318187},{set_left_heap_unchanged,true},\
                           {heap_below_policy_before_set,true},";
/// The oracle's tail: its following collection **does** install the new minimum.
const LIVE_ORACLE_TAIL: &str = "{gc_sized_heap_to_policy,true},{gc_left_heap_unchanged,false}]";
/// This runtime's tail: the collection leaves the heap exactly where the set found it — the
/// residual, and the only reason this suite's rows are a pair rather than one equality.
const LIVE_RUST_TAIL: &str = "{gc_sized_heap_to_policy,false},{gc_left_heap_unchanged,true}]";

/// Rows 3 and 4: the `Uint16` narrowing of `fullsweep_after` (`100000` → `34464`) and the
/// `BIN_VH_MIN_SIZE` floor of `min_bin_vheap_size` (`0` and `500` → `46422`), each with the old
/// value the flag reports.
const READBACKS_GOLDEN: &str = "[{fullsweep_after,65535,{fullsweep_after,34464}},\
                                {min_bin_vheap_zero,46422,{min_bin_vheap_size,46422}},\
                                {min_bin_vheap_500,46422,{min_bin_vheap_size,46422}}]";

/// `beam_rt -noshell -pa <dir> -eval 'io:format("~1000p~n",[hsf:<function>()]), halt(0).'`.
///
/// `BEAM_RT_TIER` is **removed** for the child, so an ambient setting cannot make the run
/// something other than this runtime's default tier. `~1000p` keeps a nested term on one line in
/// both runtimes, so a pinned golden stays a single readable literal.
fn rust_answer(fixture: &support::ErlangFixture, function: &str) -> String {
    let dir = fixture.dir().to_str().expect("an ASCII scratch path");
    let expr = format!("io:format(\"~1000p~n\",[hsf:{function}()]), halt(0).");
    let output = support::run_bounded_with_env(
        Path::new(BINARY),
        &["-noshell", "-pa", dir, "-eval", &expr],
        &[("BEAM_RT_TIER", None)],
    )
    .unwrap_or_else(|| {
        panic!("`hsf:{function}/0` could not be run to completion (killed at the probe bound)")
    });
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "`hsf:{function}/0` must return normally; stdout={stdout:?} stderr={:?}",
        stderr.trim()
    );
    // These are served calls (`process_flag/2`, `process_info/2`, `spawn_opt/2`,
    // `erlang:garbage_collect/0`), so a capability gap is a defect here, not a skip.
    assert!(
        !stderr.contains("unsupported"),
        "`hsf:{function}/0` is a served call, so this is not a capability gap: {stderr:?}"
    );
    stdout
}

/// The same probe under the live oracle, or `None` (after a loud skip naming [`BANNER`]).
fn oracle_answer(fixture: &support::ErlangFixture, function: &str) -> Option<String> {
    support::fixture_oracle_answer(BANNER, fixture.dir(), &format!("hsf:{function}()"))
}

/// **Row 1 — a birth-time `min_heap_size` sizes the child's heap on both sides.**
///
/// The recorded golden is asserted on this runtime **first**, so the row is pinned
/// unconditionally; the oracle leg then re-measures the same golden (a drift in the system ERTS is
/// a failure here, never a silently stale table) and the two sides are compared to each other.
#[test]
fn the_birth_time_heap_options_size_the_heap_on_both_sides() {
    let Some(fixture) = support::ErlangFixture::compile(BANNER, "hsf_birth", "hsf", FIXTURE_SRC)
    else {
        return;
    };
    let ours = rust_answer(&fixture, "birth");
    assert_eq!(
        ours, BIRTH_GOLDEN,
        "a spawn_opt `{{min_heap_size,200000}}` child must be born with its rounded minimum as \
         its heap (and report both items equal)"
    );

    let Some(theirs) = oracle_answer(&fixture, "birth") else {
        support::skip_loudly(BANNER, "the oracle could not answer `hsf:birth/0`");
        return;
    };
    assert_eq!(
        theirs, BIRTH_GOLDEN,
        "the oracle's own answer changed — re-measure the table"
    );
    assert_eq!(
        ours, theirs,
        "birth-time heap sizing is parity: the two runtimes must report the same items"
    );
}

/// **Row 2 — the live `min_heap_size` write reaches the policy but not the heap.**
///
/// One process's timeline, as a `(oracle, this runtime)` pair per step:
///
/// | step | oracle | this runtime | what it says |
/// |---|---|---|---|
/// | `old` | `233` | `233` | the replaced value is the emulator's `H_DEFAULT_SIZE` floor — equality |
/// | `readback` | `318187` | `318187` | the growth table's grain, not the requested `200000` — equality |
/// | `set_left_heap_unchanged` | `true` | `true` | the write is policy-only on both sides — equality |
/// | `heap_below_policy_before_set` | `true` | `true` | the pre-set heap really is below the new minimum — equality |
/// | `gc_sized_heap_to_policy` | `true` | `false` | **the residual**: the collection installs the minimum there, not here |
/// | `gc_left_heap_unchanged` | `false` | `true` | the same fact from the other side: this runtime's heap is still the pre-set one |
///
/// The last two rows are this crate's residual convention: the only missing half is the live
/// policy write, and **when it lands these rows become equalities and this table is what says
/// so** (the seam is `beam_process::Process::collector_mut`, see the module docs). Nothing here
/// compares a `heap_size` between the runtimes: the relations are what carry the claim, so the
/// suite is stable on a host whose own pre-set heap differs (measured 610 / 2586 / 4185 under the
/// oracle for the same probe).
#[test]
fn the_live_min_heap_size_write_reaches_the_policy_but_not_the_heap() {
    let Some(fixture) = support::ErlangFixture::compile(BANNER, "hsf_live", "hsf", FIXTURE_SRC)
    else {
        return;
    };
    let ours = rust_answer(&fixture, "live_shape");

    // (a) the old value, (b) the read-back, (c) the set leaving the live heap alone — all parity,
    // and all asserted before the oracle leg so the table is pinned without a toolchain.
    let expected_rust = format!("{LIVE_SHARED}{LIVE_RUST_TAIL}");
    assert_eq!(
        ours, expected_rust,
        "the live write's policy rows (old/readback/policy-only) plus the two pinned GC rows: a \
         change here is the live-policy-write half landing, and these rows must then become an \
         equality with the oracle's"
    );

    let Some(theirs) = oracle_answer(&fixture, "live_shape") else {
        support::skip_loudly(BANNER, "the oracle could not answer `hsf:live_shape/0`");
        return;
    };
    let expected_oracle = format!("{LIVE_SHARED}{LIVE_ORACLE_TAIL}");
    assert_eq!(
        theirs, expected_oracle,
        "the oracle's own answer changed — re-measure the table"
    );

    // (d) the divergence, stated as the two rows that differ: this runtime's post-collection
    // `heap_size` still equals its pre-set value (`gc_left_heap_unchanged,true`) where the
    // oracle's reaches the new minimum (`{readback,318187}`).
    assert!(
        ours.starts_with(LIVE_SHARED) && theirs.starts_with(LIVE_SHARED),
        "the shared policy rows must be identical on both sides; oracle={theirs:?} rust={ours:?}"
    );
    assert!(
        ours.ends_with(LIVE_RUST_TAIL),
        "this runtime's collection must still leave the heap at its pre-set capacity; got {ours:?}"
    );
    assert!(
        theirs.ends_with(LIVE_ORACLE_TAIL),
        "the oracle's collection must install the new minimum; got {theirs:?}"
    );
    assert_ne!(
        ours, theirs,
        "these rows are a pinned residual today — the day they agree, this pair becomes an \
         equality and the table above is what has to change"
    );
}

/// **Rows 3 and 4 — the `fullsweep_after` and `min_bin_vheap_size` read-backs are oracle-exact.**
///
/// Both flags are policy-only writes that this runtime already reproduces figure for figure: the
/// `Uint16` narrowing of `fullsweep_after` (`100000` reads back `34464`, old value `65535`) and
/// the `BIN_VH_MIN_SIZE` floor of `min_bin_vheap_size` (`0` and `500` both read back `46422`).
#[test]
fn the_fullsweep_after_and_min_bin_vheap_readbacks_are_oracle_exact() {
    let Some(fixture) =
        support::ErlangFixture::compile(BANNER, "hsf_readbacks", "hsf", FIXTURE_SRC)
    else {
        return;
    };
    let ours = rust_answer(&fixture, "readbacks");
    assert_eq!(
        ours, READBACKS_GOLDEN,
        "`fullsweep_after` narrows to a `Uint16` and `min_bin_vheap_size` floors at \
         `BIN_VH_MIN_SIZE`, with the old value reported each time"
    );

    let Some(theirs) = oracle_answer(&fixture, "readbacks") else {
        support::skip_loudly(BANNER, "the oracle could not answer `hsf:readbacks/0`");
        return;
    };
    assert_eq!(
        theirs, READBACKS_GOLDEN,
        "the oracle's own answer changed — re-measure the table"
    );
    assert_eq!(
        ours, theirs,
        "both narrowings are parity, so the two runtimes must agree figure for figure"
    );
}
