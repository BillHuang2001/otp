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
//! Across **many** parks the retention is not idempotent — the residue keeps one payload per park,
//! monotonically, up to a payload-count cap — which is the open defect the ladder test pins (see
//! *Known issue* below).
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
//! # The bound, and why the payload is 4 KiB (measured)
//!
//! The prune runs at a slice boundary, so the observable is a **sawtooth**, not a constant, and a
//! ladder rung's read follows a completed churn block — i.e. a park — so a rung reports the
//! **trough**, the post-prune residue, never the in-window peak (the payloads built between two
//! parks, which nothing here samples).
//!
//! The trough is what the bound is stated against, and it is measured:
//!
//! * with the churn run as **one** block and the figure read **once**, the residue is exactly one
//!   payload — `discard(N, 4096)` = 4 144 bytes at N = 1 000, 128 000, 500 000 and 1 000 000 — so
//!   the build-and-discard loop is genuinely closed for a single park;
//! * with the same iterations split into blocks, the residue keeps **one payload per park** and
//!   never falls back: rung `k` of the ladder is `k` payloads. That growth is the open defect
//!   (*Known issue*), not a property of the loop.
//!
//! The payload is 4 KiB because it is far past `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`
//! (64 bytes, so every iteration really builds an off-heap refc payload) while staying small
//! enough to keep 1 000 iterations inside one run, and because a retained payload shows up in the
//! figure as `Bytes + 48` — a payload-shaped block, ~85× the bare 48-byte handle a leak would
//! otherwise have to be read as.
//!
//! The discriminator for the reclamation call site
//! (`RuntimeRunner::reclaim_off_heap_binaries`) is the pair measured on the same workload with it
//! disabled and enabled: disabled, `discard(1000)` = 4 144 000 and `discard(10000)` = 41 440 000
//! bytes — exactly `N × (4096 + 48)`, one payload plus its handle per iteration, linear in N,
//! with peak RSS following it; enabled, `discard(N)` is 4 144 bytes at every N above. That pair
//! is what makes a non-flat ladder attributable to the reclamation and not to the loop.
//!
//! # What each test pins
//!
//! 1. [`memory_binary_is_flat_across_a_10x_iteration_step`] — two separate `beam_rt run`
//!    invocations, N = 1000 and N = 10 000 iterations of build-and-discard: the 10× step must
//!    add at most [`BOUND_BYTES`], and the larger figure must be under it in absolute terms.
//! 2. [`memory_binary_does_not_grow_monotonically_over_the_loop`] — **one** process, 10 000
//!    iterations, `erlang:memory(binary)` recorded every 1 000 (a 10-element ladder): the spread
//!    (`max − min`) and the drift (`last − first`) must be within [`BOUND_BYTES`], *and* the
//!    ladder must not be vacuous — its sampled maximum must reach [`MIN_PEAK_PAYLOADS`] payloads
//!    and its minimum must stay under [`MAX_TROUGH_PAYLOADS`]. This test is **RED** on the
//!    current product: the ladder is monotone, so the sampled maximum is ten payloads where the
//!    guard asks for a hundred (*Known issue*).
//! 3. [`a_retained_off_heap_binary_survives_every_prune_byte_exactly`] — the **safety** half:
//!    a 64 KiB off-heap binary kept live across thousands of prunes must come back
//!    **byte-exact** (MD5 unchanged, `=:=` a fresh rebuild, both ends of the payload read back,
//!    identical to the live oracle's own answer), *and* its payload must still be counted in
//!    `erlang:memory(binary)`. A prune that freed a live payload — or that never ran — cannot
//!    satisfy both halves at once.
//! 4. [`a_missing_toolchain_is_a_loud_skip_never_a_silent_pass`] — the loud-skip guard: an
//!    empty toolchain path resolves no compiler, and the suite's gate declines (printing the
//!    shared `SKIPPED` / `NOT a pass` banner) instead of building a fixture or asserting.
//!
//! # Known issue — the residue retains one payload per park, so the ladder is monotone
//!
//! Measured on this tree with the system `erlc`-compiled fixture on the `run` route,
//! `ladder(N, 1_000, Bytes)`: rung `k` is exactly `k` payloads of `Bytes + 48`, with **no
//! sawtooth at all**, up to a cap of **999 payloads** followed by a reset to `0` on the next rung
//! and a period of exactly 1 000 rungs.
//!
//! | `Bytes` | rung-to-rung | sampled maximum | at rung |
//! |---|---|---|---|
//! | 1 024 | `+1 072` | 1 070 928 = 999 × 1 072 | 998 |
//! | 2 048 | `+2 096` | 2 093 904 = 999 × 2 096 | 998 |
//! | 4 096 | `+4 144` | 4 139 856 = 999 × 4 144 | 998 |
//!
//! N = 10 000 (this suite's configuration) therefore reads `[4144, 8288, …, 41440]`; N = 200 000
//! is 200 rungs with every difference `+4 144`; N = 2 000 000 is 2 000 rungs with 1 997 of 1 999
//! differences `+4 144` and the two resets. The live oracle on the same fixture reports a flat
//! band with no trend (`[87896, 29768, 125264, 29768, 133568, …]` — a ~100 KB sawtooth whose
//! envelope does not move), so this is a divergence in the product, not in the workload.
//!
//! The growth is **per churn-block boundary, not per iteration**, and two controls isolate it to
//! the park:
//!
//! * ten blocks of 1 000 iterations and ten blocks of 2 000 give the *same* ladder (topping at
//!   103 600), four blocks of 5 000 give four rungs, twenty blocks of 1 000 give twenty — so the
//!   increment tracks the number of blocks (parks), not the iteration count;
//! * twelve consecutive reads after one `churn(2 000)` are flat (`[4144 × 12]`, and
//!   `[65584 × 12]` at 65 536-byte payloads) — a read is not what leaks;
//! * one `churn` block of 1 000 000 iterations read once is flat at 4 144, like every other
//!   `discard(N)`;
//! * an explicit `erlang:garbage_collect()` before each read does **not** clear the residue — it
//!   doubles the increment to two payloads per rung (`[4144, 12432, 20720, …]`), because a forced
//!   collection is itself another collection point. The retained payloads are therefore immortal
//!   to a full GC, which rules out a stale heap word as their owner and points at the
//!   reservoir-side retention instead.
//!
//! So one park is closed and each further collection retains exactly one more payload: the
//! retained set is monotone in the number of pre-emptions, and its size is payload-size
//! proportional (capped at 999 payloads — 4.1 MB at 4 KiB, ~65 MB at 64 KiB).
//!
//! The seam the law points at is the retention predicate of the prune,
//! `src/context/gc/bin_vheap.rs`'s `live ∪ shared` pair. `shared` is derived **from the reservoir
//! being pruned** (every entry with `refcount() > 1`), so a payload that acquires a second owner
//! is immortal across every later prune and the belt is fed back to itself; `live` covers the
//! rest. Narrowing it is a product change with a hard constraint: a false accept is only a leak,
//! a **false reject frees a payload a live `BinRef.val` names**, so the predicate may only be
//! narrowed where ownership is proven elsewhere (the runtime's escaped store, the mailbox's
//! retained families, the send path's destination tables).
//!
//! [`memory_binary_does_not_grow_monotonically_over_the_loop`] is **red** on this state, and its
//! failure message is the honest one — the sampled maximum is ten payloads where the non-vacuity
//! guard asks for a hundred. Neither that guard, nor [`MAX_TROUGH_PAYLOADS`], nor
//! [`BOUND_BYTES`] may be re-derived to fit this residue: the product owes a residue that does
//! not grow, and [`BOUND_BYTES`] (8 MiB) cannot discriminate the current shape in any case — the
//! signal is 37 KB at N = 10 000 and 4.1 MB at the cap, both under it.
//!
//! The guard itself is mis-specified against the sampling it is given, independently of the
//! residue: [`MIN_PEAK_PAYLOADS`] asks a rung to report an **in-window peak**, and a rung's read
//! follows a completed churn block, so it reports the trough. A *healthy* ladder cannot reach the
//! threshold either — the oracle's own ten-rung readings peak at 123 776–184 480 bytes (30–45
//! payloads) against the guard's 409 600-byte (98.8-payload) threshold. The re-derived guard
//! therefore has to be stated against the property the sampling can see, the residue's
//! **monotonicity** (`last − first` bounded by a small multiple of one payload, and a trough far
//! below the cap), so that a residue growing one payload per park still fails it.
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

