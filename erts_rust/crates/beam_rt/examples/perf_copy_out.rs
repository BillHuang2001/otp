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

//! `perf_copy_out` - an **in-process** micro-benchmark of the registered-BIF **result
//! copy-out**: the part of a `call_bif` that moves a container result off the pooled BIF heap
//! onto the running process's own young heap (`calls.rs::move_result_into_process_heap`) before
//! the pool is rewound.
//!
//! # Why this exists
//!
//! The `etf_roundtrip` decomposition attributes a large share of
//! `erlang:binary_to_term/1`'s in-crate cost to "glue" that is not the codec. The hypothesis this
//! harness tests is: **that glue is the copy-out** - the decoded graph is built on the pooled BIF
//! heap and then deep-copied onto the process young heap, i.e. two full traversals of the term.
//! The payload is the *published* `etf_roundtrip` one
//! (`tools/bench/src/workloads/copy_bound.rs::payload/0`): a 5-key map holding a 10-element list,
//! a `{ok, <<"hello world">>, 3.5}` tuple, a 12-byte binary, a char-list string and `-42` - not a
//! made-up 3-word tuple.
//!
//! # Recipe
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt \
//!     --example perf_copy_out [SAMPLES] [ITERATIONS] [--oracle]
//! ```
//!
//! * `SAMPLES` (default [`DEFAULT_SAMPLES`]) - interleaved timed samples per row;
//! * `ITERATIONS` (default [`DEFAULT_ITERATIONS`]) - calls per sample;
//! * `--oracle` - additionally run the *system* `erl` on the same host (needs `erl` on `$PATH`).
//!
//! Run it under `taskset -c <core>` and record `/proc/loadavg`: this host is shared, so an
//! absolute wall time means nothing and only a **like-for-like, interleaved, min-of-SAMPLES**
//! pair means anything. The harness prints its own CPU affinity and `/proc/loadavg` before and
//! after the timed blocks, and reports the **minimum** *and* the **median** of every row.
//!
//! # The rows, and which of them actually copies out
//!
//! | row | what it measures |
//! |---|---|
//! | `control` | the empty loop, for the marginal of every other row |
//! | `bin_to_term A` | `erlang:binary_to_term/1` through `RuntimeContext::new` (**copy-out OFF**) |
//! | `bin_to_term B` | `erlang:binary_to_term/1` through `RuntimeContext::for_process` (**copy-out ON**) |
//! | `term_to_bin A` / `B` | the encode direction, the same two shapes |
//! | `codec decode` | `beam_codec::decode_term` alone, onto a rewound heap |
//! | `codec encode` | `beam_codec::encode_term` alone |
//! | `copy_struct` | `beam_message::copy_struct` alone (off-heap tables + context reused) |
//! | `copy_simple` | `beam_message::copy_struct_simple` (fresh tables per call) |
//! | `copy_out wrap` | the full `copy_result_fresh` shape: fresh tables, the copy, the four adoption loops |
//! | `structural eq` | `beam_term::eq` of the decoded map against the payload (the case's match) |
//!
//! **The A/B pair is a construction-level switch, not a flag.** `RuntimeContext::new` leaves
//! `mailbox_table` as `None`, so `move_result_into_process_heap` returns at its first `?`
//! (`calls.rs:900`) - no copy, **and** no pool rewind and no adoption loops. The `B - A` delta is
//! therefore "copy-out + rewind + adoption"; the three `copy*` rows decompose it further, because
//! they are the same copier called directly.
//!
//! # The guard: is the copy-out actually happening?
//!
//! The copy-out is only reachable from a context that has a process table, and its effect is
//! *deterministic and observable*: after the copy, the process's young heap holds exactly
//! `size_of_copy(decoded)` more used words. [`Harness::verify_copy_out`] resets the young heap,
//! makes one call, and asserts the used-word delta **equals** the copier's own flat word count
//! (and that two calls double it). A measurement taken on a path that does not copy out cannot
//! pass that check, which is what makes the `A`/`B` attribution trustworthy. The same section
//! reports how many `grow_nursery` retries a raw run pays (one retired nursery space per retry),
//! i.e. the "copy attempts per call" figure.
//!
//! # What is NOT measured
//!
//! The engine's per-`call_ext` cost (`beam_engine/tests/bif_cost.rs`), the baseline JIT, process
//! start-up, and the `beam_bifs` BIF body in isolation: a `bin_to_term` row is dispatch **plus**
//! the `beam_bifs` body **plus** the copy-out, and `codec decode` is what separates the BIF body's
//! codec half out. There is also no GC in these loops: the harness rewinds the young heap between
//! samples instead of driving the engine's heap-admission point, so the figures are the
//! *steady-state* cost of the path, not a collector-inclusive one. The payload rebuild the
//! published case pays per iteration (25-60 ns) is not measured on the Rust side.
//!
//! A missing `erl` (only `--oracle` needs it) is a **loud skip** of that block, never a panic and
//! never a silent pass.

#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::Instant;

use beam_alloc::Heap;
use beam_engine::ExecutionContext;
use beam_message::{copy_struct, copy_struct_simple, size_of_copy, CopyContext, OffHeapTable};
use beam_process::{ProcessId, ProcessTable, SignalAtoms};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{size_object, AtomTable, Eterm};

/// The published `etf_roundtrip` payload (`tools/bench/src/workloads/copy_bound.rs`): a 5-key
/// map holding a 10-element list, a `{ok, Binary, Float}` tuple, a 12-byte binary, a char-list
/// string and a negative small integer. Every row of this harness uses it, on both runtimes, so
/// the figures are comparable with the published `etf_roundtrip` per-op split.
const PAYLOAD_SRC: &str = "\
#{a => [1,2,3,4,5,6,7,8,9,10],
  b => {ok, <<\"hello world\">>, 3.5},
  c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
  d => \"a char list\",
  e => -42}";

/// Interleaved timed samples taken per row; the **minimum** and the **median** are reported.
const DEFAULT_SAMPLES: usize = 15;

/// Calls per sample. A `bin_to_term B` call is ~1.5 us, so 5 000 is ~7 ms of work - far above the
/// clock's resolution - while keeping the young heap the copies land in small (it is rewound
/// before every sample).
const DEFAULT_ITERATIONS: u64 = 5_000;

/// The loop count the **oracle** block uses per row and per sample. `timer:tc` reports integer
/// microseconds, so 50 000 codec calls (~32 ms) make the microsecond resolution irrelevant.
const SYS_ITERATIONS: u64 = 50_000;

/// Capacity of the harness's own decode/encode/copy scratch heaps, in words. The payload's flat
/// size is a few dozen words, so this is comfortable headroom - and it is also the size the
/// process's young heap is pre-grown to, so a sample never pays a mid-sample `grow_nursery`.
const SCRATCH_HEAP_WORDS: usize = 4096;

/// How many copy-outs the "attempts per call" probe runs before reading the nursery's space
/// count. Each `grow_nursery` retires exactly one space, so `spaces - 1` is the retry count.
const RETRY_PROBE_COPIES: u64 = 1_000;

/// How many copies one batch of the `copy-out ON` rows runs before the young heap is rewound.
///
/// The copy-out lands on the process's young heap, which no collector empties in this harness.
/// At [`SCRATCH_HEAP_WORDS`] (4096 words) and ~70 words per copy, **48** copies (3360 words) fit
/// without ever triggering a `grow_nursery` - so every sample pays the copier and nothing else,
/// and the destination stays a warm post-collection-sized region (which is what a real process's
/// heap is between two minor collections). The rewind itself is one pointer store per batch,
/// i.e. ~0.02 ns/op, and the per-batch context construction is amortised the same way.
const BATCH_CALLS: u64 = 48;

