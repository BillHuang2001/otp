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
//! **Off-heap refc-binary reclamation, witnessed end to end** — the integration suite for the
//! slice-boundary prune (`src/context/gc/bin_vheap.rs`, reached from
//! `RuntimeRunner::reclaim_off_heap_binaries` in the runner's `Yield` arm).
//!
//! # The loop under test
//!
//! A process that builds an **off-heap** binary per iteration and discards it accumulates one
//! handle per iteration in its machine's payload reservoir ([`Machine::refc_binaries`]
//! (`beam_engine::Machine::refc_binaries`)), which `Machine::reset` deliberately never clears: a
//! returned term may still name a payload after the run that built it. Both the `binary_alloc`
//! live-bytes figure — what `erlang:memory(binary)` reports, read **live** at each call
//! (`src/context/memory_hook.rs`) — and RSS grow with the loop count where the oracle's stay
//! flat. The prune closes that loop for the payloads **one** park built: at every slice-boundary
//! park whose window **published** the machine's root set it walks the process's own roots
//! (`beam_gc::for_each_reachable_binary_payload`) and releases the handles nothing reaches.
//! Across **many** parks the retention is not a residue at all: a park releases the **whole**
//! reservoir (the census reads `retained_by_live = 0`, `retained_by_shared = 0` and
//! `binary_alloc` live `0` after every prune), so the peak a loop shows is one pre-emption window
//! rather than a sum over parks (see *The phase-immune reading* below).
//!
//! Only **compiled bitstring code** feeds the reservoir
//! (`Machine::alloc_refc_binary`/`alloc_refc_binary_with_capacity`, whose callers are
//! `bs_create_bin` and `bs_init_writable`), so every payload here is built by compiled Erlang —
//! `<<0:(Bytes*8)>>` with a **runtime** `Bytes` (`bs_create_bin`). A
//! `binary:copy/2`/`base64:encode/1` *result* would still be a BIF-built payload in part, and a
//! compile-time-constant size would be folded into a literal; both shapes would prove nothing.
//!
//! # The `run` route, never `-eval`
//!
//! The reclamation needs a **scheduled** process: on the `-eval` route the machine is not in
//! `RunnerState`, allocates on the runtime's pointer-stable arena, and the park arm never
//! publishes, so the prune declines by construction (`bin_vheap`'s module docs). Every test here
//! therefore drives `beam_rt run <file.beam> <m>:<f>/<a> <args…>`, whose result the CLI prints
//! via `~w~n`.
//!
//! # A fixed-offset reading is a slice phase, never a leak
//!
//! The reservoir is emptied at a **pre-emption**, so between two parks the figure is exactly the
//! number of payloads the current slice has built. A reading taken at a fixed iteration offset
//! therefore samples a fixed point of that cycle — measured on this tree, the `run` route, the
//! native baseline tier, with the fixture below:
//!
//! | reading | 1 024-byte payloads | 4 096-byte payloads |
//! |---|---|---|
//! | `discard(1 000)` / `discard(10 000)`, one read after the loop | 1 072 / 1 072 | 4 144 / 4 144 |
//! | `ladder(10 000, 1 000)`, a rung every 1 000 iterations | `[1 072 000, 1 072, 1 073 072, 1 072, …]` | `[4 144, 8 288, …, 41 440]` |
//!
//! The ladder is the phase in its clearest form. A slice of the churn body is ≈999 iterations — the
//! same at every payload size, because the reduction cost per iteration does not depend on the
//! payload — so a 1 000-iteration rung is *one* iteration longer than a slice and
//! the read advances one iteration deeper into the next window at every rung: rung `k` is `k`
//! payloads, for ever, without the reservoir ever exceeding one slice. **A fixed-offset ladder's
//! monotonicity is therefore not an assertion** — it is a property of the sampling offset, and the
//! `1 072 000`-byte rung above is not a leak.
//!
//! # The phase-immune reading: the dense envelope, and the bounds derived from it
//!
//! Reading `erlang:memory(binary)` after **every** iteration and folding `{min, max}` removes the
//! phase: the `min` is the post-park trough, the `max` is one pre-emption window, and the window —
//! not `N` — bounds both. The fixture's `dense/2` returns `{Min, Max, Trough, Parks}`, where
//! `Trough` is the largest reading taken immediately after a reading **fell** (the residue a park
//! leaves behind), `Parks` counts those falls, and `-1` is the sentinel for "no reading ever
//! fell". Measured (`/proc/loadavg` ≈ 0.6, counters only):
//!
//! | shape | N | payload | `{min, max, trough, parks}` |
//! |---|---|---|---|
//! | `dense/2` | 2 500 | 1 024 | `{0, 475 968, 1 072, 5}` |
//! | `dense/2` | 20 000 | 1 024 | `{0, 475 968, 1 072, 45}` |
//! | `dense/2` | 20 000 | 512 | `{1 040, 924 560, 1 040, 22}` |
//! | `dense/2` | 20 000 | 2 048 | `{0, 930 624, 2 096, 45}` |
//! | `dense/2` | 20 000 | 4 096 | `{0, 1 839 936, 4 144, 45}` |
//! | `dense_hold/3` `ref`, `ets`, `mon` | 20 000 | 1 024 | `{{0, 475 968, 1 072, 45}, true}` |
//! | `dense/2` under `BEAM_RT_TIER=interpreter` | 20 000 | 1 024 | `{0, 428 800, 1 072, 50}` |
//!
//! Three properties follow, and they are the three the guard asserts:
//!
//! * the **peak** is one window — 444 iterations × (1 024 + 48) = **475 968 bytes** — the same at
//!   N = 2 500 and at N = 20 000;
//! * the **trough** is exactly one payload (1 072 bytes) at every park, never a residue that
//!   accumulates, because the park releases the whole reservoir;
//! * the **park count** moves with N (5 → 45 over the 8× step), so the readings are measured
//!   against a signal that really is doing work.
//!
//! [`BOUND_BYTES`] is derived from that window rather than chosen: `beam_process`'s
//! `DEFAULT_REDUCTIONS_PER_SLICE` (= `CONTEXT_REDS` = 4 000) divided by this body's ≈9 reductions
//! per iteration gives ≈444 payloads per slice, `444 × 1 072` = **475 968**; the binary
//! virtual-heap floor (`BIN_VH_MIN_SIZE` = 46 422 words = **371 376 bytes**) is added as margin,
//! because that budget gates a prune and can only *delay* one: `475 968 + 371 376` = **847 344**,
//! rounded up to **1 048 576 (1 MiB)**. That is 8× tighter than the bound a *single* read needs —
//! [`SINGLE_READ_BYTES`], since one slice of the 4 KiB-payload churn body is 999 × 4 144 = 4.1 MB
//! and a single read may legitimately land there — and every shape that retains instead of
//! releasing breaks it:
//!
//! | the shape that breaks the bound | figure | factor over 1 MiB |
//! |---|---|---|
//! | one live reference declining every park, 1 KiB payloads, N = 10 000 | 10 720 000 | 10.2× |
//! | the same shape at 4 KiB payloads | 41 440 000 | 39.5× |
//! | no prune at all (a per-iteration residue) at N = 20 000, 1 KiB payloads | 21 440 000 | 20.4× |
//!
//! The first two rows are what `gc_walk.rs`'s prune-only valve produces when it hits on **any**
//! pointer-bearing foreign word: a reference block is foreign, so the prune declined at every park
//! and `erlang:memory(binary)` read exactly `N × (payload + 48)` with zero releases — 1 000 rungs of
//! this file's own ladder at 1 KiB payloads read `[1 072 000, 2 144 000, …, 10 720 000]`, each rung
//! one whole payload over the last — while the reference-free control stayed at the dense envelope
//! above.
//!
//! [`SMALL_BYTES`] (4 payloads = 4 288 bytes) is the trough bound, and it is the assertion that
//! names a **per-park residue**: the measured trough is one payload at every park in every shape
//! above, so a residue of one payload per park reaches 45 payloads = 48 240 bytes at N = 20 000 —
//! 11× over the bound — and grows with the park count, while a phase never does.
//!
//! # The one shape this loop does not bound
//!
//! A slice that **never pre-empts** reaches no park, and so no prune: on the native baseline tier
//! a linked tail exit charges no reduction, so a tail-call-only build-and-drop loop runs its whole
//! computation inside one slice, its reservoir grows with the iteration count and
//! `erlang:memory(binary)` with it — no reading ever falls, so the dense sampler below would
//! report its `-1` trough sentinel and zero parks. The same program under
//! `BEAM_RT_TIER=interpreter` is bounded: that tier charges one reduction per instruction, so the
//! process really pre-empts and its parks prune. The repair is a reduction charge on the linked
//! tail-exit path (`jit/baseline/runtime.rs::jit_tail_call`, plus `emit/link.rs`'s
//! `call_only`/`jump` arm), a `beam_engine` change and **not** one here: no host-side charge can
//! end such a slice, because the engine's position store parks at block granularity. No body
//! [`FIXTURE_ERL`] compiles has that shape, so this suite neither drives nor asserts it: every one
//! keeps a non-tail call per iteration (`lists:seq/2` + `length/1`), which is what makes a park
//! reachable at all.
//!
//! # What each test pins
//!
//! 1. [`memory_binary_is_flat_across_a_10x_iteration_step`] — two separate `beam_rt run`
//!    invocations, N = 1 000 and N = 10 000 single reads: the 10× step must add at most
//!    [`SINGLE_READ_BYTES`], and the larger figure must be under it in absolute terms.
//! 2. [`the_dense_envelope_is_bounded_and_phase_immune`] — the dense envelope at
//!    [`DENSE_SMALL_N`] and [`DENSE_N`]: non-inertness (a peak of at least [`MIN_PEAK_BYTES`] and
//!    at least [`MIN_PARKS`] parks on each leg), both peaks under [`BOUND_BYTES`], both troughs
//!    under [`SMALL_BYTES`], and the 10× sibling's peak and trough unmoved.
//! 3. [`a_live_reference_in_the_root_set_costs_the_reservoir_nothing`] — the regression guard for
//!    the reference exemption: one `make_ref/0`, one unnamed ETS tid and one `monitor/2` reference
//!    held live across the whole loop, each against the same peak/trough bound as the
//!    reference-free control *and* a peak within 2× of it. This is the assertion that is red on a
//!    product whose prune declines on a reference.
//! 4. [`a_retained_off_heap_binary_survives_every_prune_byte_exactly`] — the **safety** half:
//!    a 64 KiB off-heap binary kept live across thousands of prunes must come back
//!    **byte-exact** (MD5 unchanged, `=:=` a fresh rebuild, both ends of the payload read back,
//!    identical to the live oracle's own answer), *and* its payload must still be counted in
//!    `erlang:memory(binary)`. A prune that freed a live payload — or that never ran — cannot
//!    satisfy both halves at once.
//! 5. [`a_missing_toolchain_is_a_loud_skip_never_a_silent_pass`] — the loud-skip guard: an
//!    empty toolchain path resolves no compiler, and the suite's gate declines (printing the
//!    shared `SKIPPED` / `NOT a pass` banner) instead of building a fixture or asserting.
//! 6. [`a_retained_off_heap_binary_survives_the_fun_loop_byte_exactly`] — the fun-loop safety
//!    half: the same anti-dangle property through `lists:foldl/3`'s nested-machine route.
//! 7. [`the_fun_loop_envelope_is_bounded_by_the_bin_vheap_budget`] — the fun-loop release half:
//!    one read per N, bounded and non-growing (see the case's own note on the `N − last park`
//!    aliasing that makes its small leg read a whole slice).
//! 8. [`a_binary_named_only_by_a_pending_timer_survives_the_eval_statement_prune_byte_exactly`] —
//!    the `-eval` route's escaped-payload store: a payload whose only owner is a pending
//!    `erlang:send_after` survives the statement-boundary prune byte-exactly, with the prune
//!    counted and nothing declined.
//!
//! # Conventions
//!
//! The fixture is compiled **at test time** with the system `erlc` into a scratch directory
//! (never committed — the source of truth is [`FIXTURE_ERL`] below) that holds *only* the
//! fixture module, so it is a safe single `-pa` for the live oracle. `erl`/`erlc` are found by
//! **filesystem** discovery only (never by running the tool), and a missing toolchain is a
//! **loud skip** (`tests/support/mod.rs`), never a silent pass and never a panic.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use beam_rt::config::RuntimeConfig;
use beam_rt::runtime::Runtime;
// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