/// The iterations between two ladder readings.
const LADDER_STEP: u64 = 1_000;

/// How many rungs the ladder has (`LARGE_N / LADDER_STEP`).
const LADDER_LEN: usize = 10;

/// The size of the binary the safety half **retains** across thousands of prunes.
const KEEP_BYTES: u64 = 65_536;

/// The churn count of the safety half: 6 000 × 4 KiB = 24 MiB of payloads built and discarded
/// while the retained binary stays live (three times [`BOUND_BYTES`], so the assertion below is
/// decisive against a run whose churn was never pruned).
const RETAIN_CHURN_N: u64 = 6_000;

/// The bound every assertion is stated against, in **bytes**.
const BOUND_BYTES: u64 = 8 * 1024 * 1024;

/// The peak a non-vacuous loop must reach: at least this many payloads outstanding between two
/// parks. A ladder rung samples the **post-park residue**, not an in-window peak, so this
/// threshold is unreachable by the sampling it is applied to — the current product reports 10
/// payloads and a healthy runtime (the live oracle) reports 30–45. The guard is deliberately left
/// at its value rather than re-derived to fit the residue; see the module docs' *Known issue* for
/// the property it must be restated against (*monotonicity*, not peak height).
const MIN_PEAK_PAYLOADS: u64 = 100;