/// A two-byte ETF image of the small integer `1` (`131, 97, 1`: version magic + `SMALL_INTEGER_EXT`).
///
/// Decoding it produces an **immediate**, so a `binary_to_term/1` row on this image pays the
/// dispatch prologue and the BIF's fixed body but allocates nothing on the pooled heap and (on
/// the process shape) copies nothing out. The difference against the payload row is therefore the
/// codec's own graph work, and the row itself *is* the prologue.
const TINY_IMAGE: [u8; 3] = [131, 97, 1];
/// One measurement row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    /// The empty loop (the load-invariant baseline every marginal is taken over).
    Control,
    /// `erlang:binary_to_term/1` on a table-less context: copy-out OFF.
    BinaryToTermNoCopy,
    /// `erlang:binary_to_term/1` on a process context: copy-out ON.
    BinaryToTermCopyOut,
    /// `erlang:binary_to_term/1` of the **2-byte immediate image**, table-less: the prologue plus
    /// the BIF's fixed body, with no codec graph and no allocation on the pooled heap.
    TinyNoCopy,
    /// The same immediate image on a process context: the prologue with **no** copy-out (an
    /// immediate is not a foreign container), i.e. the control for the A/B shape difference.
    TinyProcess,
    /// `erlang:is_binary/1`, table-less: the prologue with the cheapest possible BIF body.
    LeafNoCopy,
    /// `erlang:is_binary/1` on a process context: the same, plus the copy-out gate's own cost.
    LeafProcess,
    /// `erlang:term_to_binary/1` on a table-less context.
    TermToBinaryNoCopy,
    /// `erlang:term_to_binary/1` on a process context.
    TermToBinaryCopyOut,
    /// `beam_codec::decode_term` alone.
    CodecDecode,
    /// `beam_codec::encode_term` alone.
    CodecEncode,
    /// `beam_message::copy_struct` with the off-heap tables and context reused.
    CopyStruct,
    /// `beam_message::copy_struct_simple` (two fresh `OffHeapTable`s per call).
    CopySimple,
    /// The full `copy_result_fresh` shape, including the four adoption loops.
    CopyOutWrapper,
    /// `beam_term::eq` of the decoded map against the payload.
    StructuralEq,
}

impl Row {
    /// Every row, in report order.
    const ALL: [Row; 15] = [
        Row::Control,
        Row::BinaryToTermNoCopy,
        Row::BinaryToTermCopyOut,
        Row::TinyNoCopy,
        Row::TinyProcess,
        Row::LeafNoCopy,
        Row::LeafProcess,
        Row::TermToBinaryNoCopy,
        Row::TermToBinaryCopyOut,
        Row::CodecDecode,
        Row::CodecEncode,
        Row::CopyStruct,
        Row::CopySimple,
        Row::CopyOutWrapper,
        Row::StructuralEq,
    ];

    /// This row's index in [`Row::ALL`].
    fn index(self) -> usize {
        // A direct map, so the hot path never searches: the array is a fixed literal and
        // `Row::ALL[i]` is documented to be the row at index `i`.
        match self {
            Row::Control => 0,
            Row::BinaryToTermNoCopy => 1,
            Row::BinaryToTermCopyOut => 2,
            Row::TinyNoCopy => 3,
            Row::TinyProcess => 4,
            Row::LeafNoCopy => 5,
            Row::LeafProcess => 6,
            Row::TermToBinaryNoCopy => 7,
            Row::TermToBinaryCopyOut => 8,
            Row::CodecDecode => 9,
            Row::CodecEncode => 10,
            Row::CopyStruct => 11,
            Row::CopySimple => 12,
            Row::CopyOutWrapper => 13,
            Row::StructuralEq => 14,
        }
    }

    /// The label the tables print.
    fn label(self) -> &'static str {
        match self {
            Row::Control => "control (empty loop)",
            Row::BinaryToTermNoCopy => "erlang:binary_to_term/1   copy-out OFF",
            Row::BinaryToTermCopyOut => "erlang:binary_to_term/1   copy-out ON",
            Row::TinyNoCopy => "  tiny image, copy-out OFF",
            Row::TinyProcess => "  tiny image, process shape",
            Row::LeafNoCopy => "  is_binary/1, copy-out OFF",
            Row::LeafProcess => "  is_binary/1, process shape",
            Row::TermToBinaryNoCopy => "erlang:term_to_binary/1   copy-out OFF",
            Row::TermToBinaryCopyOut => "erlang:term_to_binary/1   copy-out ON",
            Row::CodecDecode => "beam_codec::decode_term (codec only)",
            Row::CodecEncode => "beam_codec::encode_term (codec only)",
            Row::CopyStruct => "copy_struct (tables reused)",
            Row::CopySimple => "copy_struct_simple",
            Row::CopyOutWrapper => "copy-out wrapper (fresh tables + drains)",
            Row::StructuralEq => "beam_term::eq (the case's match)",
        }
    }

    /// Which side of the A/B the row belongs to.
    fn side(self) -> &'static str {
        match self {
            Row::Control => "baseline",
            Row::BinaryToTermNoCopy | Row::TermToBinaryNoCopy => "A no copy-out",
            Row::BinaryToTermCopyOut | Row::TermToBinaryCopyOut => "B copy-out",
            Row::TinyNoCopy | Row::LeafNoCopy => "A no copy-out",
            Row::TinyProcess | Row::LeafProcess => "B copy-out",
            _ => "direct",
        }
    }
}
/// How the harness ended. A missing `erl` (only `--oracle` needs it) is handled as a block-local
/// loud skip *inside* the run, so a `Skip` outcome here is reserved for a future whole-run
/// prerequisite; everything else is a failure the caller reports.
enum Failure {
    /// The invocation itself was malformed - exit 2 with the usage text.
    Usage(String),
    /// `-h`/`--help` - print the usage text and exit 0.
    Help,
    /// The measurement could not be taken or a row misbehaved - exit 1.
    Error(String),
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(Failure::Help) => print_usage(),
        Err(Failure::Usage(why)) => {
            eprintln!("perf_copy_out: {why}");
            print_usage();
            process::exit(2);
        }
        Err(Failure::Error(why)) => {
            eprintln!("perf_copy_out: {why}");
            process::exit(1);
        }
    }
}

fn print_usage() {
    println!(
        "usage: perf_copy_out [SAMPLES] [ITERATIONS] [--oracle]\n\
         \n\
         SAMPLES     interleaved timed samples per row (default {DEFAULT_SAMPLES})\n\
         ITERATIONS  calls per sample (default {DEFAULT_ITERATIONS})\n\
         --oracle    also run the system `erl` on the same host (needs `erl` on $PATH)"
    );
}

/// The parsed invocation.
struct Config {
    /// Interleaved timed samples per row.
    samples: usize,
    /// Calls per sample.
    iterations: u64,
    /// Whether the system-`erl` comparison block runs.
    oracle: bool,
}