/// The fixture's module name.
const MODULE: &str = "offheap_reclaim";

/// The suite's name in the loud-skip banner.
const SUITE: &str = "off_heap_reclaim";

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The payload every discarded iteration builds, in bytes.
///
/// Well past `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` (64 bytes), so each one is a genuine
/// off-heap refc payload; see the module docs for why it is this size rather than 64 KiB.
const PAYLOAD_BYTES: u64 = 4_096;

/// The smaller iteration count of the 10× step.
const SMALL_N: u64 = 1_000;

/// The larger iteration count of the 10× step.
const LARGE_N: u64 = 10_000;

/// The size of the binary the safety half **retains** across thousands of prunes.
const KEEP_BYTES: u64 = 65_536;

/// The churn count of the safety half: 6 000 × 4 KiB = 24 MiB of payloads built and discarded
/// while the retained binary stays live (three times [`SINGLE_READ_BYTES`], so the assertion
/// below is decisive against a run whose churn was never pruned).
const RETAIN_CHURN_N: u64 = 6_000;

/// The bound a **single** read taken after a churn block is stated against, in bytes.
///
/// A single read lands at an arbitrary point of the slice phase, and one slice of the 4 KiB
/// payload churn body is ≈999 iterations — 4.1 MB of payload bytes — so this bound is eight times
/// the dense one on purpose: its only job is to separate "bounded by one slice" from "linear in
/// N". The same loop with the reclamation disabled is `N × (4 096 + 48)` (4 144 000 bytes at
/// N = 1 000, 41 440 000 at N = 10 000), and the fun-loop route's small leg reads a whole slice
/// (4 144 000 at N = 1 000). The phase-immune bound is [`BOUND_BYTES`].
const SINGLE_READ_BYTES: u64 = 8 * 1024 * 1024;

/// The **dense envelope's** peak bound, in bytes — derived, not chosen (module docs,
/// *The phase-immune reading*).
///
/// `DEFAULT_REDUCTIONS_PER_SLICE` (4 000) ÷ the dense body's ≈9 reductions per iteration ≈ 444
/// iterations per slice, × [`DENSE_BLOCK_BYTES`] (1 072) = 475 968 bytes, plus the binary
/// virtual-heap floor (`BIN_VH_MIN_SIZE` = 46 422 words = 371 376 bytes) as margin = 847 344,
/// rounded up to 1 MiB. The measured peak is 475 968 (2.2× of headroom); a product that retains
/// instead of releasing is 4.6×–20.4× over it.
const BOUND_BYTES: u64 = 1_048_576;

/// The payload the **dense** envelope is sampled with, in bytes.
///
/// Four times smaller than [`PAYLOAD_BYTES`] on purpose: a slice then holds ≈444 iterations
/// instead of ≈999, so one window is 475 968 bytes and the derived [`BOUND_BYTES`] can be 8×
/// tighter than [`SINGLE_READ_BYTES`]. At 4 KiB the window alone (4.1 MB) could not be told apart
/// from the pre-fix retention.
const DENSE_PAYLOAD_BYTES: u64 = 1_024;

/// One dense payload plus its 48-byte control header — the unit the dense figures are counted in.
const DENSE_BLOCK_BYTES: u64 = DENSE_PAYLOAD_BYTES + 48;

/// The iteration count of the dense guard's small leg (measured 5 parks).
const DENSE_SMALL_N: u64 = 2_500;

/// The iteration count of the dense guard's large leg (measured 45 parks).
const DENSE_N: u64 = 20_000;

/// The peak a non-vacuous dense envelope must reach: 64 payloads.
///
/// Measured 444 payloads ([`MIN_PEAK_BYTES`] = 68 608 bytes, 6.9× below the measured 475 968), so
/// the threshold is comfortably reachable by a real window and unreachable by a fixture that never
/// fed the reservoir.
const MIN_PEAK_PAYLOADS: u64 = 64;

/// [`MIN_PEAK_PAYLOADS`] in bytes.
const MIN_PEAK_BYTES: u64 = MIN_PEAK_PAYLOADS * DENSE_BLOCK_BYTES;

/// The trough bound: 4 payloads — the assertion that names a **per-park residue**.
///
/// Measured one payload at every park in every shape, so a residue of one payload per park reaches
/// 45 payloads at [`DENSE_N`] (11× over this bound) and grows with the park count, while a slice
/// phase never does.
const MAX_TROUGH_PAYLOADS: u64 = 4;

/// [`MAX_TROUGH_PAYLOADS`] in bytes.
const SMALL_BYTES: u64 = MAX_TROUGH_PAYLOADS * DENSE_BLOCK_BYTES;

/// The park count both dense legs must show: the sampler really saw parks (measured 5 at
/// [`DENSE_SMALL_N`] and 45 at [`DENSE_N`]), so "flat" cannot be the flatness of a reservoir that
/// nothing ever emptied.
const MIN_PARKS: u64 = 3;

/// How long one `beam_rt run` child may live before it is killed and reported.
///
/// `LARGE_N` iterations are ≈1.5 s of work on an idle host, so this is a loaded-host safety
/// net, not the normal path (`beam_rt run` is itself bounded by the runtime's own 60 s
/// `RUN_DEADLINE`).
const PROBE_LIMIT: Duration = Duration::from_secs(300);

