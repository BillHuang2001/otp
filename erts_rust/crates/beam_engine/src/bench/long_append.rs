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

//! The **long-append** workload: a binary accumulator that really outgrows its
//! `bs_init_writable` hint.
//!
//! The sibling [`super::foreign_bstr_to_lower_decomposition`] workload
//! (`cowboy_bstr:to_lower/1`) never exercises payload growth: its accumulator hint is 32 bytes
//! and its result is 30 bytes, so it fits the initial capacity on every call. This workload is
//! the complement — the same compiler-emitted accumulator shape (`bs_init_writable` + a
//! `private_append` first segment in the comprehension body), but sized so the accumulator
//! **2048×** outgrows the hint, which is what the writable-payload growth path
//! (`BitWriter::ensure_room` → `Machine::grow_writable_payload` → `beam_alloc`'s
//! `RefcBinary::ensure_capacity`) exists for.
//!
//! # The workload, verbatim ([`SOURCE`])
//!
//! * `input/1` — one `bs_create_bin` of two dynamic-size `integer` segments
//!   (`<<16#0011…eeff:N/unit:8, 16#ffee…1100:N/unit:8>>`): an `integer` segment of `N * unit`
//!   bits encodes the value **zero-extended**, so this is `2 * N` bytes with a distinct 16-byte
//!   marker near the end of each half. **No BIF** (which matters: [`CountingContext`]'s inner
//!   host rejects every BIF), and the two markers make the output byte check non-uniform.
//! * `expand/1` — `<< <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>`: four bytes out per input
//!   byte, so a 16 KiB input grows a 64 KiB accumulator.
//!
//! ## `erlc -S` evidence that it is a *growing* `bs_init_writable` accumulator
//!
//! `expand/1` itself (OTP 29.1.1's `erlc -S`, only the lines that matter):
//!
//! ```text
//! {function, expand, 1, 4}.
//!   {label,4}.
//!     {move,{integer,256},{x,0}}.          % <- the size hint: 256 BITS = 32 bytes
//!     bs_init_writable.
//!     {move,{x,0},{x,1}}.
//!     {move,{y,0},{x,0}}.
//!     {call_last,2,{f,10},1}. % '-expand/1-lbc$^0/2-0-'/2
//! ```
//!
//! and the comprehension body `'-expand/1-lbc$^0/2-0-'/2`:
//!
//! ```text
//!   {label,10}.
//!     {'%',{var_info,{x,1},[{type,{t_bitstring,32,true}}]}}.   % <- `true` = writable
//!     {test,bs_start_match3,{f,12},2,[{x,0}],{x,2}}.
//!     {bs_match,{f,11},{x,2},{commands,[{ensure_at_least,8,1},
//!                                       {integer,3,{literal,[]},8,1,{x,0}}]}}.
//!     {bs_create_bin,{f,0},
//!                    0,3,32,
//!                    {x,1},
//!                    {list,[{atom,private_append},        % <- the first segment
//!                           1,1,nil,
//!                           {tr,{x,1},{t_bitstring,32,true}},
//!                           {atom,all},
//!                           {atom,integer},2,1,nil,{tr,{x,0},{t_integer,{0,255}}},{integer,8},
//!                           {atom,integer},3,1,nil,{tr,{x,0},{t_integer,{0,255}}},{integer,8},
//!                           {atom,integer},4,1,nil,{tr,{x,0},{t_integer,{0,255}}},{integer,8},
//!                           {atom,integer},5,1,nil,{tr,{x,0},{t_integer,{0,255}}},{integer,8}]}}.
//!     {move,{x,2},{x,0}}.
//!     {call_only,2,{f,10}}.                                % <- tail-recursive accumulator loop
//! ```
//!
//! So: a 32-byte writable accumulator (`bs_init_writable`, hint `{integer,256}` bits), one
//! `private_append` of the accumulator itself as the **first** segment, four `integer 8`
//! segments appended per element, and a tail call — precisely C's writable-accumulator idiom
//! (`erl_bits.c:1965-2020`), and precisely the shape whose payload must grow 32 B → 64 KiB
//! (2048×) over the build. The hint is *not* an estimate of the result: the argument's size is
//! unknown to `expand/1`, so the compiler falls back to its 256-bit default. (Building the
//! input inside `expand/1` would have made the estimate `bit_size(Bin) * 4` — exact — and the
//! accumulator would never grow; that is why `input/1` is a separate function.)
//!
//! # What the counters prove
//!
//! [`foreign_long_append_decomposition`] reports the deterministic counters (instructions, the
//! two heap doors, BIF dispatches, off-heap payloads) plus, for a single fresh call, the
//! accumulator payload's growth **shape** (`capacity()`, `reserved_bytes()`,
//! [`GrowthCounters`]) read from [`Machine::refc_binaries`]. The shape is the structural proof
//! that the loop is amortised: `moves` is `O(log n)` and `bytes_copied` is `O(n)`, whereas the
//! copy-rebuild form this replaced copied the whole accumulated prefix once per append, i.e.
//! `≈ n² / (2 * segment_bytes)` — see [`COPY_REBUILD_BYTES`].
//!
//! Only `Machine::run_export` + [`CountingContext`] are used here (no `BitWriter`/`InPlace`/
//! `raw` API), so this file also compiles and runs against the pre-growth tree to produce the
//! A/B.
//!
//! [`GrowthCounters`]: beam_alloc::GrowthCounters