fn parse_args() -> Result<Config, Failure> {
    let mut samples = DEFAULT_SAMPLES;
    let mut iterations = DEFAULT_ITERATIONS;
    let mut oracle = false;
    let mut numbers = Vec::new();
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => return Err(Failure::Help),
            "--oracle" => oracle = true,
            other if other.starts_with('-') => {
                return Err(Failure::Usage(format!("unknown switch {other:?}")));
            }
            other => numbers.push(other.to_string()),
        }
    }
    if numbers.len() > 2 {
        return Err(Failure::Usage(
            "too many arguments (only the sample count and the iteration count are accepted)"
                .to_string(),
        ));
    }
    if let Some(raw) = numbers.first() {
        samples = raw
            .parse::<usize>()
            .map_err(|_| Failure::Usage(format!("{raw:?} is not a sample count")))?;
        if samples == 0 {
            return Err(Failure::Usage(
                "the sample count must be at least 1".to_string(),
            ));
        }
    }
    if let Some(raw) = numbers.get(1) {
        iterations = raw
            .parse::<u64>()
            .map_err(|_| Failure::Usage(format!("{raw:?} is not an iteration count")))?;
        if iterations == 0 {
            return Err(Failure::Usage(
                "the iteration count must be at least 1".to_string(),
            ));
        }
    }
    Ok(Config {
        samples,
        iterations,
        oracle,
    })
}

/// `/proc/loadavg`, or `"<unreadable>"` where the file does not exist. This host is shared, so
/// the load every block ran under is part of the record.
fn loadavg() -> String {
    match fs::read_to_string("/proc/loadavg") {
        Ok(text) => text.trim().to_string(),
        Err(_) => "<unreadable>".to_string(),
    }
}

/// This process's minor page-fault count (`/proc/self/stat` field 10), or `0` where the file does
/// not exist.
///
/// This is the harness's **deterministic** memory-traffic counter: a row that writes never-touched
/// anonymous memory faults roughly once per 512 words it touches, and a fault costs far more than
/// the store it serves. A row whose `faults/call` is non-zero is measuring first-touch cost as much
/// as it measures code.
fn minor_faults() -> u64 {
    let Ok(text) = fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    // The comm field is parenthesised and may itself contain spaces or parentheses, so the parse
    // starts after the LAST ')': the fields after it are state(3), ppid(4), pgrp(5), session(6),
    // tty_nr(7), tpgid(8), flags(9), minflt(10) - i.e. minflt is index 7 of that tail.
    let Some(tail) = text.rsplit_once(')') else {
        return 0;
    };
    tail.1
        .split_whitespace()
        .nth(7)
        .and_then(|field| field.parse().ok())
        .unwrap_or(0)
}

/// The clock of the core this process is pinned to, in MHz, read from `/proc/cpuinfo`.
///
/// This host frequency-scales by 3x between runs (1.2 GHz and 3.6 GHz were both observed while
/// this harness was written), so an absolute ns/op figure is only interpretable next to the clock
/// it was taken at. When the affinity is a single core its own `cpu MHz` is reported; otherwise
/// the range plus the mean over the allowed cores.
fn clock_mhz() -> String {
    let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") else {
        return "<unreadable>".to_string();
    };
    let mut clocks: Vec<(usize, f64)> = Vec::new();
    for block in cpuinfo.split("\n\n") {
        let mut processor = None;
        let mut mhz = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("processor") {
                processor = value.trim_start_matches([':', ' ', '\t']).parse::<usize>().ok();
            } else if let Some(value) = line.strip_prefix("cpu MHz") {
                mhz = value.trim_start_matches([':', ' ', '\t']).parse::<f64>().ok();
            }
        }
        if let (Some(processor), Some(mhz)) = (processor, mhz) {
            clocks.push((processor, mhz));
        }
    }
    if clocks.is_empty() {
        return "<unknown>".to_string();
    }
    let allowed = allowed_cpus();
    let selected: Vec<f64> = match allowed.as_slice() {
        [only] => clocks
            .iter()
            .filter(|(processor, _)| processor == only)
            .map(|(_, mhz)| *mhz)
            .collect(),
        _ => clocks
            .iter()
            .filter(|(processor, _)| allowed.is_empty() || allowed.contains(processor))
            .map(|(_, mhz)| *mhz)
            .collect(),
    };
    if selected.is_empty() {
        return "<unknown>".to_string();
    }
    let min = selected.iter().copied().fold(f64::INFINITY, f64::min);
    let max = selected.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mean = selected.iter().sum::<f64>() / selected.len() as f64;
    match allowed.as_slice() {
        [only] => format!("{min:.0} MHz on cpu{only}"),
        _ => format!("{min:.0}-{max:.0} MHz (mean {mean:.0}) over {} cores", selected.len()),
    }
}

/// The CPU ids in this process's `Cpus_allowed_list` (`7` or `0-3,8`); empty when unknown.
fn allowed_cpus() -> Vec<usize> {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return Vec::new();
    };
    let Some(line) = status
        .lines()
        .find(|line| line.starts_with("Cpus_allowed_list:"))
    else {
        return Vec::new();
    };
    let list = line.trim_start_matches("Cpus_allowed_list:").trim();
    let mut cpus = Vec::new();
    for part in list.split(',') {
        match part.split_once('-') {
            Some((first, last)) => {
                if let (Ok(first), Ok(last)) = (first.parse::<usize>(), last.parse::<usize>()) {
                    cpus.extend(first..=last);
                }
            }
            None => {
                if let Ok(cpu) = part.trim().parse::<usize>() {
                    cpus.push(cpu);
                }
            }
        }
    }
    cpus
}

/// This process's own CPU affinity, so the report names the core both sides ran on.
fn affinity() -> String {
    match fs::read_to_string("/proc/self/status") {
        Ok(text) => text
            .lines()
            .find(|line| line.starts_with("Cpus_allowed_list:"))
            .map(|line| line.trim().to_string())
            .unwrap_or_else(|| "<unknown>".to_string()),
        Err(_) => "<unreadable>".to_string(),
    }
}

// --- the measurement core ---------------------------------------------------------

/// Times `n` iterations of `f` and returns nanoseconds per iteration. `f` returns a word folded
/// into a checksum so the loop cannot be optimised away.
fn time_ns<F: FnMut() -> usize>(n: u64, mut f: F) -> (f64, f64) {
    let faults_before = minor_faults();
    let mut acc = 0usize;
    let started = Instant::now();
    for _ in 0..n {
        acc = acc.wrapping_add(f());
    }
    let elapsed = started.elapsed();
    let faults = minor_faults().saturating_sub(faults_before);
    std::hint::black_box(acc);
    (
        elapsed.as_nanos() as f64 / n as f64,
        faults as f64 / n as f64,
    )
}

/// One timed row, split into halves so a **within-sample drift** is visible.
///
/// The drift matters for exactly one reason: the copy-out-OFF shape never rewinds the pooled BIF
/// heap, so each call writes fresh (never touched) anonymous memory and pays a page fault every
/// ~7 calls. A row whose second half is markedly slower than its first is telling us its cost is
/// dominated by memory first-touch rather than by the code under test.
struct Timing {
    /// The whole run: nanoseconds per iteration.
    ns: f64,
    /// The first half's nanoseconds per iteration.
    first: f64,
    /// The second half's nanoseconds per iteration.
    second: f64,
    /// Minor page faults the row took, per iteration (`/proc/self/stat` field 10). This is the
    /// **deterministic** counter that says whether a row's cost is memory first-touch: the
    /// copy-out-OFF shape never rewinds the pooled BIF heap, so every call writes never-touched
    /// anonymous memory and faults every few calls.
    faults_per_call: f64,
}

impl Timing {
    /// A row whose two halves were not timed separately.
    fn flat(ns: f64, faults_per_call: f64) -> Timing {
        Timing {
            ns,
            first: ns,
            second: ns,
            faults_per_call,
        }
    }
}