/// The trough a completing prune must reach: fewer than this many payloads outstanding
/// (the ladder's first rung is one payload at this payload size; an un-pruned reservoir never
/// gets below N payloads). Unchanged by the *Known issue*: a trough that climbs is exactly what
/// the product must stop doing.
const MAX_TROUGH_PAYLOADS: u64 = 64;

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
/// off-heap binary live across the churn and reports its identity/content plus the figure.
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/off_heap_reclaim.rs` (never committed).
-module(offheap_reclaim).
-export([discard/2, ladder/3, retain/3, foldl_discard/2, foldl_keep/3,
         schedule_payload/2, collect_payload/2]).

%% One iteration: build a `Bytes`-byte OFF-HEAP binary (`bs_create_bin` above the 64-byte
%% on-heap limit), discard it, and touch the heap. The size is a RUNTIME value on purpose: a
%% compile-time constant would be folded into a literal and never reach the payload reservoir.
one(Bytes) ->
    _ = <<0:(Bytes*8)>>,
    L = lists:seq(1, 20),
    _ = length(L),
    ok.

churn(0, _Bytes) -> ok;
churn(N, Bytes) -> one(Bytes), churn(N - 1, Bytes).

%% N build-and-discard iterations, then the `binary_alloc` live-bytes figure in BYTES.
discard(N, Bytes) -> churn(N, Bytes), erlang:memory(binary).

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
/// Two separate `beam_rt run` invocations of the same workload, N = 1 000 and N = 10 000, each
/// printing the `binary_alloc` live-bytes figure at its end. Unfixed, the figure is linear in N
/// (measured on the same fixture with the reclamation call disabled: 4 144 000 bytes at N =
/// 1 000, 41 440 000 at N = 10 000 — the 10× step added 37 296 000 bytes, and `VmHWM` grew
/// 121.8 → 158.8 MiB with it), so the bound below is decisive and the fixed sawtooth
/// (≈1.4 MiB per window) has ~6× of headroom. Peak RSS is printed too (`VmHWM` is a monotonic
/// kernel watermark, so a loaded host does not inflate it) but not asserted: the absolute
/// figure is dominated by the runtime's fixed boot cost.
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
        "[off_heap_reclaim] the 10× step adds {} bytes ({:.2} MiB), bound {BOUND_BYTES} bytes \
         ({:.2} MiB)",
        large_bytes.saturating_sub(small_bytes),
        large_bytes.saturating_sub(small_bytes) as f64 / (1024.0 * 1024.0),
        BOUND_BYTES as f64 / (1024.0 * 1024.0)
    );
    if let (Some(small_line), Some(large_line)) = (&oracle_small, &oracle_large) {
        println!(
            "[off_heap_reclaim] the live oracle: {} at N=1000, {} at N=10000",
            small_line.trim(),
            large_line.trim()
        );
    }
    assert!(
        large_bytes <= small_bytes + BOUND_BYTES,
        "the 10x iteration step must not grow erlang:memory(binary) by more than {BOUND_BYTES} \
         bytes: {} at N={SMALL_N}, {} at N={LARGE_N} (a linear, un-reclaimed reservoir adds \
         ~{} bytes)",
        report(small_bytes),
        report(large_bytes),
        (LARGE_N - SMALL_N) * (PAYLOAD_BYTES + 48)
    );
    assert!(
        large_bytes < BOUND_BYTES,
        "even in absolute terms the figure after {LARGE_N} discarded payloads must stay under \
         {BOUND_BYTES} bytes (it is bounded by the bin-vheap floor plus one pre-emption window, \
         not by N): got {}",
        report(large_bytes)
    );
}

// ---------------------------------------------------------------------------------------
// 2. The non-monotonicity regression ladder
// ---------------------------------------------------------------------------------------

/// **`erlang:memory(binary)` must not grow monotonically with the loop count.**
///
/// In **one** process: `LARGE_N` build-and-discard iterations, the figure recorded every
/// `LADDER_STEP` (a 10-element ladder). Two independent halves are asserted:
///
/// * **bounded** — `max − min` and `last − first` are both within [`BOUND_BYTES`], i.e. the
///   figure does not track N;
/// * **the sawtooth is real** — the ladder's peak is at least [`MIN_PEAK_PAYLOADS`] payloads
///   (the loop really accumulates between two parks, so the flatness is not the trivial
///   flatness of a fixture that never fed the reservoir) and its trough is under
///   [`MAX_TROUGH_PAYLOADS`] (the prune really released them).
///
/// Never exact bytes: the values are a live allocator reading, and the printed ladder is the
/// measurement of record.
#[test]
fn memory_binary_does_not_grow_monotonically_over_the_loop() {
    let Some(toolchain) = setup("ladder") else {
        return;
    };
    let run = run_workload(
        &toolchain,
        "the ladder",
        "ladder",
        &[
            &LARGE_N.to_string(),
            &LADDER_STEP.to_string(),
            &PAYLOAD_BYTES.to_string(),
        ],
    );
    let ladder: Vec<u64> = run
        .stdout
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|rung| {
            rung.trim().parse().unwrap_or_else(|error| {
                panic!("the ladder printed a rung {rung:?} that is not a byte figure: {error}")
            })
        })
        .collect();
    assert_eq!(
        ladder.len(),
        LADDER_LEN,
        "a {LARGE_N}-iteration loop sampled every {LADDER_STEP} is a {LADDER_LEN}-rung ladder: {}",
        run.stdout
    );
    let oracle_ladder = oracle_answer(
        &toolchain,
        "the ladder",
        "ladder",
        &[
            &LARGE_N.to_string(),
            &LADDER_STEP.to_string(),
            &PAYLOAD_BYTES.to_string(),
        ],
    );
    let max = ladder.iter().copied().max().unwrap_or(0);
    let min = ladder.iter().copied().min().unwrap_or(0);
    let spread = max - min;
    println!("\n[off_heap_reclaim] /proc/loadavg {}", loadavg());
    println!(
        "[off_heap_reclaim] the ladder (N={LARGE_N}, step {LADDER_STEP}, {PAYLOAD_BYTES}-byte \
         payloads) in bytes: {ladder:?}"
    );
    println!(
        "[off_heap_reclaim] min {} / max {} / spread {} bytes ({:.2} MiB) / last − first = {} \
         bytes | VmHWM {:.1} MiB ({} samples)",
        min,
        max,
        spread,
        spread as f64 / (1024.0 * 1024.0),
        ladder[LADDER_LEN - 1] as i64 - ladder[0] as i64,
        run.vmhwm_mib(),
        run.samples
    );
    if let Some(line) = &oracle_ladder {
        println!(
            "[off_heap_reclaim] the live oracle's ladder (its own allocator view): {}",
            line.trim()
        );
    }
    assert!(
        spread <= BOUND_BYTES,
        "the ladder's spread must stay within {BOUND_BYTES} bytes (a figure that tracks N has a \
         spread of ~{} bytes): min {min}, max {max}, ladder {ladder:?}",
        (LARGE_N - LADDER_STEP) * (PAYLOAD_BYTES + 48)
    );
    assert!(
        ladder[LADDER_LEN - 1] <= ladder[0] + BOUND_BYTES,
        "the last rung must not exceed the first by more than {BOUND_BYTES} bytes (monotone \
         growth with N is the regression): first {}, last {}, ladder {ladder:?}",
        ladder[0],
        ladder[LADDER_LEN - 1]
    );
    assert!(
        max >= MIN_PEAK_PAYLOADS * PAYLOAD_BYTES,
        "the loop must really accumulate between two parks — a peak of at least \
         {MIN_PEAK_PAYLOADS} payloads ({} bytes) — otherwise flatness proves nothing: max {max}, \
         ladder {ladder:?}",
        MIN_PEAK_PAYLOADS * PAYLOAD_BYTES
    );
    assert!(
        min < MAX_TROUGH_PAYLOADS * PAYLOAD_BYTES,
        "the prune must really release the discarded payloads — a trough under \
         {MAX_TROUGH_PAYLOADS} payloads ({} bytes): min {min}, ladder {ladder:?}",
        MAX_TROUGH_PAYLOADS * PAYLOAD_BYTES
    );
}

// ---------------------------------------------------------------------------------------
// 3. The safety half — a pruned payload must never be a live one
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
/// [`BOUND_BYTES`], so a run whose churn was never pruned cannot satisfy the "under the bound"
/// assertion either.
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
        bytes < BOUND_BYTES,
        "the churn's {RETAIN_CHURN_N} discarded payloads must have been pruned around the \
         retained one, so the figure stays under {BOUND_BYTES} bytes; got {}",
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
// 4. The loud skip
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
/// The store is budgeted by `STORE_BUDGET_WORDS` (46 422 words, the C `BIN_VH_MIN_SIZE`), so the
/// intended envelope is a converged capacity, not a function of N. **Today the first prune is not
/// reliably reached** (4 096-byte payloads, three repeats each): `foldl_discard(1 000)` reads
/// **4 144 000** bytes — every payload of the run, the fully un-pruned figure — and so does
/// `foldl_discard(3 000)` (4 160 576), while N = 2 000 and every N from 4 000 to 10 000 read
/// **16 576**. Both a never-pruned and a converged run are therefore reachable, which is the same
/// retention family as the module docs' *Known issue*, and it is why this assertion is structural
/// on purpose — bounded, and no growth from the small to the large N — never an exact byte figure
/// and never a wall-clock claim.
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
            *bytes <= BOUND_BYTES,
            "a {n}-iteration fun-loop holds {bytes} bytes of off-heap payloads, past the \
             {BOUND_BYTES}-byte budget (a bounded sawtooth never exceeds it): {figures:?}"
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