use std::sync::Arc;

use beam_loader::{load_file, Module};
use beam_term::{make_small, Atom, Eterm};

use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::binary_bytes;

use super::counting::CountingContext;
use super::{compile_fixture, env_usize, measure, selected_tiers, Measurement, Prepare};

/// The module name the fixture compiles to.
const MODULE: &str = "long_append";

/// The measured entry: four bytes out per input byte.
const ENTRY: &str = "expand";

/// The fixture's own input builder (setup, never timed).
const INPUT_ENTRY: &str = "input";

/// The integer `input/1` is called with; it returns `2 * N_ARG` bytes (`16 KiB`).
const N_ARG: usize = 8 * 1024;

/// The input binary's size in bytes (`input(N_ARG)`).
const INPUT_BYTES: usize = 2 * N_ARG;

/// The accumulator's final size in bytes: four per input byte (`64 KiB`).
const OUTPUT_BYTES: usize = 4 * INPUT_BYTES;

/// The `bs_init_writable` hint the compiler emits: `{integer,256}` bits = 32 bytes.
const HINT_BYTES: usize = 256 / 8;

/// How many bytes one `private_append` iteration contributes (four `integer 8` segments).
const SEGMENT_BYTES: usize = 4;

/// The appends one call makes (`INPUT_BYTES`, one per input element).
const APPENDS: usize = OUTPUT_BYTES / SEGMENT_BYTES;

/// What the **copy-rebuild** form this growth path replaced would have copied in one call.
///
/// The pre-growth writer filled the accumulator's 32-byte payload, then promoted to a fresh
/// `Vec` carrying the prefix and copied the **whole accumulated prefix** on every subsequent
/// append (`≈ Σ i * SEGMENT_BYTES` for `i` in `1..=APPENDS`). Arithmetic, not a measurement —
/// it is the counterfactual the growth counters are compared against.
const COPY_REBUILD_BYTES: usize = SEGMENT_BYTES * APPENDS * (APPENDS + 1) / 2;

/// The two integer markers `input/1` encodes: each a 16-byte big-endian pattern, so the
/// input is two `N_ARG`-byte integer segments (zero-extended) and every call's bytes are
/// distinct in two places rather than uniformly zero.
const MARKERS: [u128; 2] = [
    0x0011_2233_4455_6677_8899_aabb_ccdd_eeff,
    0xffee_ddcc_bbaa_9988_7766_5544_3322_1100,
];

/// The fixture source: a variable-size fill plus a 4×-expanding binary comprehension.
///
/// Both functions are **BIF-free**: `input/1` is one `bs_create_bin` of two dynamic-size
/// `integer` segments, and `expand/1` is the compiler's writable-accumulator comprehension.
const SOURCE: &str = r#"-module(long_append).
-export([input/1, expand/1]).

input(N) ->
    <<16#00112233445566778899aabbccddeeff:N/unit:8,
      16#ffeeddccbbaa99887766554433221100:N/unit:8>>.

expand(Bin) -> << <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>.
"#;