/// [`time_ns`] over two halves of the loop, so drift is observable.
fn time_halves<F: FnMut() -> usize>(n: u64, mut f: F) -> Timing {
    let first_n = n / 2;
    let second_n = n - first_n;
    if first_n == 0 {
        let (ns, faults) = time_ns(n, f);
        return Timing::flat(ns, faults);
    }
    let (first, first_faults) = time_ns(first_n, &mut f);
    let (second, second_faults) = time_ns(second_n, &mut f);
    Timing {
        ns: (first * first_n as f64 + second * second_n as f64) / n as f64,
        first,
        second,
        faults_per_call: (first_faults * first_n as f64 + second_faults * second_n as f64)
            / n as f64,
    }
}
/// The lowest value of a sample set, or `f64::INFINITY` for an empty one.
fn min_of(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::INFINITY, f64::min)
}

/// The median of a sample set (the mean of the two middles for an even count).
fn median_of(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

/// Renders `bytes` as an Erlang binary literal (`<<1,2,3>>`), so the image can be materialised
/// through the `-eval` parser instead of an `unsafe` term write.
fn binary_literal(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 + 4);
    out.push_str("<<");
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&byte.to_string());
    }
    out.push_str(">>");
    out
}

/// The process's own young heap in `table`.
fn process_heap(table: &mut ProcessTable, pid: ProcessId) -> Result<&mut Heap, Failure> {
    let process = table
        .get_mut(pid)
        .ok_or_else(|| Failure::Error("the harness's process vanished from its table".to_string()))?;
    Ok(process.heap_mut())
}

/// Rewinds the process's young heap to its base - the state a collection leaves - so a sample
/// starts from a known occupancy.
fn reset_young_heap(table: &mut ProcessTable, pid: ProcessId) -> Result<(), Failure> {
    process_heap(table, pid)?.reset();
    Ok(())
}

/// The number of nursery spaces the process's collector holds (`1` = no growth-retired space).
fn young_space_count(table: &mut ProcessTable, pid: ProcessId) -> Result<usize, Failure> {
    let process = table
        .get_mut(pid)
        .ok_or_else(|| Failure::Error("the harness's process vanished from its table".to_string()))?;
    Ok(process.collector().young_space_count())
}