/// How often a live child's `/proc/<pid>/status` is sampled for `VmHWM`.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(5);

/// The fixture source (compiled at test time — never committed as a `.beam`).
///
/// `one/1` is the whole workload: build a `Bytes`-byte binary through `bs_create_bin`, discard
/// it, and touch the heap (so the process reaches a heap-admission door, which is where the
/// slice budget pre-empts — see the module docs). `discard/2` and `ladder/3` drive it and read
/// `erlang:memory(binary)` (bytes) inside the *same* process; `retain/3` keeps one engine-built
/// off-heap binary live across the churn and reports its identity/content plus the figure;
/// `dense/2` is the phase-immune sampler the guard asserts on and `dense_hold/3` is the same
/// sampler with one live term held across it (`ref`/`ets`/`mon` — `make_ref/0`, an unnamed ETS
/// tid and a `monitor/2` reference).
///
/// `ladder/3` is the **phase probe** the module docs quote (one reading per `Step` iterations);
/// it is measured and documented, never asserted on, because which rung it reads is a property of
/// the sampling offset.
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/off_heap_reclaim.rs` (never committed).
-module(offheap_reclaim).
-export([discard/2, ladder/3, retain/3, foldl_discard/2, foldl_keep/3,
         schedule_payload/2, collect_payload/2, dense/2, dense_hold/3]).

%% One iteration: build a `Bytes`-byte OFF-HEAP binary (`bs_create_bin` above the 64-byte
%% on-heap limit), discard it, and touch the heap. The size is a RUNTIME value on purpose: a
%% compile-time constant would be folded into a literal and never reach the payload reservoir.
%% `lists:seq/2` + `length/1` are the one NON-TAIL call per iteration that makes a pre-emption
%% (and therefore a park, and therefore a prune) reachable at all: a tail-call-only body charges
%% nothing on the native tier and never parks (module docs, *The one shape this loop does not
%% bound*).
one(Bytes) ->
    _ = <<0:(Bytes*8)>>,
    L = lists:seq(1, 20),
    _ = length(L),
    ok.

churn(0, _Bytes) -> ok;
churn(N, Bytes) -> one(Bytes), churn(N - 1, Bytes).

%% N build-and-discard iterations, then the `binary_alloc` live-bytes figure in BYTES.
discard(N, Bytes) -> churn(N, Bytes), erlang:memory(binary).

%% The PHASE-IMMUNE sampler: one iteration, then `erlang:memory(binary)` after EVERY one of
%% them, folded into {Min, Max, Trough, Parks}. `Trough` is the largest reading taken immediately
%% after a reading FELL (the residue a park leaves behind) and `Parks` counts those falls; -1 is
%% the sentinel for "no reading ever fell".
dense(N, Bytes) -> dense(N, Bytes, -1, -1, -1, 0, -1).
dense(0, _Bytes, Min, Max, Trough, Parks, _Prev) -> {Min, Max, Trough, Parks};
dense(N, Bytes, Min, Max, Trough, Parks, Prev) ->
    one(Bytes),
    V = erlang:memory(binary),
    Min1 = case Min of -1 -> V; _ -> min(Min, V) end,
    Max1 = max(Max, V),
    {Trough1, Parks1} =
        case Prev >= 0 andalso V < Prev of
            true -> {max(Trough, V), Parks + 1};
            false -> {Trough, Parks}
        end,
    dense(N - 1, Bytes, Min1, Max1, Trough1, Parks1, V).

%% One live term of each reference kind, held ACROSS the loop below (the caller keeps it live by
%% using it afterwards). Every one of them is a reference thing in this runtime's term model: a
%% `make_ref/0` block, an unnamed ETS table's tid, and a `monitor/2` reference.
hold(ref) -> make_ref();
hold(ets) -> ets:new(offheap_reclaim, []);
hold(mon) -> erlang:monitor(process, self()).

%% The same dense envelope with one live reference in the frame's own root set for the whole run.
%% `is_reference/1` after the loop is what keeps `Hold` live across it (the compiler keeps it in a
%% stack slot), and it also witnesses that the held term really is a reference.
dense_hold(N, Bytes, Kind) ->
    Hold = hold(Kind),
    {Min, Max, Trough, Parks} = dense(N, Bytes),
    {{Min, Max, Trough, Parks}, erlang:is_reference(Hold)}.
%% The SAME workload driven by a `lists:foldl/3` fun-loop instead of direct recursion: the
%% lambda body builds and drops one `Bytes`-byte off-heap binary per iteration and touches no
%% heap otherwise. Returns the `binary_alloc` live-bytes figure in BYTES.
foldl_discard(N, Bytes) ->
    lists:foldl(fun(_, Acc) -> _ = <<0:(Bytes*8)>>, Acc end, ok, lists:seq(1, N)),
    erlang:memory(binary).

%% The fun-loop safety half: `KeepBytes` of 0xFF stays live in the fold accumulator across N
%% fun-loop iterations that each build and drop one `Bytes`-byte payload. The result is the
%% retained size, two content witnesses and the live-bytes figure.
foldl_keep(N, Bytes, KeepBytes) ->
    {ok, Keep} =
        lists:foldl(fun(_, {Acc, Live}) -> _ = <<0:(Bytes*8)>>, {Acc, Live} end,
                    {ok, <<(-1):(KeepBytes*8)>>}, lists:seq(1, N)),
    Before = erlang:md5(Keep),
    Fresh = <<(-1):(KeepBytes*8)>>,
    {byte_size(Keep),
     Before =:= erlang:md5(Keep),
     Keep =:= Fresh,
     erlang:memory(binary)}.

%% The same loop, reading the figure every `Step` iterations (the regression ladder).
ladder(Total, Step, Bytes) -> ladder(Total, Step, Bytes, []).
ladder(0, _Step, _Bytes, Acc) -> lists:reverse(Acc);
ladder(N, Step, Bytes, Acc) ->
    churn(Step, Bytes),
    ladder(N - Step, Step, Bytes, [erlang:memory(binary) | Acc]).

%% The safety half: KeepBytes of 0xFF (engine-built, off-heap, in the reservoir) stays live
%% across N discarded churn payloads, so every prune in that window must retain it. The result
%% is the retained size, three independent content witnesses and the live-bytes figure.
retain(N, ChurnBytes, KeepBytes) ->
    Keep = <<(-1):(KeepBytes*8)>>,
    Before = erlang:md5(Keep),
    churn(N, ChurnBytes),
    After = erlang:memory(binary),
    Fresh = <<(-1):(KeepBytes*8)>>,
    {byte_size(Keep),
     Before =:= erlang:md5(Keep),
     Keep =:= Fresh,
     binary:part(Keep, 0, 4),
     binary:part(Keep, KeepBytes - 4, 4),
     After}.

%% Statement 1 of the pending-timer case: build ONE engine-built off-heap payload, hand it to a
%% pending `erlang:send_after` (so the runtime's timer store is its only owner), then churn enough
%% further off-heap payloads to push the escaped-payload store past its budget, and return the
%% payload's md5. The parks/discards mean the prune really runs before the message is read.
schedule_payload(Bytes, Churn) ->
    Payload = <<(-1):(Bytes*8)>>,
    Digest = erlang:md5(Payload),
    _ = erlang:send_after(1000, self(), Payload),
    churn(Churn, Bytes),
    Digest.

%% Statement 2: the pending delivery must still carry the payload BYTE-EXACTLY -- same size, the
%% md5 taken before it was scheduled, `=:=` a fresh rebuild of the same bytes, and both ends
%% readable. A payload a prune freed under a live timer shows up as a mismatch here.
collect_payload(Bytes, Digest) ->
    Got = receive X -> X after 5000 -> timeout end,
    case Got of
        timeout ->
            timeout;
        _ ->
            {byte_size(Got),
             erlang:md5(Got) =:= Digest,
             Got =:= <<(-1):(Bytes*8)>>,
             binary:part(Got, 0, 4),
             binary:part(Got, Bytes - 4, 4)}
    end.
"#;
// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler beside `erlc` found in one of `dirs` (no `$PATH` read), or `None`.
///
/// Filesystem discovery only — a probe must never *run* `erlc` to find out whether it exists
/// (`tests/support/mod.rs`). Parameterised on the directory list so the loud-skip guard below
/// can ask "is an empty search path really empty?" without touching the process environment.
fn find_erlc_in(dirs: &[&Path]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join("erlc"))
        .find(|candidate| support::is_executable(candidate))
}

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        if let Some(found) = find_erlc_in(&[dir]) {
            return Some(found);
        }
    }
    let dirs: Vec<PathBuf> = std::env::var_os("PATH").map_or_else(Vec::new, |path| {
        std::env::split_paths(&path).collect()
    });
    let dirs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
    find_erlc_in(&dirs)
}

/// The resolved toolchain, or `None` after a **loud** skip naming what is missing.
///
/// Both halves are parameters so the loud-skip guard can drive either failure without a
/// missing installation on the host that runs the suite.
fn toolchain(oracle: Option<PathBuf>, erlc: Option<PathBuf>, label: &str) -> Option<Toolchain> {
    let Some(oracle) = oracle else {
        support::skip_loudly(SUITE, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
        return None;
    };
    let Some(erlc) = erlc else {
        support::skip_loudly(SUITE, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    let fixture = build_fixture(&erlc, label)?;
    Some(Toolchain { oracle, fixture })
}

/// The suite's one setup call: the live oracle, a compiler and the compiled fixture.
fn setup(label: &str) -> Option<Toolchain> {
    toolchain(support::resolve_oracle(), resolve_erlc(), label)
}

/// The live oracle plus the compiled fixture.
struct Toolchain {
    /// The system `erl` the answers are cross-checked against.
    oracle: PathBuf,
    /// The compiled fixture module.
    fixture: Fixture,
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding the fixture's `.erl` and `.beam`.
    dir: PathBuf,
    /// The compiled `.beam`.
    beam: PathBuf,
}

impl Fixture {
    /// The compiled image as a `str`, for the CLI's argv.
    fn beam_str(&self) -> &str {
        self.beam
            .to_str()
            .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", self.beam.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test, yet a
        // cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Writes [`FIXTURE_ERL`] into a fresh scratch directory and compiles it there, or `None`
/// when it cannot be compiled (the caller then returns early — **loudly**).
///
/// The directory is unique per call (label + pid + a process-wide counter), holds **only** the
/// fixture module, and is therefore safe to hand to the oracle as a single `-pa`.
fn build_fixture(erlc: &Path, label: &str) -> Option<Fixture> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_offheap_reclaim_{label}_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{MODULE}.erl"));
    if let Err(error) = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&source, FIXTURE_ERL))
    {
        support::skip_loudly(SUITE, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    // `erlc` runs with the scratch directory as cwd and the bare file name, so nothing outside
    // it can be picked up and the module is self-contained.
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg(
            source
                .file_name()
                .unwrap_or_else(|| panic!("the fixture source has no name")),
        )
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(SUITE, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            SUITE,
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let beam = dir.join(format!("{MODULE}.beam"));
    if !beam.is_file() {
        support::skip_loudly(SUITE, &format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixture { dir, beam })
}

// ---------------------------------------------------------------------------------------
// The bounded, RSS-sampling probe
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit status, captured output, peak `VmHWM` and sample count.
struct Run {
    /// The exit code (`-1` when the child died by signal).
    code: i32,
    /// Trimmed stdout — for the `run` action, exactly the workload's `~w~n` line.
    stdout: String,
    /// Trimmed stderr (must be empty on success).
    stderr: String,
    /// The largest `VmHWM` (kB) observed while the child lived; `0` when the run was missed.
    vmhwm_kib: u64,
    /// How many successful `/proc/<pid>/status` samples were taken.
    samples: usize,
}

impl Run {
    /// The peak RSS in MiB, for the printed report.
    fn vmhwm_mib(&self) -> f64 {
        self.vmhwm_kib as f64 / 1024.0
    }

    /// The single `erlang:memory(binary)` figure the workload printed, in bytes.
    fn figure(&self, label: &str) -> u64 {
        self.stdout.trim().parse().unwrap_or_else(|error| {
            panic!(
                "{label}: the workload printed {:?}, not a byte figure ({error}); stderr: {}",
                self.stdout, self.stderr
            )
        })
    }
}

/// Reads a field of `/proc/<pid>/status` such as `VmHWM:` (value in kB), or `None` when the
/// process is already gone (`/proc` entry removed) or `/proc` is not mounted.
fn read_status_field(pid: u32, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix(field)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

/// The current `/proc/loadavg` line (for the printed report), or `"unknown"`.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Runs the built `beam_rt` with `args` and samples its `/proc/<pid>/status` for `VmHWM`.
///
/// The child is **polled** rather than waited on, so a regression is a loud failure (killed,
/// reaped and reported) instead of a hang; `VmHWM` is a monotonic kernel watermark, so the
/// largest sample is the process's true peak (it is not a wall-clock measurement, which is what
/// makes it usable on a shared host).
fn run_measured(label: &str, args: &[&str]) -> Run {
    let mut child: Child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("{label}: cannot spawn {BINARY}: {error}"));
    let pid = child.id();
    let mut vmhwm_kib = 0u64;
    let mut samples = 0usize;
    let deadline = Instant::now() + PROBE_LIMIT;
    let status: Option<ExitStatus> = loop {
        if let Some(hwm) = read_status_field(pid, "VmHWM:") {
            samples += 1;
            vmhwm_kib = vmhwm_kib.max(hwm);
        }
        match child.try_wait().expect("the child is waitable") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(SAMPLE_INTERVAL),
        }
    };
    let output = child
        .wait_with_output()
        .expect("the child's output is collectable");
    let Some(status) = status else {
        panic!(
            "{label}: `{BINARY} {}` outlived the {}s probe bound",
            args.join(" "),
            PROBE_LIMIT.as_secs()
        );
    };
    let run = Run {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        vmhwm_kib,
        samples,
    };
    assert_eq!(
        run.code, 0,
        "{label}: `beam_rt {}` exited {}: {}",
        args.join(" "),
        run.code,
        run.stderr
    );
    assert!(
        !run.stderr.contains("unsupported"),
        "{label}: not a capability gap: {}",
        run.stderr
    );
    assert!(run.stderr.is_empty(), "{label}: stderr: {}", run.stderr);
    run
}

/// The `beam_rt -noshell run <beam> <Module>:<Function>/<Arity> <args…>` argv.
///
/// `-noshell` mirrors the oracle probe; the `run` action is banner-free by construction (it
/// records the `noshell` init argument), so stdout is exactly the workload's own line.
fn run_argv<'a>(fixture: &'a Fixture, spec: &'a str, args: &[&'a str]) -> Vec<&'a str> {
    let mut argv = vec!["-noshell", "run", fixture.beam_str(), spec];
    argv.extend_from_slice(args);
    argv
}

/// Runs `Module:Function/Arity` with `args` through the built runtime and returns the [`Run`].
fn run_workload(toolchain: &Toolchain, label: &str, function: &str, args: &[&str]) -> Run {
    let spec = format!("{MODULE}:{function}/{}", args.len());
    let argv = run_argv(&toolchain.fixture, &spec, args);
    run_measured(label, &argv)
}

/// The same call under the live oracle, as its printed `~w~n` line(s), or `None` when the
/// oracle could not answer (reported — never silently ignored).
fn oracle_answer(toolchain: &Toolchain, label: &str, function: &str, args: &[&str]) -> Option<String> {
    let call = format!("{MODULE}:{function}({})", args.join(","));
    let expr = format!("io:format(\"~w~n\", [{call}]), halt(0).");
    let answer = support::oracle_eval(&toolchain.oracle, Some(&toolchain.fixture.dir), &expr);
    if answer.is_none() {
        eprintln!("warning: {label}: the live oracle could not answer `{call}`");
    }
    answer
}

/// Formats a byte figure for the printed report (`N bytes (M.MM MiB, ~K churn payloads)`).
///
/// The payload count is the figure divided by one [`PAYLOAD_BYTES`] payload plus its 48-byte
/// handle — a reading aid for the sawtooth, not an assertion (a retained 64 KiB payload counts
/// as ~16 of them).
fn report(bytes: u64) -> String {
    format!(
        "{bytes} bytes ({:.2} MiB, {} payloads)",
        bytes as f64 / (1024.0 * 1024.0),
        bytes / (PAYLOAD_BYTES + 48)
    )
}

// ---------------------------------------------------------------------------------------
// 1. The 10× iteration step
// ---------------------------------------------------------------------------------------

/// **A 10× longer build-and-discard loop must not grow `erlang:memory(binary)`.**
///
/// Two separate `beam_rt run` invocations of the same workload (`discard/2`), N = 1 000 and
/// N = 10 000, each printing the `binary_alloc` live-bytes figure once at the end. With the
/// reclamation disabled the figure is linear in N — 4 144 000 bytes at N = 1 000 and 41 440 000 at
/// N = 10 000, exactly `N × (4 096 + 48)`, the 10× step adding 37 296 000 bytes — so the bound
/// below separates "bounded by one slice" from "linear in N" by an order of magnitude. Measured
/// with the prune: **4 144 at both legs** (1 payload, i.e. the read landed just after a park).
///
/// The bound is [`SINGLE_READ_BYTES`] rather than the dense [`BOUND_BYTES`] because a single read
/// samples the slice phase: one slice of this 4 KiB-payload body is ≈999 iterations = 4 139 856
/// bytes, so a read may legitimately land anywhere under that. The phase-immune statement is
/// [`the_dense_envelope_is_bounded_and_phase_immune`]'s, one test below. Peak RSS is printed too
/// (`VmHWM` is a monotonic kernel watermark, so a loaded host does not inflate it) but not
/// asserted: the absolute figure is dominated by the runtime's fixed boot cost.
#[test]
fn memory_binary_is_flat_across_a_10x_iteration_step() {
    let Some(toolchain) = setup("step") else {
        return;
    };
    let payload = PAYLOAD_BYTES.to_string();
    let small = run_workload(
        &toolchain,
        "the 10x step (N=1000)",
        "discard",
        &[&SMALL_N.to_string(), &payload],
    );
    let large = run_workload(
        &toolchain,
        "the 10x step (N=10000)",
        "discard",
        &[&LARGE_N.to_string(), &payload],
    );
    let small_bytes = small.figure("the 10x step (N=1000)");
    let large_bytes = large.figure("the 10x step (N=10000)");
    let oracle_small = oracle_answer(
        &toolchain,
        "the 10x step (N=1000)",
        "discard",
        &[&SMALL_N.to_string(), &payload],
    );
    let oracle_large = oracle_answer(
        &toolchain,
        "the 10x step (N=10000)",
        "discard",
        &[&LARGE_N.to_string(), &payload],
    );
    println!("\n[off_heap_reclaim] /proc/loadavg {}", loadavg());
    println!(
        "[off_heap_reclaim] discard(1000, {PAYLOAD_BYTES}):  erlang:memory(binary) = {} \
         | VmHWM {:.1} MiB ({} samples)",
        report(small_bytes),
        small.vmhwm_mib(),
        small.samples
    );
    println!(
        "[off_heap_reclaim] discard(10000, {PAYLOAD_BYTES}): erlang:memory(binary) = {} \
         | VmHWM {:.1} MiB ({} samples)",
        report(large_bytes),
        large.vmhwm_mib(),
        large.samples
    );
    println!(
        "[off_heap_reclaim] the 10× step adds {} bytes ({:.2} MiB), bound {SINGLE_READ_BYTES} \
         bytes ({:.2} MiB)",
        large_bytes.saturating_sub(small_bytes),
        large_bytes.saturating_sub(small_bytes) as f64 / (1024.0 * 1024.0),
        SINGLE_READ_BYTES as f64 / (1024.0 * 1024.0)
    );
    if let (Some(small_line), Some(large_line)) = (&oracle_small, &oracle_large) {
        println!(
            "[off_heap_reclaim] the live oracle: {} at N=1000, {} at N=10000",
            small_line.trim(),
            large_line.trim()
        );
    }
    assert!(
        large_bytes <= small_bytes + SINGLE_READ_BYTES,
        "the 10x iteration step must not grow erlang:memory(binary) by more than {SINGLE_READ_BYTES} \
         bytes: {} at N={SMALL_N}, {} at N={LARGE_N} (a linear, un-reclaimed reservoir adds \
         ~{} bytes)",
        report(small_bytes),
        report(large_bytes),
        (LARGE_N - SMALL_N) * (PAYLOAD_BYTES + 48)
    );
    assert!(
        large_bytes < SINGLE_READ_BYTES,
        "even in absolute terms the figure after {LARGE_N} discarded payloads must stay under \
         {SINGLE_READ_BYTES} bytes (it is bounded by the bin-vheap floor plus one pre-emption window, \
         not by N): got {}",
        report(large_bytes)
    );
}

// ---------------------------------------------------------------------------------------
// 2. The dense envelope — the phase-immune guard
// ---------------------------------------------------------------------------------------

/// One dense reading of `erlang:memory(binary)`: the sampled envelope and what it proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Envelope {
    /// The smallest reading of the run.
    min: u64,
    /// The largest — one pre-emption window's worth of payloads.
    max: u64,
    /// The largest reading taken immediately after a reading **fell** (the residue a park leaves
    /// behind), or `-1` when no reading ever fell.
    trough: i64,
    /// How many readings fell (one per park the sampler saw).
    parks: u64,
}

impl Envelope {
    /// Parses the fixture's `{Min, Max, Trough, Parks}` answer.
    fn parse(label: &str, stdout: &str) -> Envelope {
        let body = stdout.trim();
        let fields: Vec<&str> = body
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
            .unwrap_or_else(|| {
                panic!("{label}: {body:?} is not a {{Min, Max, Trough, Parks}} tuple")
            })
            .split(',')
            .map(str::trim)
            .collect();
        assert_eq!(
            fields.len(),
            4,
            "{label}: {body:?} must be {{Min, Max, Trough, Parks}}"
        );
        let number = |text: &str, what: &str| -> i64 {
            text.parse()
                .unwrap_or_else(|error| panic!("{label}: {what} {text:?}: {error}"))
        };
        Envelope {
            min: u64::try_from(number(fields[0], "min"))
                .unwrap_or_else(|_| panic!("{label}: a negative min: {body:?}")),
            max: u64::try_from(number(fields[1], "max"))
                .unwrap_or_else(|_| panic!("{label}: a negative max: {body:?}")),
            trough: number(fields[2], "trough"),
            parks: u64::try_from(number(fields[3], "parks"))
                .unwrap_or_else(|_| panic!("{label}: a negative park count: {body:?}")),
        }
    }

    /// Parses the `dense_hold/3` answer `{{Min, Max, Trough, Parks}, true}` — the trailing `true`
    /// is the fixture's own witness that the held term stayed live across the loop and really is
    /// a reference.
    fn parse_hold(label: &str, stdout: &str) -> Envelope {
        let body = stdout.trim();
        let inner = body
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix(",true}"))
            .unwrap_or_else(|| {
                panic!(
                    "{label}: {body:?} must be {{{{Min, Max, Trough, Parks}}, true}} — the held \
                     term must stay live (and be a reference) across the whole loop"
                )
            });
        Envelope::parse(label, inner)
    }

    /// The three properties a reservoir that is **released at every park** shows, each of which a
    /// retaining product breaks (module docs, *The phase-immune reading*):
    ///
    /// * [`MIN_PARKS`] parks really happened, so flatness cannot be vacuous;
    /// * the trough is under [`SMALL_BYTES`] — the assertion that names a per-park residue;
    /// * the peak is under [`BOUND_BYTES`] — one pre-emption window plus the bin-vheap floor,
    ///   which a per-iteration residue exceeds by an order of magnitude.
    fn assert_bounded(&self, label: &str, runs: &str) {
        assert!(
            self.parks >= MIN_PARKS,
            "{label}: the sampler must see at least {MIN_PARKS} parks — a park is the only thing \
             that runs the prune, so a reservoir nothing emptied would look flat too: {self:?} \
             ({runs})"
        );
        assert!(
            self.trough >= 0,
            "{label}: no reading ever fell, so nothing was ever released — every payload the run \
             built is still live: {self:?} ({runs})"
        );
        assert!(
            (self.trough as u64) <= SMALL_BYTES,
            "{label}: the post-park trough must stay under {SMALL_BYTES} bytes ({} payloads of \
             {DENSE_BLOCK_BYTES}) — a residue of one payload per park reaches {} bytes over {} \
             parks: {self:?} ({runs})",
            SMALL_BYTES / DENSE_BLOCK_BYTES,
            self.parks * DENSE_BLOCK_BYTES,
            self.parks
        );
        assert!(
            self.max <= BOUND_BYTES,
            "{label}: the peak must stay under {BOUND_BYTES} bytes ({} payloads: one pre-emption \
             window of 444 iterations plus the 371 376-byte bin-vheap floor) — a reservoir nothing \
             released is one payload per iteration, {} bytes over {} iterations: {self:?} ({runs})",
            BOUND_BYTES / DENSE_BLOCK_BYTES,
            self.max / DENSE_BLOCK_BYTES * DENSE_BLOCK_BYTES,
            self.max / DENSE_BLOCK_BYTES
        );
    }
}

/// Formats a dense figure for the printed report (`N bytes (M.MM MiB, ~K 1 KiB payloads)`).
fn report_dense(bytes: u64) -> String {
    format!(
        "{bytes} bytes ({:.2} MiB, {} x {DENSE_PAYLOAD_BYTES}-byte payloads)",
        bytes as f64 / (1024.0 * 1024.0),
        bytes / DENSE_BLOCK_BYTES
    )
}

/// **The dense `erlang:memory(binary)` envelope is one pre-emption window, and does not move with
/// the loop count.**
///
/// One process per leg, `erlang:memory(binary)` read after **every** iteration and folded into
/// `{min, max, trough, parks}` (the ladder this replaced sampled a fixed offset, which is a slice
/// phase — see the module docs). `DENSE_SMALL_N` and `DENSE_N` are the 8× step.
///
/// Asserted, each half able to fail:
///
/// * **non-inertness** — the peak reaches [`MIN_PEAK_BYTES`] (64 payloads; measured 444) and the
///   sampler saw at least [`MIN_PARKS`] parks (measured 5 and 45), so the envelope is a real
///   sawtooth rather than the flatness of a reservoir nothing fed or nothing emptied;
/// * **the peak is one window** — `max <= BOUND_BYTES` (1 MiB, derived from
///   `DEFAULT_REDUCTIONS_PER_SLICE` ÷ 9 reductions per iteration × 1 072 bytes plus the bin-vheap
///   floor; measured 475 968 = 2.2× under) on both legs;
/// * **the trough is a per-park residue's opposite** — `trough <= SMALL_BYTES` (4 payloads;
///   measured exactly 1) on both legs;
/// * **the step does not move either** — the large leg's peak is within 2× of the small leg's
///   (measured: identical) and its trough is within two payloads of it, so a residue that grows
///   with the park count (5 × 1 072 vs 45 × 1 072) fails while a phase cannot.
///
/// Never exact bytes: these are live allocator readings, and the printed figures are the
/// measurement of record.
#[test]
fn the_dense_envelope_is_bounded_and_phase_immune() {
    let Some(toolchain) = setup("dense") else {
        return;
    };
    let payload = DENSE_PAYLOAD_BYTES.to_string();
    let small_n = DENSE_SMALL_N.to_string();
    let large_n = DENSE_N.to_string();
    let small = run_workload(
        &toolchain,
        "the dense envelope (small leg)",
        "dense",
        &[&small_n, &payload],
    );
    let large = run_workload(
        &toolchain,
        "the dense envelope (large leg)",
        "dense",
        &[&large_n, &payload],
    );
    let small = Envelope::parse("the dense envelope (small leg)", &small.stdout);
    let large = Envelope::parse("the dense envelope (large leg)", &large.stdout);
    let runs = format!("dense({DENSE_SMALL_N}, {DENSE_PAYLOAD_BYTES}) = {small:?}, dense({DENSE_N}, {DENSE_PAYLOAD_BYTES}) = {large:?}");
    println!("\n[off_heap_reclaim] /proc/loadavg {}", loadavg());
    println!("[off_heap_reclaim] {runs}");
    println!(
        "[off_heap_reclaim] bound {BOUND_BYTES} bytes ({} payloads) | trough bound {SMALL_BYTES} \
         bytes ({} payloads) | non-vacuity {MIN_PEAK_BYTES} bytes and {MIN_PARKS} parks",
        BOUND_BYTES / DENSE_BLOCK_BYTES,
        SMALL_BYTES / DENSE_BLOCK_BYTES
    );
    for (label, envelope) in [
        ("the small leg", small),
        ("the large leg", large),
    ] {
        assert!(
            envelope.max >= MIN_PEAK_BYTES,
            "{label}: the loop must really accumulate between two parks — a peak of at least \
             {MIN_PEAK_BYTES} bytes ({MIN_PEAK_PAYLOADS} payloads) — otherwise the envelope proves \
             nothing: {envelope:?} ({runs})"
        );
        assert!(
            envelope.max > envelope.min,
            "{label}: the sampler must see a sawtooth (max > min), not one constant reading: \
             {envelope:?} ({runs})"
        );
        envelope.assert_bounded(label, &runs);
    }
    assert!(
        large.max <= 2 * small.max,
        "the 8x iteration step must not grow the peak: {} on the small leg, {} on the large one \
         ({runs})",
        report_dense(small.max),
        report_dense(large.max)
    );
    assert!(
        large.trough <= small.trough.max(2 * DENSE_BLOCK_BYTES as i64),
        "the trough must not grow with the park count: {} on the small leg ({} parks), {} on the \
         large one ({} parks) ({runs})",
        report_dense(small.trough.max(0) as u64),
        small.parks,
        report_dense(large.trough.max(0) as u64),
        large.parks
    );
}

// ---------------------------------------------------------------------------------------
// 3. The reference exemption — a live reference may not cost the reservoir anything
// ---------------------------------------------------------------------------------------

/// **A live reference in the root set costs the off-heap reservoir nothing.**
///
/// The prune-only valve (`src/context/gc_walk.rs`, `Criterion::PayloadPrune`) must not treat a
/// reference thing as a hazard: a reference block is a childless leaf whose payload words are
/// terms or packed `u32` ids, never a `BinRef.val` pointer, so it can name no off-heap payload.
/// When the valve treated it as a hazard, **any** live reference — `make_ref/0`, an unnamed ETS
/// table's tid, a `monitor/2` or `alias/1` reference, i.e. what every `gen_server` holds — declined
/// the prune at every park and the reservoir was retained for ever: `erlang:memory(binary)` read
/// exactly `N × 2 416` bytes with zero releases, **4 832 000** at N = 2 000 and **19 328 000** at
/// N = 8 000 (1 024-byte payloads), while the reference-free control stayed flat.
///
/// The same fixture with one such reference held live across the whole loop
/// (`dense_hold/3`'s `ref`, `ets` and `mon`) must therefore show the **control's** envelope: the
/// same [`BOUND_BYTES`] peak, the same [`SMALL_BYTES`] trough, [`MIN_PARKS`] parks and a peak
/// within 2× of the control's. On the pre-fix product the peak was 4.6× over `BOUND_BYTES` (and
/// 18.4× at N = 8 000) with no trough at all, so both the peak assertion and the
/// `trough >= 0` / `parks` half of [`Envelope::assert_bounded`] were red.
///
/// The held term is a genuine root: the fixture keeps `Hold` live to the end of the frame (its
/// `is_reference/1` witness is what tells the compiler so), and the prune's census over the same
/// run shows the walk descending with **0 declines** — the reference is reached and classified,
/// it is simply inert.
#[test]
fn a_live_reference_in_the_root_set_costs_the_reservoir_nothing() {
    let Some(toolchain) = setup("dense_hold") else {
        return;
    };
    let payload = DENSE_PAYLOAD_BYTES.to_string();
    let n = DENSE_N.to_string();
    let control_run = run_workload(
        &toolchain,
        "the reference-free control",
        "dense",
        &[&n, &payload],
    );
    let control = Envelope::parse("the reference-free control", &control_run.stdout);
    println!("\n[off_heap_reclaim] /proc/loadavg {}", loadavg());
    println!(
        "[off_heap_reclaim] control dense({DENSE_N}, {DENSE_PAYLOAD_BYTES}) = {control:?} | VmHWM \
         {:.1} MiB ({} samples)",
        control_run.vmhwm_mib(),
        control_run.samples
    );
    control.assert_bounded(
        "the reference-free control",
        &format!("dense({DENSE_N}, {DENSE_PAYLOAD_BYTES})"),
    );
    for (kind, what) in [
        ("ref", "a make_ref/0 reference"),
        ("ets", "an unnamed ETS table's tid"),
        ("mon", "a monitor/2 reference"),
    ] {
        let label = format!("a live {what}");
        let run = run_workload(&toolchain, &label, "dense_hold", &[&n, &payload, kind]);
        let held = Envelope::parse_hold(&label, &run.stdout);
        let runs = format!("dense_hold({DENSE_N}, {DENSE_PAYLOAD_BYTES}, {kind}) = {held:?}");
        println!(
            "[off_heap_reclaim] {label} ({kind}) = {held:?} | VmHWM {:.1} MiB ({} samples)",
            run.vmhwm_mib(),
            run.samples
        );
        held.assert_bounded(&label, &runs);
        assert!(
            held.max <= 2 * control.max,
            "{label}: a live reference may not cost the reservoir anything — its peak must stay \
             within 2x the reference-free control's ({}), but it is {} ({runs})",
            report_dense(control.max),
            report_dense(held.max)
        );
        assert!(
            held.trough <= control.trough.max(2 * DENSE_BLOCK_BYTES as i64),
            "{label}: a live reference may not leave a residue behind — the trough must match the \
             control's ({}), but it is {} ({runs})",
            report_dense(control.trough.max(0) as u64),
            report_dense(held.trough.max(0) as u64)
        );
    }
}

// ---------------------------------------------------------------------------------------
// 4. The safety half — a pruned payload must never be a live one
// ---------------------------------------------------------------------------------------

/// **A retained off-heap binary survives every prune byte-exactly.**
///
/// The anti-dangle half of the reclamation, and as important as the flatness assertions: a
/// 64 KiB engine-built payload (`<<(-1):(KeepBytes*8)>>`, all `0xFF`) stays live across
/// [`RETAIN_CHURN_N`] discarded 4 KiB payloads — so it is re-scanned by every collection and by
/// every prune in that window — and must come back:
///
/// * of the same size, with an unchanged `erlang:md5/1`;
/// * `=:=` a **fresh** rebuild of the same bytes (the churn payloads are zero-filled, so a
///   freed-and-reused buffer shows up here);
/// * readable at both ends of the payload (first and last four bytes `<<255,255,255,255>>`);
/// * **still counted** in `erlang:memory(binary)` (its payload was retained, not released);
/// * **and identical to the live oracle's own answer** for the same call.
///
/// The churn is `RETAIN_CHURN_N × (PAYLOAD_BYTES + 48)` = ~24 MiB of payloads, three times
/// [`SINGLE_READ_BYTES`], so a run whose churn was never pruned cannot satisfy the "under the
/// bound" assertion either. Measured: `{65 536, true, true, <<255,255,255,255>>,
/// <<255,255,255,255>>, 69 728}` — the retained payload plus exactly one churn payload.
#[test]
fn a_retained_off_heap_binary_survives_every_prune_byte_exactly() {
    let Some(toolchain) = setup("retain") else {
        return;
    };
    let args = [
        RETAIN_CHURN_N.to_string(),
        PAYLOAD_BYTES.to_string(),
        KEEP_BYTES.to_string(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = run_workload(&toolchain, "the retained payload", "retain", &args);
    // `{Size, Md5Unchanged, EqualToRebuild, Head, Tail, Bytes}`.
    let prefix = format!(
        "{{{KEEP_BYTES},true,true,<<255,255,255,255>>,<<255,255,255,255>>,"
    );
    println!("\n[off_heap_reclaim] /proc/loadavg {}", loadavg());
    println!(
        "[off_heap_reclaim] retain({RETAIN_CHURN_N}, {PAYLOAD_BYTES}, {KEEP_BYTES}) = {}",
        run.stdout
    );
    println!(
        "[off_heap_reclaim] VmHWM {:.1} MiB ({} samples)",
        run.vmhwm_mib(),
        run.samples
    );
    let oracle_line = oracle_answer(&toolchain, "the retained payload", "retain", &args)
        .unwrap_or_else(|| panic!("the live oracle could not answer the retained-payload call"));
    println!(
        "[off_heap_reclaim] the live oracle: {}",
        oracle_line.trim()
    );
    let rest = run.stdout.strip_prefix(&prefix).unwrap_or_else(|| {
        panic!(
            "the retained {KEEP_BYTES}-byte payload must come back byte-exact (expected the \
             prefix {prefix:?}); got {}",
            run.stdout
        )
    });
    let figure = rest.strip_suffix('}').unwrap_or_else(|| {
        panic!("the retained-payload result is not a tuple: {}", run.stdout)
    });
    let bytes: u64 = figure
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("{figure:?} is not a byte figure: {error}"));
    assert!(
        bytes >= KEEP_BYTES,
        "the retained payload must still be counted in erlang:memory(binary) ({KEEP_BYTES} bytes \
         of 0xFF payload); got {}",
        report(bytes)
    );
    assert!(
        bytes < SINGLE_READ_BYTES,
        "the churn's {RETAIN_CHURN_N} discarded payloads must have been pruned around the \
         retained one, so the figure stays under {SINGLE_READ_BYTES} bytes; got {}",
        report(bytes)
    );
    // The oracle's own answer must agree on everything but the figure: same size, same MD5
    // verdict, same rebuild comparison, same payload ends. (Its live-bytes figure is its own
    // allocator's reading, so it is printed, not compared.)
    let oracle_prefix = oracle_line
        .trim()
        .rsplit_once(',')
        .map(|(head, _figure)| format!("{head},"))
        .unwrap_or_default();
    assert_eq!(
        oracle_prefix, prefix,
        "the content witnesses must be the live oracle's answer too"
    );
}

// ---------------------------------------------------------------------------------------
// 5. The loud skip
// ---------------------------------------------------------------------------------------

/// **A missing toolchain is a loud skip, never a silent pass and never a panic.**
///
/// The suite's whole setup is gated on two filesystem discoveries — the live oracle and a
/// compiler — and either being absent must *decline* the suite (the shared `SKIPPED` /
/// `NOT a pass` banner on stderr) rather than build no fixture and assert nothing silently.
/// This test drives both refusals explicitly, and checks that an empty search path really
/// resolves no compiler, so the guard cannot rot into "always true".
#[test]
fn a_missing_toolchain_is_a_loud_skip_never_a_silent_pass() {
    // An empty search path: nothing is found, and the discovery never *runs* the candidate.
    assert!(
        find_erlc_in(&[]).is_none(),
        "an empty search path resolves no `erlc`"
    );
    let empty = std::env::temp_dir().join("beam_rt_offheap_reclaim_no_such_dir");
    assert!(
        find_erlc_in(&[empty.as_path()]).is_none(),
        "a directory without `erlc` resolves no compiler"
    );
    // Both halves of the gate decline — loudly (the banner goes to this test's stderr, which
    // `--nocapture` shows) and without building a fixture, so there is nothing to assert
    // vacuously.
    let no_oracle = toolchain(None, Some(PathBuf::from("/nonexistent/erlc")), "skip-no-oracle");
    assert!(
        no_oracle.is_none(),
        "no oracle must decline the suite, not run it half-way"
    );
    let no_compiler = toolchain(
        Some(PathBuf::from("/nonexistent/erl")),
        None,
        "skip-no-erlc",
    );
    assert!(
        no_compiler.is_none(),
        "no `erlc` must decline the suite, not run it half-way"
    );
    println!(
        "[off_heap_reclaim] the loud-skip guard holds: a missing toolchain declines the suite \
         (banner on stderr, NOT a pass), it never reports a measurement"
    );
}


// ---------------------------------------------------------------------------------------------
// The **fun-loop** shapes: the same build-and-drop payload written by a `lists:foldl/3` lambda
// instead of direct recursion.
//
// These two cases exist because the fun-loop arm is a **different execution path** from the
// direct-recursion one the cases above drive: `lists:foldl/3` is entered as a *nested* external
// call, and each application of the lambda is served by
// `RuntimeContext::apply_fun` -> `run_label_blocking`'s own short-lived `Machine`, whose payload
// reservoir is handed to `Runtime::adopt_escaped_binaries` when that machine drops
// (`src/context/pump.rs`, `src/runtime.rs`). The payloads therefore live in the runtime's
// **escaped-payload store**, not in the process's `Machine::refc_binaries` reservoir that the
// slice-boundary prune walks.
//
// Both halves are green. The safety half (`foldl_keep`) proves a payload a live root names is
// never freed; the release half (`foldl_discard`) proves the fun-loop envelope is bounded,
// because that store is pruned at the nested-call adopt site against the run's own published
// root set (`RuntimeContext::adopt_nested_payloads`).
// ---------------------------------------------------------------------------------------------

/// The fun-loop **safety** half: a payload the fold accumulator keeps live across
/// [`SMALL_N`] fun-loop iterations that each build and discard a [`PAYLOAD_BYTES`]-byte
/// payload must survive byte-exactly.
///
/// This is the anti-dangle case for the fun-loop arm: however the loop's own payloads are (or
/// are not) released, a payload a *live* root names must never be. Content is checked two
/// independent ways -- the md5 taken before the churn and a fresh rebuild compared `=:=` --
/// plus the size.
#[test]
fn a_retained_off_heap_binary_survives_the_fun_loop_byte_exactly() {
    let Some(toolchain) = setup("foldl_keep") else {
        return;
    };
    let run = run_workload(
        &toolchain,
        "the fun-loop safety half",
        "foldl_keep",
        &[
            &SMALL_N.to_string(),
            &PAYLOAD_BYTES.to_string(),
            &KEEP_BYTES.to_string(),
        ],
    );
    println!("\n[off_heap_reclaim] /proc/loadavg {}", loadavg());
    println!(
        "[off_heap_reclaim] fun-loop retain ({SMALL_N} x {PAYLOAD_BYTES} B discarded, \
         {KEEP_BYTES} B kept): {}",
        run.stdout.trim()
    );
    let fields: Vec<&str> = run
        .stdout
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .split(',')
        .collect();
    assert_eq!(
        fields.len(),
        4,
        "the fun-loop safety half answers {{Size, md5 matches, =:= a rebuild, live bytes}}: {}",
        run.stdout
    );
    assert_eq!(
        fields[0], KEEP_BYTES.to_string(),
        "the retained payload is still {KEEP_BYTES} bytes: {}",
        run.stdout
    );
    assert_eq!(
        fields[1], "true",
        "the retained payload's content is unchanged (a pruned payload would dangle): {}",
        run.stdout
    );
    assert_eq!(
        fields[2], "true",
        "the retained payload is `=:=` a fresh rebuild of the same bytes: {}",
        run.stdout
    );
}

/// The fun-loop **release** half: the envelope must be bounded by the binary-virtual-heap
/// budget and flat in N.
///
/// The fun-loop body builds and drops one [`PAYLOAD_BYTES`]-byte off-heap binary per iteration,
/// exactly like the direct-recursion cases above. The lambda is applied through `apply_fun` ->
/// `run_label_blocking`, whose per-call `Machine` hands its reservoir to
/// `Runtime::adopt_escaped_binaries` when it drops, so the payloads land in the runtime's
/// escaped-payload store and are released there: that store is pruned at the same nested-call
/// epilogue, against the run's own published root set (its X registers, its whole Erlang stack,
/// its FR file and the term it is about to return).
///
/// The store is budgeted by `STORE_BUDGET_WORDS` (46 422 words, the C `BIN_VH_MIN_SIZE`), so its
/// envelope is a converged capacity, not a function of N — and one *read* of it is the same slice
/// phase the module docs describe, on this route it is the fold's own nested call that has to end
/// before the store is pruned. So the figure `foldl_discard(N, 4 096)` reports is
/// `(N − last park) × 4 144`, measured: **4 144 000** at N = 1 000 (nothing of the slice had been
/// released yet — the whole run), **16 576** at N = 10 000 (four payloads). At 1 024-byte payloads
/// the same aliasing is visible step by step: `foldl_discard(1 995)` = 2 138 640,
/// `(1 996)` = 0, `(1 997)` = 1 072, `(3 000)` = 1 076 288, `(4 000)` = 4 288, `(10 000)` = 4 288 —
/// one park per ~1 996 iterations, and every reading is the phase *after* the last one. It is not
/// a retention family: the envelope does not grow with N.
///
/// That is why the assertion is structural on purpose — bounded by [`SINGLE_READ_BYTES`], and no
/// growth from the small to the large N — never an exact byte figure and never a wall-clock claim.
#[test]
fn the_fun_loop_envelope_is_bounded_by_the_bin_vheap_budget() {
    let Some(toolchain) = setup("foldl_discard") else {
        return;
    };
    let mut figures = Vec::new();
    for n in [SMALL_N, LARGE_N] {
        let run = run_workload(
            &toolchain,
            "the fun-loop envelope",
            "foldl_discard",
            &[&n.to_string(), &PAYLOAD_BYTES.to_string()],
        );
        let bytes: u64 = run
            .stdout
            .trim()
            .parse()
            .unwrap_or_else(|error| panic!("foldl_discard printed {:?}: {error}", run.stdout));
        figures.push((n, bytes));
    }
    println!("\n[off_heap_reclaim] /proc/loadavg {}", loadavg());
    println!("[off_heap_reclaim] fun-loop envelope in bytes: {figures:?}");
    for (n, bytes) in &figures {
        assert!(
            *bytes <= SINGLE_READ_BYTES,
            "a {n}-iteration fun-loop holds {bytes} bytes of off-heap payloads, past the \
             {SINGLE_READ_BYTES}-byte single-read bound (a reservoir bounded by one slice never \
             exceeds it): {figures:?}"
        );
    }
    let (_, small) = figures[0];
    let (_, large) = figures[1];
    assert!(
        large <= small.saturating_mul(2),
        "the fun-loop envelope grows with N ({small} -> {large}) instead of converging on the \
         binary-virtual-heap budget: {figures:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The **pending-timer** root family of the `-eval` route's escaped-payload store
//
// `Runtime::eval` builds one runtime per call, and this suite's `run`-route cases above cannot see
// the store at all (the `-eval` action process never parks), so this case is driven in process
// through the same fixture: `Runtime::new` -> one `eval` of a two-statement expression.
//
// Statement 1 builds ONE engine-built off-heap payload, hands it to a pending
// `erlang:send_after`, churns `TIMER_CHURN_N` further off-heap payloads (which is what pushes the
// store past `STORE_BUDGET_WORDS` and makes the statement boundary prune actually run) and
// returns; statement 2 receives the pending message and compares it against a fresh rebuild. The
// timer store is the payload's only owner across the boundary, and the scheduling path
// **stabilizes** the message (`try_stabilize_escaped` -> `copy_struct_simple` with an empty source
// table), which flattens a refc `[BinRef][ErlSubBits]` pair on-heap -- so the payload half of this
// family is a *flattening* pin: the bytes must come back exact, and the prune must have run.
// ---------------------------------------------------------------------------------------------

/// The churn of the pending-timer case: `TIMER_CHURN_N × PAYLOAD_BYTES` = 819 200 bytes, well past
/// `STORE_BUDGET_WORDS` (46 422 words ≈ 371 kB), so the statement boundary's prune is due.
const TIMER_CHURN_N: u64 = 200;

/// The most payload bytes the store may still hold after the boundary's prune: the surviving
/// handles are one pre-emption window's worth at most, and the churn is 200 payloads, so eight is
/// a decisive bound.
const MAX_RETAINED_PAYLOAD_BYTES: u64 = 8 * PAYLOAD_BYTES;

/// The two statements the case evaluates: schedule-then-churn (binding the payload's md5), then
/// receive-and-compare.
fn timer_payload_expression(bytes: u64, churn: u64) -> String {
    format!(
        "Digest = {MODULE}:schedule_payload({bytes}, {churn}), \
         {MODULE}:collect_payload({bytes}, Digest)"
    )
}

/// **A payload named only by a pending timer delivery must survive the statement-boundary prune
/// byte-exactly, and the prune must really run.**
///
/// Asserts:
/// * the answer is `{4096, true, true, <<255,255,255,255>>, <<255,255,255,255>>}` — the delivered
///   payload's size, its md5 unchanged, `=:=` a fresh rebuild of the same bytes, and both ends
///   readable — *and* it is the live oracle's answer for the same module;
/// * `eval_off_heap_stats().prunes > 0` — the boundary rewrite really happened (the churn crossed
///   the budget), so the byte-exactness is not the trivial case of an un-pruned store;
/// * `declines == 0` — nothing refused the walk; and
/// * the store holds at most [`MAX_RETAINED_PAYLOAD_BYTES`] afterwards, i.e. the churn's 200
///   payloads were released around the live one.
#[test]
fn a_binary_named_only_by_a_pending_timer_survives_the_eval_statement_prune_byte_exactly() {
    let Some(toolchain) = setup("timer_payload") else {
        return;
    };
    let expression = timer_payload_expression(PAYLOAD_BYTES, TIMER_CHURN_N);
    let config = RuntimeConfig::default().with_code_paths(vec![toolchain.fixture.dir.clone()]);
    let mut runtime = Runtime::new(config).expect("the -eval runtime");
    let value = runtime
        .eval(&expression)
        .unwrap_or_else(|error| panic!("the pending-timer expression failed: {error:?}"));
    let answer = runtime
        .format_term_w(value, None)
        .expect("the witnesses format")
        .trim()
        .to_string();
    let stats = runtime.eval_off_heap_stats();
    let oracle_line =
        support::oracle_eval(&toolchain.oracle, Some(&toolchain.fixture.dir), &{
            // `begin … end`, not parentheses: `-eval` takes one *expression*, and only `begin`
            // sequences several of them without a parse error.
            format!("io:format(\"~w~n\", [begin {expression} end]), halt(0).")
        });
    println!("\n[off_heap_reclaim] /proc/loadavg {}", loadavg());
    println!(
        "[off_heap_reclaim] the pending-timer payload ({TIMER_CHURN_N} x {PAYLOAD_BYTES} B churned \
         past the store budget): {answer}"
    );
    println!(
        "[off_heap_reclaim] eval store after the boundary: prunes={} declines={} handles={} \
         retained={} bytes",
        stats.prunes, stats.declines, stats.handles, stats.retained_bytes
    );
    if let Some(line) = &oracle_line {
        println!("[off_heap_reclaim] the live oracle: {}", line.trim());
    }
    let expected = format!(
        "{{{PAYLOAD_BYTES},true,true,<<255,255,255,255>>,<<255,255,255,255>>}}"
    );
    assert_eq!(
        answer, expected,
        "the payload named only by the pending timer must come back byte-exact (its md5 unchanged, \
         `=:=` a fresh rebuild, both ends readable)"
    );
    assert!(
        stats.prunes > 0,
        "the boundary must really have pruned the store ({TIMER_CHURN_N} payloads of churn, budget \
         46 422 words), otherwise the byte-exactness proves nothing: {stats:?}"
    );
    assert_eq!(
        stats.declines, 0,
        "nothing in this root graph may decline: {stats:?}"
    );
    assert!(
        (stats.retained_bytes as u64) <= MAX_RETAINED_PAYLOAD_BYTES,
        "the churn's payloads must have been released around the live one: {} bytes still \
         retained ({stats:?})",
        stats.retained_bytes
    );
    if let Some(line) = &oracle_line {
        assert_eq!(
            answer,
            line.trim(),
            "the same two statements must answer the live oracle's own witnesses"
        );
    }
}