/// Default measured calls (each one copies ~64 KiB out and appends 16 384 segments).
const DEFAULT_ITERS: usize = 50;
/// Default calls per fresh machine/host batch.
const DEFAULT_BATCH: usize = 5;
/// Default untimed warmup calls.
const DEFAULT_WARMUP: usize = 5;

/// Decompose the long-append accumulator on every selected tier, and report the growth shape
/// of its off-heap payload.
///
/// `#[ignore]`d: it is a measurement, not a gate. Run it with
/// `-- --ignored --nocapture foreign_long_append_decomposition`.
#[test]
#[ignore = "measurement harness"]
fn foreign_long_append_decomposition() {
    let beam = match compile_fixture(MODULE, SOURCE, "long_append") {
        Ok(beam) => beam,
        Err(why) => {
            println!("SKIP: {why} -- the long_append decomposition did not run");
            return;
        }
    };
    let module = Arc::new(
        load_file(&beam).unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display())),
    );
    let entry = atom_for(&module, ENTRY);
    let input_entry = atom_for(&module, INPUT_ENTRY);
    let iters = env_usize("BENCH_ITERS", DEFAULT_ITERS);
    let batch = env_usize("BENCH_BATCH", DEFAULT_BATCH);
    let warmup = env_usize("BENCH_WARMUP", DEFAULT_WARMUP);
    let tiers = selected_tiers();

    // The batch argument: `input/1`'s single integer operand.
    let argument = make_small(N_ARG as i64);
    let expected = expected_bytes();
    let prepare = |ctx: &mut CountingContext, machine: &mut Machine| -> beam_common::Result<Eterm> {
        // Setup runs on the interpreter whatever the measured tier is: it is untimed, and the
        // counters are parked across it (see `super::run_calls`).
        match machine.run_export_outcome_with_tier(
            &module,
            input_entry,
            1,
            &[argument],
            ctx,
            Tier::Interpreter,
        )? {
            RunOutcome::Returned(value) => Ok(value),
            RunOutcome::Wait { .. } => {
                panic!("the fixture's input/1 blocked in a receive: it has none")
            }
        }
    };

    println!(
        "workload = {MODULE}:{ENTRY}/1 ({INPUT_BYTES} bytes in, {OUTPUT_BYTES} bytes out; \
         {HINT_BYTES}-byte bs_init_writable hint)"
    );
    println!("beam = {}", beam.display());
    println!("note = one private_append iteration contributes {SEGMENT_BYTES} bytes, so a call");
    println!("note = appends {APPENDS} times into ONE writable payload -- {:.0}x the hint.",
        OUTPUT_BYTES as f64 / HINT_BYTES as f64);
    println!("note = instr_per_call is the INTERPRETER's reduce-hook count; the native baseline");
    println!("note = tier makes no reduce call at all, so the same figure is reported for both");
    println!("note = tiers (it is a property of the code, not of the tier).");
    println!("note = every *_per_call count is deterministic (a host-side tally); min/median");
    println!("note = ns_per_call are wall clock on a shared host -- compare ratios, not absolutes.");

    let mut measurements = Vec::new();
    for tier in &tiers {
        measurements.push((
            *tier,
            measure(&module, entry, *tier, iters, batch, warmup, &prepare),
        ));
    }
    let instr_per_call = match measurements.iter().find(|(tier, _)| *tier == Tier::Interpreter) {
        Some((_, measurement)) => {
            measurement.counters.instructions as f64 / measurement.iters.max(1) as f64
        }
        None => {
            println!(
                "note = the instruction count came from a short interpreter counting pass \
                 (BENCH_TIER did not select the interpreter)"
            );
            let (instructions, calls) = interpreter_count(&module, &entry, &prepare, batch);
            instructions as f64 / calls as f64
        }
    };

    for (tier, measurement) in &measurements {
        report(measurement, *tier, instr_per_call, &expected);
    }
    if let [(_, first), (_, second)] = measurements.as_slice() {
        assert_eq!(
            binary_bytes(first.result).ok(),
            binary_bytes(second.result).ok(),
            "the two tiers disagree on {MODULE}:{ENTRY}/1"
        );
    }
    report_growth(&module, entry, input_entry, argument, &expected);
}