/// One `call_bif`, mapped to the harness's error type.
fn call(
    ctx: &mut RuntimeContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, Failure> {
    ctx.call_bif(module, function, arity, args)
        .map_err(|error| Failure::Error(format!("{module}:{function}/{arity} raised {error}")))
}

/// Warms `ctx` with one call, then times `n` calls and returns the timing. An error inside the
/// timed loop is reported (never folded into the figure as a zero).
fn timed_call(
    ctx: &mut RuntimeContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
    n: u64,
) -> Result<Timing, Failure> {
    // The warm call is what resolves the dispatch memo for this key on this context (a
    // `DispatchMemo` miss additionally pays two `String` allocations and ten roster searches).
    let _ = call(ctx, module, function, arity, args)?;
    let mut failure: Option<String> = None;
    let timing = time_halves(n, || match ctx.call_bif(module, function, arity, args) {
        Ok(term) => term.raw(),
        Err(error) => {
            if failure.is_none() {
                failure = Some(format!("{error}"));
            }
            0
        }
    });
    match failure {
        Some(why) => Err(Failure::Error(format!(
            "{module}:{function}/{arity} raised {why} in the timed loop"
        ))),
        None => Ok(timing),
    }
}
/// [`timed_call`] in [`BATCH_CALLS`]-sized batches, rewinding the process's young heap between
/// them - the copy-out row's timing.
///
/// The rewind is an O(1) pointer store (`Heap::reset`), i.e. ~0.02 ns/op, and it is what keeps the
/// sample free of `grow_nursery`: without it a 5 000-call sample would retire a nursery space
/// every ~1 000 calls and grow the destination region to half a million words, measuring region
/// growth rather than the copier. The per-batch `RuntimeContext::for_process` construction is
/// amortised over `BATCH_CALLS` calls.
fn timed_batched(
    rt: &mut Runtime,
    table: &mut ProcessTable,
    pid: ProcessId,
    module: &str,
    function: &str,
    args: &[Eterm],
    n: u64,
) -> Result<Timing, Failure> {
    let mut points: Vec<(u64, f64, f64)> = Vec::new();
    let mut done = 0u64;
    while done < n {
        let batch = BATCH_CALLS.min(n - done);
        reset_young_heap(table, pid)?;
        let timing = {
            let mut ctx = RuntimeContext::for_process(rt, table, pid);
            timed_call(&mut ctx, module, function, 1, args, batch)?
        };
        points.push((batch, timing.ns, timing.faults_per_call));
        done += batch;
    }
    let total: u64 = points.iter().map(|(calls, _, _)| *calls).sum();
    let half = total / 2;
    let (mut sum, mut seen) = (0.0f64, 0u64);
    let (mut first_sum, mut first_calls, mut second_sum, mut second_calls) = (0.0f64, 0u64, 0.0f64, 0u64);
    let mut faults = 0.0f64;
    for (calls, ns, row_faults) in &points {
        faults += row_faults * *calls as f64;
        sum += ns * *calls as f64;
        if seen < half {
            first_sum += ns * *calls as f64;
            first_calls += *calls;
        } else {
            second_sum += ns * *calls as f64;
            second_calls += *calls;
        }
        seen += *calls;
    }
    Ok(Timing {
        ns: sum / total as f64,
        first: if first_calls == 0 { 0.0 } else { first_sum / first_calls as f64 },
        second: if second_calls == 0 { 0.0 } else { second_sum / second_calls as f64 },
        faults_per_call: faults / total as f64,
    })
}

/// Everything the rows measure against, held for the whole run.
struct Harness {
    /// The booted runtime: the code server, the atom table, the BIF registry, the BIF heap pool.
    rt: Runtime,
    /// The process table `RuntimeContext::for_process` runs against - the copy-out only exists
    /// when this is `Some`, which is exactly the A/B switch.
    table: ProcessTable,
    /// The pid in `table` the copy-out re-homes results onto.
    pid: ProcessId,
    /// The payload term, materialised on the runtime's long-lived `-eval` heap.
    payload: Eterm,
    /// `term_to_binary(payload)` as an Erlang **binary term** (the argument `binary_to_term/1`
    /// receives), also on the `-eval` heap.
    image_arg: Eterm,
    /// The same image as plain bytes, for the codec-only rows.
    image_bytes: Vec<u8>,
    /// The **2-byte immediate image** ([`TINY_IMAGE`]) as an Erlang binary term: the prologue rows
    /// decode it into an immediate, so they allocate no graph and copy nothing out.
    tiny_arg: Eterm,
    /// A private copy of the runtime's atom table, so the codec rows need no `&mut Runtime`
    /// borrow and every row can be measured in one interleaved loop.
    atoms: AtomTable,
    /// The decoded graph, as a **source** term the direct-copy rows copy from (it stands in for
    /// the pooled BIF heap: a foreign region holding the decoded graph).
    decoded: Eterm,
    /// The heap `decoded` lives on; held only to keep it alive, never read.
    _source_heap: Heap,
    /// The decode destination for the codec-only row (rewound per iteration).
    codec_heap: Heap,
    /// The copy destination for the direct-copy rows (rewound per iteration).
    copy_heap: Heap,
}

impl Harness {
    /// Boots the runtime and materialises the fixtures. The workload is its own self-check: a
    /// fixture that does not round-trip is a loud error, never a number.
    fn new() -> Result<Harness, Failure> {
        let mut rt = Runtime::with_defaults()
            .map_err(|error| Failure::Error(format!("the default runtime: {error}")))?;
        rt.boot()
            .map_err(|error| Failure::Error(format!("the runtime could not boot: {error}")))?;

        // The payload, materialised through the runtime's own `-eval` route so it lives on the
        // long-lived `-eval` heap (never reset) and is built by the same parser the differential
        // harness drives.
        let payload = rt
            .eval(PAYLOAD_SRC)
            .map_err(|error| Failure::Error(format!("cannot materialize the payload: {error}")))?;

        // The ETF image twice over: once as bytes (through the codec, so no `unsafe` term read is
        // needed) and once as an Erlang binary term (through the `-eval` parser, so the BIF rows
        // pass exactly the term shape a compiled module would).
        let image_bytes = {
            let atoms = rt.code_server_atoms_mut();
            beam_codec::encode_term(payload, atoms)
                .map_err(|error| Failure::Error(format!("cannot encode the payload: {error}")))?
        };
        if image_bytes.first() != Some(&131) {
            return Err(Failure::Error(format!(
                "the encoded payload is not an ETF image (first byte {:?})",
                image_bytes.first()
            )));
        }
        let image_arg = rt.eval(&binary_literal(&image_bytes)).map_err(|error| {
            Failure::Error(format!("cannot materialize the ETF image term: {error}"))
        })?;
        // The 2-byte immediate image, materialised the same way: the prologue rows decode it and
        // get an immediate back, so they touch neither the pooled heap nor the young heap.
        let tiny_arg = rt
            .eval(&binary_literal(&TINY_IMAGE))
            .map_err(|error| Failure::Error(format!("cannot materialize the tiny image: {error}")))?;

        // A private copy of the atom table: the codec rows then intern into a table that is
        // structurally identical to the live one (same index for every name) without borrowing
        // the runtime, which is what lets every row share one interleaved loop.
        let mut atoms = rt.code_server_atoms_mut().clone();

        let signals = SignalAtoms::from_table(rt.code_server_atoms_mut());
        let mut table = ProcessTable::new(signals);
        let pid = table
            .spawn_default()
            .map_err(|error| Failure::Error(format!("could not spawn a process: {error}")))?;

        let mut source_heap = Heap::new(SCRATCH_HEAP_WORDS)
            .map_err(|error| Failure::Error(format!("the source heap: {error}")))?;
        let decoded = beam_codec::decode_term(&image_bytes, &mut source_heap, &mut atoms)
            .map_err(|error| Failure::Error(format!("cannot decode the payload: {error}")))?;
        let codec_heap = Heap::new(SCRATCH_HEAP_WORDS)
            .map_err(|error| Failure::Error(format!("the codec heap: {error}")))?;
        let copy_heap = Heap::new(SCRATCH_HEAP_WORDS)
            .map_err(|error| Failure::Error(format!("the copy heap: {error}")))?;

        let mut harness = Harness {
            rt,
            table,
            pid,
            payload,
            image_arg,
            image_bytes,
            tiny_arg,
            atoms,
            decoded,
            _source_heap: source_heap,
            codec_heap,
            copy_heap,
        };
        harness.check_round_trip()?;
        // Pre-grow the process's nursery so a timed sample never pays a mid-sample growth (the
        // retry cost is measured separately, in `verify_copy_out`).
        harness
            .table
            .get_mut(harness.pid)
            .ok_or_else(|| {
                Failure::Error("the harness's process vanished from its table".to_string())
            })?
            .grow_nursery(SCRATCH_HEAP_WORDS)
            .map_err(|error| Failure::Error(format!("cannot size the young heap: {error}")))?;
        Ok(harness)
    }

    /// Asserts `binary_to_term(term_to_binary(Payload)) =:= Payload` through the real BIF path, on
    /// **both** context shapes - so the A side is proven to produce the same answer as the B side
    /// (they differ only in the copy-out).
    fn check_round_trip(&mut self) -> Result<(), Failure> {
        let Harness {
            rt,
            table,
            pid,
            payload,
            image_arg,
            atoms,
            ..
        } = self;
        let pid = *pid;
        {
            let mut ctx = RuntimeContext::new(rt);
            let back = call(&mut ctx, "erlang", "binary_to_term", 1, &[*image_arg])?;
            if !beam_term::eq(back, *payload, atoms) {
                return Err(Failure::Error(
                    "the copy-out-OFF route did not reproduce the payload".to_string(),
                ));
            }
        }
        {
            let mut ctx = RuntimeContext::for_process(rt, table, pid);
            let back = call(&mut ctx, "erlang", "binary_to_term", 1, &[*image_arg])?;
            if !beam_term::eq(back, *payload, atoms) {
                return Err(Failure::Error(
                    "the copy-out-ON route did not reproduce the payload".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Times one row for `n` iterations.
    fn measure_row(&mut self, row: Row, n: u64) -> Result<Timing, Failure> {
        let Harness {
            rt,
            table,
            pid,
            payload,
            image_arg,
            image_bytes,
            tiny_arg,
            atoms,
            decoded,
            _source_heap: _,
            codec_heap,
            copy_heap,
        } = self;
        let pid = *pid;
        let args = [*image_arg];
        let tiny = [*tiny_arg];
        match row {
            Row::Control => Ok(time_halves(n, || 0)),
            Row::BinaryToTermNoCopy => {
                let mut ctx = RuntimeContext::new(rt);
                timed_call(&mut ctx, "erlang", "binary_to_term", 1, &args, n)
            }
            Row::BinaryToTermCopyOut => {
                // Batched: the young heap is rewound between batches so no sample ever pays a
                // `grow_nursery`, and the copy lands in a warm post-collection-sized region.
                timed_batched(rt, table, pid, "erlang", "binary_to_term", &args, n)
            }
            Row::TinyNoCopy => {
                let mut ctx = RuntimeContext::new(rt);
                timed_call(&mut ctx, "erlang", "binary_to_term", 1, &tiny, n)
            }
            Row::TinyProcess => {
                let mut ctx = RuntimeContext::for_process(rt, table, pid);
                timed_call(&mut ctx, "erlang", "binary_to_term", 1, &tiny, n)
            }
            Row::LeafNoCopy => {
                let mut ctx = RuntimeContext::new(rt);
                timed_call(&mut ctx, "erlang", "is_binary", 1, &tiny, n)
            }
            Row::LeafProcess => {
                let mut ctx = RuntimeContext::for_process(rt, table, pid);
                timed_call(&mut ctx, "erlang", "is_binary", 1, &tiny, n)
            }
            Row::TermToBinaryNoCopy => {
                let mut ctx = RuntimeContext::new(rt);
                // `term_to_binary/1` needs a caller module for a local fun's identity, but the
                // payload carries no fun: the encode is identical either way.
                timed_call(&mut ctx, "erlang", "term_to_binary", 1, &[*payload], n)
            }
            Row::TermToBinaryCopyOut => {
                // The encoded image is a 101-byte (off-heap refc) binary, so its copy-out is the
                // 8-word `[BinRef][ErlangSubBits]` pair plus one adopted payload handle - three
                // orders of magnitude smaller than the decode's graph copy, which is why this
                // side is the control that the `B - A` delta is the *graph* copy.
                timed_batched(rt, table, pid, "erlang", "term_to_binary", &[*payload], n)
            }
            Row::CodecDecode => {
                let mut failure: Option<String> = None;
                let (ns, faults) = time_ns(n, || {
                    codec_heap.reset();
                    match beam_codec::decode_term(image_bytes, codec_heap, atoms) {
                        Ok(term) => term.raw(),
                        Err(error) => {
                            if failure.is_none() {
                                failure = Some(format!("{error}"));
                            }
                            0
                        }
                    }
                });
                match failure {
                    Some(why) => Err(Failure::Error(format!("decode_term raised {why}"))),
                    None => Ok(Timing::flat(ns, faults)),
                }
            }
            Row::CodecEncode => {
                let mut failure: Option<String> = None;
                let (ns, faults) = time_ns(n, || match beam_codec::encode_term(*decoded, atoms) {
                    // The `Vec` is what the BIF produces per call; its length keeps it alive.
                    Ok(bytes) => bytes.len(),
                    Err(error) => {
                        if failure.is_none() {
                            failure = Some(format!("{error}"));
                        }
                        0
                    }
                });
                match failure {
                    Some(why) => Err(Failure::Error(format!("encode_term raised {why}"))),
                    None => Ok(Timing::flat(ns, faults)),
                }
            }
            Row::CopyStruct => {
                // The off-heap tables and the copy context are built ONCE, outside the loop, so
                // this row is the copier's own walk and bump allocation.
                let src = OffHeapTable::new();
                let mut local = OffHeapTable::new();
                let mut copy_ctx = CopyContext::new(&src, &mut local);
                let mut failure: Option<String> = None;
                let (ns, faults) = time_ns(n, || {
                    copy_heap.reset();
                    match copy_struct(*decoded, copy_heap, &mut copy_ctx) {
                        Ok(term) => term.raw(),
                        Err(error) => {
                            if failure.is_none() {
                                failure = Some(format!("{error}"));
                            }
                            0
                        }
                    }
                });
                match failure {
                    Some(why) => Err(Failure::Error(format!("copy_struct raised {why}"))),
                    None => Ok(Timing::flat(ns, faults)),
                }
            }
            Row::CopySimple => {
                let mut failure: Option<String> = None;
                let (ns, faults) = time_ns(n, || {
                    copy_heap.reset();
                    match copy_struct_simple(*decoded, copy_heap) {
                        Ok(term) => term.raw(),
                        Err(error) => {
                            if failure.is_none() {
                                failure = Some(format!("{error}"));
                            }
                            0
                        }
                    }
                });
                match failure {
                    Some(why) => Err(Failure::Error(format!("copy_struct_simple raised {why}"))),
                    None => Ok(Timing::flat(ns, faults)),
                }
            }
            Row::CopyOutWrapper => {
                // The `copy_result_fresh` shape: two FRESH off-heap tables per attempt, the copy,
                // then the three adoption loops (a no-op for this payload, which owns no off-heap
                // handle - which is exactly the point of measuring it).
                let mut failure: Option<String> = None;
                let (ns, faults) = time_ns(n, || {
                    copy_heap.reset();
                    let src = OffHeapTable::new();
                    let mut local = OffHeapTable::new();
                    let outcome = {
                        let mut copy_ctx = CopyContext::new(&src, &mut local);
                        copy_struct(*decoded, copy_heap, &mut copy_ctx)
                    };
                    match outcome {
                        Ok(term) => {
                            // The three adoption loops of `copy_result_fresh`. They are empty for
                            // this payload (the pooled heap owns no off-heap handle, so the
                            // copier registers nothing), which is exactly why they are measured
                            // here: their cost is the drain itself, not the entries.
                            let mut adopted = 0usize;
                            for _handle in local.drain_handles() {
                                adopted = adopted.wrapping_add(1);
                            }
                            for _binary in local.drain_binaries() {
                                adopted = adopted.wrapping_add(1);
                            }
                            for entry in local.drain_nodes() {
                                adopted = adopted.wrapping_add(entry.refcount());
                            }
                            term.raw().wrapping_add(adopted)
                        }
                        Err(error) => {
                            if failure.is_none() {
                                failure = Some(format!("{error}"));
                            }
                            0
                        }
                    }
                });
                match failure {
                    Some(why) => Err(Failure::Error(format!("the copy-out wrapper raised {why}"))),
                    None => Ok(Timing::flat(ns, faults)),
                }
            }
            Row::StructuralEq => {
                let (ns, faults) = time_ns(n, || usize::from(beam_term::eq(*decoded, *payload, atoms)));
                Ok(Timing::flat(ns, faults))
            }
        }
    }
    /// The deterministic (wall-clock-independent) facts: the workload's size, the words each
    /// copy-out moves, and the proof that the copy-out really runs on the `B` side.
    fn verify_copy_out(&mut self) -> Result<Deterministic, Failure> {
        let Harness {
            rt,
            table,
            pid,
            payload,
            image_arg,
            image_bytes,
            decoded,
            ..
        } = self;
        let pid = *pid;
        let words_per_copy = size_of_copy(*decoded);
        let words_per_payload = size_object(*payload);
        let args = [*image_arg];

        // (1) One and two copy-outs, with the nursery already large enough, so the used-word
        // delta must equal the copier's own flat word count exactly.
        reset_young_heap(table, pid)?;
        let spaces_before = young_space_count(table, pid)?;
        {
            let mut ctx = RuntimeContext::for_process(rt, table, pid);
            let _ = call(&mut ctx, "erlang", "binary_to_term", 1, &args)?;
        }
        let used_after_one = process_heap(table, pid)?.used();
        {
            let mut ctx = RuntimeContext::for_process(rt, table, pid);
            let _ = call(&mut ctx, "erlang", "binary_to_term", 1, &args)?;
        }
        let used_after_two = process_heap(table, pid)?.used();
        let spaces_after = young_space_count(table, pid)?;
        if used_after_one != words_per_copy || used_after_two != words_per_copy * 2 {
            return Err(Failure::Error(format!(
                "the copy-out did not land {} / {} words on the young heap as expected \
                 (saw {used_after_one} / {used_after_two})",
                words_per_copy,
                words_per_copy * 2
            )));
        }

        // (2) How many attempts a copy-out really costs: a refused attempt (the nursery could not
        // hold the graph) retires exactly one nursery space and is retried once, so
        // `spaces - 1` over `RETRY_PROBE_COPIES` copies is the extra-attempt count. The nursery is
        // reset to its base first and NOT pre-grown, so the growth really happens.
        reset_young_heap(table, pid)?;
        {
            let mut ctx = RuntimeContext::for_process(rt, table, pid);
            for _ in 0..RETRY_PROBE_COPIES {
                let _ = call(&mut ctx, "erlang", "binary_to_term", 1, &args)?;
            }
        }
        let spaces_probe = young_space_count(table, pid)?;
        // Restore the pre-grown nursery the timed samples rely on.
        table
            .get_mut(pid)
            .ok_or_else(|| {
                Failure::Error("the harness's process vanished from its table".to_string())
            })?
            .grow_nursery(SCRATCH_HEAP_WORDS)
            .map_err(|error| Failure::Error(format!("cannot re-size the young heap: {error}")))?;
        reset_young_heap(table, pid)?;

        Ok(Deterministic {
            image_bytes: image_bytes.len(),
            words_per_copy,
            words_per_payload,
            used_after_one,
            used_after_two,
            spaces_before,
            spaces_after,
            grow_events_per_probe: spaces_probe.saturating_sub(1),
        })
    }

    /// Runs the interleaved sample loop and prints the report.
    fn measure_all(&mut self, config: &Config) -> Result<(), Failure> {
        let mut samples: Vec<Vec<Timing>> = (0..Row::ALL.len())
            .map(|_| Vec::with_capacity(config.samples))
            .collect();
        let erl = if config.oracle {
            match find_erl() {
                Some(path) => Some(path),
                None => {
                    println!();
                    println!("perf_copy_out: SKIP the oracle block - no `erl` on $PATH");
                    None
                }
            }
        } else {
            None
        };
        // enc, dec, round-trip, round-trip+match, payload build
        let mut sys_samples: Vec<Vec<f64>> = (0..5).map(|_| Vec::new()).collect();

        println!();
        println!("loadavg (before the timed blocks): {}", loadavg());
        println!("clock (before the timed blocks):   {}", clock_mhz());
        for round in 0..config.samples {
            // Alternate the row order every round, so a monotone drift in the host's load cannot
            // systematically favour one side of the A/B pair.
            if round % 2 == 0 {
                for row in Row::ALL {
                    let timing = self.measure_row(row, config.iterations)?;
                    samples[row.index()].push(timing);
                }
            } else {
                for row in Row::ALL.iter().rev() {
                    let timing = self.measure_row(*row, config.iterations)?;
                    samples[row.index()].push(timing);
                }
            }
            if let Some(erl) = erl.as_ref() {
                let point = sys_point(erl, SYS_ITERATIONS)?;
                let n = SYS_ITERATIONS as f64;
                sys_samples[0].push(marginal_us(point.encode, point.control, n));
                sys_samples[1].push(marginal_us(point.decode, point.control, n));
                sys_samples[2].push(marginal_us(point.round_trip, point.control, n));
                sys_samples[3].push(marginal_us(point.round_trip_match, point.control, n));
                sys_samples[4].push(marginal_us(point.build, point.control, n));
            }
        }
        println!("loadavg (after the timed blocks):  {}", loadavg());
        println!("clock (after the timed blocks):    {}", clock_mhz());

        print_rows(&samples);
        print_drift(&samples);
        print_attribution(&samples);
        if !sys_samples[0].is_empty() {
            print_sys(&sys_samples, &samples);
        }
        println!();
        println!("loadavg (end): {}", loadavg());
        println!("clock (end):   {}", clock_mhz());
        Ok(())
    }
}

/// The deterministic, wall-clock-independent facts about the workload.
struct Deterministic {
    /// The ETF image length, in bytes.
    image_bytes: usize,
    /// The copier's flat word count for the decoded graph (`size_of_copy`).
    words_per_copy: usize,
    /// `beam_term::size_object` of the payload as the `-eval` route built it.
    words_per_payload: usize,
    /// The young heap's used words after **one** copy-out.
    used_after_one: usize,
    /// The young heap's used words after **two** copy-outs.
    used_after_two: usize,
    /// The nursery's space count before the copies.
    spaces_before: usize,
    /// The nursery's space count after them (with ample capacity: no growth).
    spaces_after: usize,
    /// Growth-retired spaces over [`RETRY_PROBE_COPIES`] copies on a fresh nursery.
    grow_events_per_probe: usize,
}

/// Prints the workload's own shape: the numbers every ns/op below is "per".
fn print_fixture(harness: &Harness, report: &Deterministic) {
    println!();
    println!("workload: the published etf_roundtrip payload (a 5-key map)");
    println!("  ETF image                        : {} bytes", report.image_bytes);
    println!(
        "  words per copy (copier's count)  : {} (size_object of the -eval payload: {})",
        report.words_per_copy, report.words_per_payload
    );
    println!(
        "  young-heap words after 1 / 2 copies: {} / {} (expect {} / {})",
        report.used_after_one,
        report.used_after_two,
        report.words_per_copy,
        report.words_per_copy * 2
    );
    println!(
        "  nursery spaces before / after the copies: {} / {} (no growth expected)",
        report.spaces_before, report.spaces_after
    );
    println!(
        "  grow retries over {} copies        : {} -> {:.4} extra copy attempts per call",
        RETRY_PROBE_COPIES,
        report.grow_events_per_probe,
        report.grow_events_per_probe as f64 / RETRY_PROBE_COPIES as f64
    );
    println!(
        "  payload term raw word            : {:#x}, decoded {:#x} (distinct regions)",
        harness.payload.raw(),
        harness.decoded.raw()
    );
}

/// The minimum of a row's samples (nanoseconds per iteration).
fn min_row(values: &[Timing]) -> f64 {
    values.iter().map(|timing| timing.ns).fold(f64::INFINITY, f64::min)
}

/// The median of a row's samples (nanoseconds per iteration).
fn median_row(values: &[Timing]) -> f64 {
    let plain: Vec<f64> = values.iter().map(|timing| timing.ns).collect();
    median_of(&plain)
}

/// Prints one line per row: min and median ns/op, plus the marginal over the control row.
fn print_rows(samples: &[Vec<Timing>]) {
    let control = min_row(&samples[Row::Control.index()]);
    println!();
    println!(
        "{:<44} {:>6} {:>12} {:>12} {:>12} {:>10}",
        "row", "side", "min ns/op", "median ns/op", "min-control", "faults/op"
    );
    println!("{}", "-".repeat(102));
    for row in Row::ALL {
        let values = &samples[row.index()];
        let faults = values
            .iter()
            .map(|timing| timing.faults_per_call)
            .fold(f64::INFINITY, f64::min);
        println!(
            "{:<44} {:>6} {:>12.1} {:>12.1} {:>12.1} {:>10.5}",
            row.label(),
            row.side(),
            min_row(values),
            median_row(values),
            min_row(values) - control,
            faults
        );
    }
}

/// Prints each row's **within-sample drift**: the minimum of the second halves over the minimum of
/// the first halves. A ratio well above 1.0 means the row's cost grows as it runs - which, for a
/// copy-out-OFF row, is the pooled BIF heap being written without a rewind (fresh anonymous memory
/// every call, i.e. a page fault every few calls), not the code under test.
fn print_drift(samples: &[Vec<Timing>]) {
    println!();
    println!(
        "{:<44} {:>12} {:>12} {:>8}",
        "row", "min 1st half", "min 2nd half", "drift"
    );
    println!("{}", "-".repeat(80));
    for row in Row::ALL {
        let values = &samples[row.index()];
        let first = values
            .iter()
            .map(|timing| timing.first)
            .filter(|value| *value > 0.0)
            .fold(f64::INFINITY, f64::min);
        let second = values
            .iter()
            .map(|timing| timing.second)
            .filter(|value| *value > 0.0)
            .fold(f64::INFINITY, f64::min);
        let ratio = if first.is_finite() && first > 0.0 && second.is_finite() {
            second / first
        } else {
            f64::NAN
        };
        println!(
            "{:<44} {:>12.1} {:>12.1} {:>7.2}x",
            row.label(),
            first,
            second,
            ratio
        );
    }
}

/// Prints the cost decomposition the copy-out question is really about.
fn print_attribution(samples: &[Vec<Timing>]) {
    let control = min_row(&samples[Row::Control.index()]);
    let load = |row: Row| min_row(&samples[row.index()]) - control;
    let bin_a = load(Row::BinaryToTermNoCopy);
    let bin_b = load(Row::BinaryToTermCopyOut);
    let tiny_a = load(Row::TinyNoCopy);
    let tiny_b = load(Row::TinyProcess);
    let leaf_a = load(Row::LeafNoCopy);
    let leaf_b = load(Row::LeafProcess);
    let enc_a = load(Row::TermToBinaryNoCopy);
    let enc_b = load(Row::TermToBinaryCopyOut);
    let codec_decode = min_row(&samples[Row::CodecDecode.index()]);
    let codec_encode = min_row(&samples[Row::CodecEncode.index()]);
    let copy_struct = min_row(&samples[Row::CopyStruct.index()]);
    let copy_simple = min_row(&samples[Row::CopySimple.index()]);
    let copy_wrapper = min_row(&samples[Row::CopyOutWrapper.index()]);
    let eq = min_row(&samples[Row::StructuralEq.index()]);

    println!();
    println!("attribution (min-based, marginal over the control loop)");
    println!(
        "  prologue  is_binary/1         A {:>8.1}   B {:>8.1} ns   (cheapest possible BIF body)",
        leaf_a, leaf_b
    );
    println!(
        "  prologue  binary_to_term(tiny) A {:>8.1}   B {:>8.1} ns   (fixed body, no graph)",
        tiny_a, tiny_b
    );
    println!(
        "  the codec's own graph work     A {:>8.1} ns   (binary_to_term(payload) - binary_to_term(tiny))",
        bin_a - tiny_a
    );
    println!(
        "      the same difference on B   {:>8.1} ns   (includes the copy-out)",
        bin_b - tiny_b
    );
    println!(
        "  beam_codec::decode_term alone  {:>8.1} ns   (same image, codec called directly)",
        codec_decode
    );
    println!(
        "  binary_to_term/1  B - A delta  {:>8.1} ns   = copy-out + rewind + adoption",
        bin_b - bin_a
    );
    println!(
        "      copy-out wrapper (direct)  {:>8.1} ns   (fresh tables + copy + drains, rewound heap)",
        copy_wrapper
    );
    println!(
        "          copier's own walk      {:>8.1} ns   (copy_struct, tables reused)",
        copy_struct
    );
    println!(
        "          table/ctx construction {:>8.1} ns   (copy_struct_simple - copy_struct)",
        copy_simple - copy_struct
    );
    println!(
        "          the adoption loops     {:>8.1} ns   (wrapper - copy_struct_simple)",
        copy_wrapper - copy_simple
    );
    println!(
        "  term_to_binary/1 A {:>8.1}   B {:>8.1} ns   (B - A {:>6.1}: the 8-word binary copy)",
        enc_a,
        enc_b,
        enc_b - enc_a
    );
    println!(
        "      beam_codec::encode_term    {:>8.1} ns   (so the encode body is {:>6.1} ns of A)",
        codec_encode,
        enc_a - codec_encode
    );
    println!("  the case's equality match      {:>8.1} ns", eq);
}

/// `(total - control) / n` microseconds, expressed in nanoseconds per operation.
fn marginal_us(total_us: f64, control_us: f64, n: f64) -> f64 {
    ((total_us - control_us).max(0.0) / n) * 1_000.0
}

// --- the oracle side --------------------------------------------------------------

/// The oracle's per-row **microsecond** totals for one spawn.
#[derive(Clone, Copy, Default)]
struct SysPoint {
    /// `term_to_binary(P)` only.
    encode: f64,
    /// `binary_to_term(B)` only.
    decode: f64,
    /// `binary_to_term(term_to_binary(P))`, no match.
    round_trip: f64,
    /// The published case's shape: the pair plus the bound-variable equality match.
    round_trip_match: f64,
    /// `P` rebuilt per iteration (the published case's payload construction).
    build: f64,
    /// The empty loop at the same count.
    control: f64,
}

/// Finds `erl` on `$PATH` by a filesystem probe alone (the tool is never run to discover it).
fn find_erl() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("erl"))
        .find(|candidate| is_executable(candidate))
}

/// Whether `path` is an existing executable file.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Runs the oracle once at `n` iterations per row and returns its microsecond totals. The payload
/// is the very same `PAYLOAD_SRC` the Rust side materialises, so the two sides cannot drift.
fn sys_point(erl: &Path, n: u64) -> Result<SysPoint, Failure> {
    let program = format!(
        "\
P = {PAYLOAD_SRC},
B = term_to_binary(P),
N = {n},
Enc = fun E(0) -> ok; E(K) -> _ = term_to_binary(P), E(K - 1) end,
Dec = fun D(0) -> ok; D(K) -> _ = binary_to_term(B), D(K - 1) end,
Rt  = fun R(0) -> ok; R(K) -> _ = binary_to_term(term_to_binary(P)), R(K - 1) end,
RtM = fun M(0) -> ok; M(K) -> case binary_to_term(term_to_binary(P)) of P -> M(K - 1) end end,
Bld = fun Y(0) -> ok; Y(K) -> _ = {PAYLOAD_SRC}, Y(K - 1) end,
Ctl = fun C(0) -> ok; C(K) -> C(K - 1) end,
{{Ue,_}} = timer:tc(fun() -> Enc(N) end),
{{Ud,_}} = timer:tc(fun() -> Dec(N) end),
{{Ur,_}} = timer:tc(fun() -> Rt(N) end),
{{Um,_}} = timer:tc(fun() -> RtM(N) end),
{{Ub,_}} = timer:tc(fun() -> Bld(N) end),
{{Uc,_}} = timer:tc(fun() -> Ctl(N) end),
io:format(\"~w ~w ~w ~w ~w ~w~n\", [Ue, Ud, Ur, Um, Ub, Uc]),
halt(0)."
    );
    let output = Command::new(erl)
        .arg("-noshell")
        .arg("-eval")
        .arg(&program)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Failure::Error(format!("cannot run {}: {error}", erl.display())))?;
    if !output.status.success() {
        return Err(Failure::Error(format!(
            "the oracle exited {}: {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<f64> = text
        .split_whitespace()
        .map(|field| field.parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| {
            Failure::Error(format!(
                "the oracle printed no numeric figure: {text:?}{}",
                String::from_utf8_lossy(&output.stderr)
            ))
        })?;
    if fields.len() != 6 {
        return Err(Failure::Error(format!(
            "the oracle printed {} figures, expected 6: {text:?}",
            fields.len()
        )));
    }
    Ok(SysPoint {
        encode: fields[0],
        decode: fields[1],
        round_trip: fields[2],
        round_trip_match: fields[3],
        build: fields[4],
        control: fields[5],
    })
}

/// Prints the system runtime's rows next to the equivalent Rust rows.
fn print_sys(sys: &[Vec<f64>], rust: &[Vec<Timing>]) {
    let labels = [
        "erl: term_to_binary(P)",
        "erl: binary_to_term(B)",
        "erl: binary_to_term(term_to_binary(P))",
        "erl: the case's round trip + match",
        "erl: payload rebuild (!P per iteration)",
    ];
    println!();
    println!(
        "{:<44} {:>12} {:>12}",
        "system runtime (same host, same affinity)", "min ns/op", "median ns/op"
    );
    println!("{}", "-".repeat(72));
    for (index, label) in labels.iter().enumerate() {
        let values = &sys[index];
        println!(
            "{:<44} {:>12.1} {:>12.1}",
            label,
            min_of(values),
            median_of(values)
        );
    }
    // The published case's whole iteration on each side.
    let control = min_row(&rust[Row::Control.index()]);
    let sys_iteration = min_of(&sys[3]) + min_of(&sys[4]);
    let rust_iteration = (min_row(&rust[Row::TermToBinaryCopyOut.index()]) - control)
        + (min_row(&rust[Row::BinaryToTermCopyOut.index()]) - control)
        + min_row(&rust[Row::StructuralEq.index()]);
    println!("{}", "-".repeat(72));
    println!(
        "{:<44} {:>12.1}",
        "the whole etf_roundtrip iteration", sys_iteration
    );
    println!(
        "{:<44} {:>12.1}   (payload rebuild on the Rust side not measured, ~25-60 ns)",
        "  rust equivalent (excl. payload rebuild)", rust_iteration
    );
    println!(
        "{:<44} {:>12.2}x",
        "  rust / sys ratio (same parts)",
        rust_iteration / sys_iteration
    );
}

fn run() -> Result<(), Failure> {
    let config = parse_args()?;
    println!(
        "perf_copy_out: samples={} iterations={} oracle={}",
        config.samples, config.iterations, config.oracle
    );
    println!("affinity (this process): {}", affinity());
    println!("clock: {}", clock_mhz());
    println!("loadavg (start): {}", loadavg());

    let mut harness = Harness::new()?;
    let report = harness.verify_copy_out()?;
    print_fixture(&harness, &report);
    harness.measure_all(&config)
}