/// The fixture's own input, modelled: each [`MARKERS`] entry big-endian in the tail of an
/// `N_ARG`-byte integer segment (an `integer` segment of `N * unit` bits encodes the value
/// zero-extended, so `N - 16` zero bytes precede each marker).
fn input_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(INPUT_BYTES);
    for marker in MARKERS {
        let encoded = marker.to_be_bytes();
        bytes.resize(bytes.len() + N_ARG - encoded.len(), 0);
        bytes.extend_from_slice(&encoded);
    }
    bytes
}

/// The byte-identical result a call must produce: four copies of each input byte.
fn expected_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(OUTPUT_BYTES);
    for byte in input_bytes() {
        bytes.resize(bytes.len() + SEGMENT_BYTES, byte);
    }
    bytes
}

/// The exported function's atom.
fn atom_for(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("the fixture exports {name:?}"))
}

/// The interpreter's instruction count per call, for `BENCH_TIER=baseline`.
fn interpreter_count(
    module: &Arc<Module>,
    entry: &Atom,
    prepare: Prepare<'_>,
    calls: usize,
) -> (u64, usize) {
    let mut ctx = CountingContext::new(Arc::clone(module));
    super::run_calls(
        module,
        *entry,
        Tier::Interpreter,
        calls,
        calls,
        &mut ctx,
        prepare,
    )
    .expect("the interpreter counting pass");
    (ctx.counters.instructions, calls.max(1))
}

/// Print one tier's `key = value` table.
fn report(measurement: &Measurement, tier: Tier, instr_per_call: f64, expected: &[u8]) {
    let calls = measurement.iters.max(1) as f64;
    let per_call = |count: u64| count as f64 / calls;
    let min = measurement.min();
    let counters = &measurement.counters;
    let correct = binary_bytes(measurement.result)
        .map(|bytes| bytes == expected)
        .unwrap_or(false);
    println!("---");
    println!("tier = {}", tier.name());
    println!("iters = {}", measurement.iters);
    println!("min_ns_per_call = {min:.1}");
    println!("median_ns_per_call = {:.1}", measurement.median());
    println!("instr_per_call = {instr_per_call:.2}");
    println!(
        "ns_per_instruction = {:.2}",
        if instr_per_call > 0.0 {
            min / instr_per_call
        } else {
            0.0
        }
    );
    println!("rooted_calls_per_call = {:.2}", per_call(counters.rooted_calls));
    println!("rooted_words_per_call = {:.2}", per_call(counters.rooted_words));
    println!(
        "note = the per-iteration rooted reservation (5 words) is bs_start_match3's match \
         context, not the accumulator:"
    );
    println!(
        "note = it is the same on the growth and the copy-rebuild route, so payload growth is \
         read from the block below."
    );
    println!("heap_door_calls_per_call = {:.2}", per_call(counters.heap_calls));
    println!("heap_door_words_per_call = {:.2}", per_call(counters.heap_words));
    println!("bif_calls_per_call = {:.2}", per_call(counters.bif_calls));
    println!("apply_calls_per_call = {:.2}", per_call(counters.apply_calls));
    println!("apply_fun_calls_per_call = {:.2}", per_call(counters.apply_fun_calls));
    println!("external_call_requests_per_call = {:.2}", per_call(counters.external_call_requests));
    println!("local_fun_requests_per_call = {:.2}", per_call(counters.local_fun_requests));
    println!("local_call_requests_per_call = {:.2}", per_call(counters.local_call_requests));
    println!("accumulator_payloads_per_call = {:.2}", per_call(measurement.refc_payloads));
    println!("result_is_correct = {correct}");
    if per_call(measurement.refc_payloads) > 1.0 {
        println!(
            "note = more than one off-heap payload per call means the append path is \
             REBUILDING the accumulator instead of growing it (one fresh payload per append)"
        );
    }

    assert!(
        correct,
        "{} returned {} bytes, not the expected {}",
        tier.name(),
        binary_bytes(measurement.result).map_or(0, |bytes| bytes.len()),
        expected.len()
    );
    assert_eq!(
        counters.bif_calls, 0,
        "{} dispatched BIF calls -- this fixture is BIF-free by construction",
        tier.name()
    );
}

/// One call on a fresh machine, then the accumulator payload's growth shape.
///
/// The payload roster of a fresh machine that ran `input/1` once and `expand/1` once is exactly
/// `[the input payload, the accumulator payload]`, so the accumulator is found by its size. Its
/// [`GrowthCounters`](beam_alloc::GrowthCounters) are the structural proof of the growth
/// behaviour: on a growing tree `moves` is `O(log n)` and `bytes_copied` is `O(n)`; on a
/// rebuilding tree the *last* payload never grew at all, and the payload count is one per
/// append instead of one per call.
fn report_growth(
    module: &Arc<Module>,
    entry: Atom,
    input_entry: Atom,
    argument: Eterm,
    expected: &[u8],
) {
    let mut ctx = CountingContext::new(Arc::clone(module));
    let mut machine = Machine::new();
    let input = match machine.run_export_outcome_with_tier(
        module,
        input_entry,
        1,
        &[argument],
        &mut ctx,
        Tier::Interpreter,
    ) {
        Ok(RunOutcome::Returned(value)) => value,
        other => panic!("the fixture's input/1: {other:?}"),
    };
    let result = match machine.run_export_outcome_with_tier(
        module,
        entry,
        1,
        &[input],
        &mut ctx,
        Tier::Interpreter,
    ) {
        Ok(RunOutcome::Returned(value)) => value,
        other => panic!("the fixture's {ENTRY}/1: {other:?}"),
    };
    assert_eq!(
        binary_bytes(input).ok(),
        Some(input_bytes().as_slice()),
        "the fixture's {INPUT_ENTRY}/1 did not build the modelled input"
    );
    assert_eq!(
        binary_bytes(result).ok(),
        Some(expected),
        "the grow-shape probe's single call returned the wrong bytes"
    );

    let payloads = machine.refc_binaries();
    let accumulator = payloads
        .iter()
        .find(|payload| payload.len() == OUTPUT_BYTES)
        .unwrap_or_else(|| {
            panic!(
                "no {OUTPUT_BYTES}-byte payload among {} (sizes: {:?})",
                payloads.len(),
                payloads.iter().map(|p| p.len()).collect::<Vec<_>>()
            )
        });
    let growth = accumulator.growth_counters();
    let created = payloads.len().saturating_sub(1);

    println!("---");
    println!("accumulator = one call on a fresh machine, interpreter tier");
    println!("input_bytes = {INPUT_BYTES}");
    println!("output_bytes = {OUTPUT_BYTES}");
    println!("init_writable_hint_bytes = {HINT_BYTES}");
    println!("append_bytes_per_iteration = {SEGMENT_BYTES}");
    println!("appends = {APPENDS}");
    println!("payloads_created_by_the_call = {created}");
    println!("accumulator_len_bytes = {}", accumulator.len());
    println!("accumulator_capacity_bytes = {}", accumulator.capacity());
    println!("accumulator_reserved_bytes = {}", accumulator.reserved_bytes());
    println!("in_place_grows = {}", growth.in_place_grows);
    println!("moves = {}", growth.moves);
    println!("bytes_copied = {}", growth.bytes_copied);
    println!("copy_rebuild_bytes_counterfactual = {COPY_REBUILD_BYTES}");
    println!(
        "copy_rebuild_over_growth_bytes = {:.1}",
        COPY_REBUILD_BYTES as f64 / growth.bytes_copied.max(1) as f64
    );
    println!(
        "note = bytes_copied is the whole of the accumulator payload's growth cost; \
         copy_rebuild_bytes_counterfactual"
    );
    println!(
        "note = is arithmetic (Σ accumulated prefixes, one full copy per append), not a \
         measurement."
    );
    if created > 1 {
        println!(
            "note = {created} payloads for one call: this tree REBUILDS the accumulator on \
             every append instead of growing it."
        );
    } else {
        // The amortisation invariant, on the tree that grows: the whole build's copy cost is
        // O(n), not the O(n² / segment) the copy-rebuild form pays. Structural, not a timing.
        assert!(
            growth.bytes_copied < 8 * OUTPUT_BYTES,
            "growing the accumulator copied {} bytes for a {OUTPUT_BYTES}-byte result -- that \
             is not amortised linear growth",
            growth.bytes_copied
        );
    }
    if growth.moves == 0 && growth.in_place_grows == 0 && created > 1 {
        println!(
            "note = the returned payload never grew: growth counters describe the payload the \
             last append built, not the accumulation."
        );
    }
}
