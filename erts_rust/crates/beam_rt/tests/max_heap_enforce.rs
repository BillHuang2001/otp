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

//! `max_heap_size` **enforcement**, cross-checked against the system `erl` oracle (contracts
//! A.6/A.8).
//!
//! The flag's *parsing* is pinned by the sibling `tests/max_heap_flag.rs`; this suite pins what
//! the bound **does**: a breach kills the process with the bare atom `killed`, or logs and lets it
//! run, and the check is on the collected **capacity** rather than the live occupancy.
//!
//! # Why every case is driven through the scheduler `run` route
//!
//! A collection records the verdict on the PCB and the runtime's collection door **latches** it
//! ([`beam_rt::Runtime`]'s `max_heap_kill`); the runner performs a latched kill at its next slice
//! boundary. The `-eval` action process is never enqueued on the run queue, so a verdict latched
//! there is never drained — a documented residual (`M1_GAPS.md` *Heap switches*), not something
//! this suite can pin. Every case therefore runs a **compiled fixture function** through
//! `beam_rt run <maxheap.beam> maxheap:<f>/0`, and the oracle is asked the same function through
//! an ordinary `-eval` call, so the only difference between the two runs is which runtime executes
//! it.
//!
//! # The observables, and what is *not* compared
//!
//! * **stdout** — every fixture function returns a term built without pids or references
//!   (`is_pid/1` / `is_reference/1` stand in for them), so the printed value is compared **byte
//!   for byte** with the oracle's, together with the exit code.
//! * **the breach report** — this runtime writes it to **stderr**, synchronously from the
//!   collection site; the oracle's is a `logger` message that lands on **stdout** and carries a
//!   timestamp. So the reports are compared **structurally** (the field names, their order and
//!   the values that are not runtime-specific) and never as raw bytes. See *Divergences* below.
//! * **the number of reports** — one per over-limit collection. Both figures are this runtime's
//!   own state: the count of emitted blocks is held against the collection-counter delta the
//!   fixture itself reads (`erlang:statistics(garbage_collection)`), and **never** against the
//!   oracle, whose collection cadence differs by construction (it answers 14 collections where
//!   this runtime answers 8 — a live GC counter, which `tests/CONTEXT.md` forbids comparing).
//!
//! # Measured oracle answers (OTP 29.1.1 / erts-17.1)
//!
//! ```none
//! kill_at_bound            {died,true,killed}          1 report, Kill: true
//! kill_at_bound_monitor    {down,true,true,killed}     1 report, Kill: true
//! log_and_continue         {survived,completed}        13 reports, Kill: false
//! log_and_continue_silent  {survived,completed}        0 reports
//! capacity_not_occupancy   {survived,{100,true}}       4 reports, Total Heap Size 1220 > 700
//! floor                    [badarg,accepted,accepted,accepted,accepted,badarg,badarg,badarg]
//! spawn_opt_log            {survived,true,completed}   13 reports
//! spawn_opt_kill           {died,true,true,killed}     1 report
//! spawn_opt_info           [{error_logger,false},{include_shared_binaries,false},
//!                           {kill,false},{size,2000},4]
//! spawn_opt_floor          [badarg,badarg,accepted]
//! ```
//!
//! # Divergences (documented, pinned here; recorded in `M1_GAPS.md`)
//!
//! 1. **Stream and header.** The oracle's report is a `logger` message: it lands on stdout with a
//!    `=ERROR REPORT==== <timestamp> ===` header. This runtime has no `logger` process, so the
//!    report is written synchronously to **stderr** with a bare `=ERROR REPORT====` header.
//! 2. **Field set.** The oracle prints nine fields; the last two — `GC Info` (the C's
//!    `erts_process_gc_info` proplist) and `Stacktrace` — are **omitted** rather than fabricated.
//!    The seven this runtime prints are a **prefix** of the oracle's, with the oracle's own names,
//!    order and column layout.
//! 3. **When a kill takes effect.** The oracle dies in the collection that decided the kill; here
//!    the kill is performed at the runner's next slice boundary, so the victim may run the rest of
//!    its slice. The report is emitted **once** regardless (the latch's first insert), which is
//!    what keeps the observable identical to the oracle's single report.
//!
//! # Skip policy
//!
//! No `erl` on `$PATH` (and no executable `$ERTS_ORACLE`) is a **loud** skip per test, never a
//! silent pass ([`support::require_oracle`]).
#![deny(unsafe_code)]

mod support;

use std::path::{Path, PathBuf};

/// The runtime binary under test (cargo builds it for this suite).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The banner the loud skip prints.
const BANNER: &str = "max_heap_enforce";

/// The committed enforcement fixture (`tests/fixtures/maxheap.beam`).
const MAXHEAP_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/maxheap.beam");

/// The seven field names this runtime emits, in the oracle's own order (`erl_gc.c:3763-3773`).
const FIELDS: [&str; 7] = [
    "Process",
    "Context",
    "Max Heap Size",
    "Total Heap Size",
    "Kill",
    "Error Logger",
    "Message Queue Len",
];

// ---------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and its trimmed stdout/stderr.
struct Run {
    /// The child's exit code (`-1` when it was signalled).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Untrimmed stderr — the report blocks are matched line by line.
    err: String,
}

/// A scratch directory holding **only** `maxheap.beam`.
///
/// `tests/fixtures/` must never be handed to the oracle as a single `-pa`: it also holds a
/// `lists.beam`, which would shadow the real stdlib module. The directory is unique per call
/// (label + pid + a counter) so concurrent tests cannot collide, and it leaks deliberately —
/// removing it would race a sibling probe.
fn scratch_dir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_max_heap_{label}_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("the scratch directory is created");
    std::fs::copy(MAXHEAP_BEAM, dir.join("maxheap.beam")).expect("the fixture is copied");
    dir
}

/// `beam_rt run maxheap.beam maxheap:<function>/0` — the scheduler route, the only route that
/// drains a latched kill.
fn run_route(function: &str) -> Run {
    let spec = format!("maxheap:{function}/0");
    let output = support::run_bounded(Path::new(BINARY), &["run", MAXHEAP_BEAM, &spec])
        .unwrap_or_else(|| panic!("beam_rt could not run {spec} within the probe bound"));
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The answer `function` gives under the oracle: `(the printed value, the report text)`.
///
/// The value is printed with a `VALUE ` marker so it can be told apart from the report blocks the
/// oracle emits on the **same** stream, and a short sleep lets the asynchronous `logger` flush
/// before `halt(0)` — without it a single-report case loses its report to the halt race (measured).
/// `None` — after a loud skip line — when the oracle could not answer.
fn oracle_answer(oracle: &Path, function: &str) -> Option<(String, String)> {
    let scratch = scratch_dir("oracle");
    let call = format!(
        "io:format(\"VALUE ~w~n\",[maxheap:{function}()]), timer:sleep(300), halt(0)."
    );
    let Some(stdout) = support::oracle_eval(oracle, Some(scratch.as_path()), &call) else {
        eprintln!("SKIP the max_heap_enforce probe for {function}: the oracle could not answer");
        return None;
    };
    let mut value = String::new();
    let mut reports = Vec::new();
    for line in stdout.lines() {
        match line.strip_prefix("VALUE ") {
            Some(rest) => value = rest.trim().to_string(),
            None => reports.push(line),
        }
    }
    assert!(
        !value.is_empty(),
        "{function}: the oracle must print the VALUE line, got {stdout:?}"
    );
    Some((value, reports.join("\n")))
}

/// Asserts that `function`'s virtual-machine observable agrees: this runtime exits `0`, prints the
/// oracle's bytes and — when `reports` is `Some` — emits the same number of report blocks.
///
/// The report **count** is compared only for the cases where the C's cadence is one-per-kill
/// (`Some(1)`), never for a log-and-continue workload whose collection cadence legitimately differs.
fn agrees(oracle: &Path, function: &str, reports: Option<usize>) -> Option<Run> {
    let (expected, _oracle_reports) = oracle_answer(oracle, function)?;
    let ours = run_route(function);
    assert_eq!(
        ours.code, 0,
        "{function}: `beam_rt run` must exit 0, stderr — {}",
        ours.err
    );
    assert_eq!(ours.out, expected, "{function}: the two runtimes must agree");
    if let Some(count) = reports {
        assert_eq!(
            blocks(&ours.err).len(),
            count,
            "{function}: report blocks — {}",
            ours.err
        );
    }
    Some(ours)
}

// ---------------------------------------------------------------------------------------
// The report blocks
// ---------------------------------------------------------------------------------------

/// Splits stderr into `=ERROR REPORT====` blocks, each a `Vec` of its lines.
fn blocks(err: &str) -> Vec<Vec<String>> {
    let mut blocks: Vec<Vec<String>> = Vec::new();
    for line in err.lines() {
        if line == "=ERROR REPORT====" {
            blocks.push(Vec::new());
        } else if let Some(block) = blocks.last_mut() {
            block.push(line.to_string());
        }
    }
    blocks
}

/// The field names of a report block, in the order they appear (a continuation line — the C's
/// `GC Info` proplist — names nothing and is skipped).
fn field_names(block: &[String]) -> Vec<String> {
    block
        .iter()
        .filter_map(|line| {
            let name = line.trim_start().split(':').next()?.trim().to_string();
            let alphabetic = !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_alphabetic())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphabetic() || c == ' ');
            alphabetic.then_some(name)
        })
        .collect()
}

/// One report block's value for the field called `name`.
fn field(block: &[String], name: &str) -> String {
    let prefix = format!("     {name}:");
    block
        .iter()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the report block must carry `{name}`, got {block:?}"))
        .trim()
        .to_string()
}

/// Asserts `block` is exactly the seven-field block this runtime documents, and that the seven
/// values satisfy `check`.
///
/// The **column layout** is pinned byte-exactly on the `Context` line (five leading spaces and the
/// same column as the oracle's), and every other line is pinned by name + value so the pid number
/// and the heap figures (runtime-specific by construction) stay out of the assertion.
fn assert_block(block: &[String], check: impl Fn(&str, &str)) {
    assert!(
        block
            .first()
            .is_some_and(|line| line.starts_with("     Process:")),
        "a block starts with `Process:`, got {block:?}"
    );
    assert_eq!(
        field_names(block),
        FIELDS.to_vec(),
        "the block's field set and order are the oracle's own"
    );
    assert!(
        block[1..].iter().any(|line| line
            == "     Context:            maximum heap size reached"),
        "the `Context` line's column layout, got {block:?}"
    );
    assert_eq!(field(block, "Context"), "maximum heap size reached");
    assert_eq!(field(block, "Error Logger"), "true");
    let process = field(block, "Process");
    assert!(
        process.starts_with('<') && process.ends_with('>'),
        "`Process` renders as a pid, got {process}"
    );
    assert!(
        field(block, "Max Heap Size").parse::<u64>().is_ok(),
        "`Max Heap Size` is an integer"
    );
    assert!(
        field(block, "Total Heap Size").parse::<u64>().is_ok(),
        "`Total Heap Size` is an integer"
    );
    assert!(
        field(block, "Message Queue Len").parse::<u64>().is_ok(),
        "`Message Queue Len` is an integer"
    );
    check(&field(block, "Kill"), &field(block, "Total Heap Size"));
}

/// The oracle's report field names, which this runtime's seven must be a **prefix** of.
fn oracle_field_names(reports: &str) -> Vec<String> {
    reports
        .lines()
        .skip_while(|line| !line.starts_with("=ERROR REPORT"))
        .skip(1)
        .map(|line| line.trim_start().split(':').next().unwrap_or("").trim())
        .filter(|name| {
            !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_alphabetic())
                && name.chars().all(|c| c.is_ascii_alphabetic() || c == ' ')
        })
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------------------
// 1 — the kill at the bound
// ---------------------------------------------------------------------------------------

/// A **trapping linked parent** sees the victim die with the bare atom `killed`.
///
/// The victim sets `process_flag(trap_exit, true)` *and* wraps its work in a `try … catch`, so the
/// bare `killed` in the parent's `'EXIT'` is the proof that the kill is neither trappable nor
/// catchable — a `try` that caught something would have answered `{survived,caught}`.
///
/// Measured: both runtimes print `{died,true,killed}`, exit 0.
#[test]
fn a_trapping_linked_parent_sees_the_bare_atom_killed() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    agrees(&oracle, "kill_at_bound", Some(1));
}

/// A **monitor** sees `'DOWN' … killed`, again with the bare atom.
///
/// Measured: both runtimes print `{down,true,true,killed}`, exit 0.
#[test]
fn a_monitor_sees_the_down_with_the_bare_atom_killed() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    agrees(&oracle, "kill_at_bound_monitor", Some(1));
}

/// A killing breach **also reports**, exactly once, with `Kill: true`.
///
/// The two flags are independent in the C: `reached_max_heap_size` builds and sends the report and
/// *then* returns the kill flag (`erl_gc.c:3744-3803`), so the oracle prints one report and dies in
/// that same collection — measured, `Kill: true` and `{died,true,killed}`. Here the death is
/// deferred to the runner's slice boundary, so the report is emitted once at the latch and the
/// count is pinned at **one** (a per-collection cadence would have emitted dozens: measured 42
/// before the latch gate).
#[test]
fn a_killing_breach_reports_once_about_the_kill() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "kill_at_bound", Some(1)) else {
        return;
    };
    let blocks = blocks(&ours.err);
    assert_block(&blocks[0], |kill, total| {
        assert_eq!(kill, "true", "the report names the kill it performed");
        assert_eq!(total, "1220", "the capacity the collection installed");
    });
    assert_eq!(field(&blocks[0], "Max Heap Size"), "1000");
    // The oracle's report is the same seven fields first, then `GC Info` and `Stacktrace`.
    let (_, reports) = oracle_answer(&oracle, "kill_at_bound").expect("the oracle answered above");
    let names = oracle_field_names(&reports);
    assert_eq!(
        &names[..FIELDS.len().min(names.len())],
        &FIELDS[..FIELDS.len().min(names.len())],
        "this runtime's fields are a prefix of the oracle's, in the oracle's order — got {names:?}"
    );
}

// ---------------------------------------------------------------------------------------
// 2 — log and continue
// ---------------------------------------------------------------------------------------

/// `kill => false, error_logger => true`: the child survives, runs to completion, and every
/// over-limit collection emits one block.
///
/// Measured: both runtimes print `{survived,completed}`; this runtime emits **42** blocks for the
/// 2000-iteration workload (the oracle 13 — its collection cadence differs by construction and the
/// count is never compared). The exact per-block values are pinned for the first block only; every
/// block is shape-checked.
#[test]
fn a_non_killing_breach_logs_and_the_process_survives() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "log_and_continue", None) else {
        return;
    };
    let blocks = blocks(&ours.err);
    assert!(
        !blocks.is_empty(),
        "a `error_logger => true` breach must report"
    );
    for block in &blocks {
        assert_block(block, |kill, _| assert_eq!(kill, "false", "no kill was configured"));
        assert_eq!(field(block, "Max Heap Size"), "1000");
    }
    assert_eq!(
        blocks[0].len(),
        FIELDS.len(),
        "exactly the seven documented fields, no more"
    );
}

/// `error_logger => false`: not one report, and the child still survives.
///
/// Measured: 0 blocks on both runtimes; `{survived,completed}`.
#[test]
fn error_logger_false_is_silent() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "log_and_continue_silent", Some(0)) else {
        return;
    };
    assert_eq!(ours.err.trim(), "", "no report at all");
}

/// **One report per over-limit collection** — measured, not assumed.
///
/// The fixture reads this runtime's **own** node-global collection counter
/// (`erlang:statistics(garbage_collection)`) around its work and returns the delta, so the number
/// of blocks is held against the number of collections the same runtime performed. The bound sits
/// below the very first capacity a collection installs (`size => 300`), so every collection in the
/// window is over the limit; measured, `{survived,8}` with **8** blocks — exactly one each.
///
/// Neither figure is compared with the oracle (a live GC counter must never be — `tests/CONTEXT.md`);
/// only the shape of the oracle's answer is cross-checked.
#[test]
fn one_report_per_over_limit_collection() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some((expected, _)) = oracle_answer(&oracle, "reports_per_collection") else {
        return;
    };
    assert!(
        expected.starts_with("{survived,"),
        "the oracle's shape is `{{survived,N}}`, got {expected}"
    );
    let ours = run_route("reports_per_collection");
    assert_eq!(ours.code, 0, "beam_rt exited {}: {}", ours.code, ours.err);
    let collections: u64 = ours
        .out
        .strip_prefix("{survived,")
        .and_then(|rest| rest.strip_suffix('}'))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("the fixture answers `{{survived,N}}`, got {}", ours.out));
    assert!(collections >= 1, "the workload must collect at least once");
    assert_eq!(
        blocks(&ours.err).len() as u64,
        collections,
        "one report per over-limit collection ({collections} collections)"
    );
}

// ---------------------------------------------------------------------------------------
// 3 — the `badarg` floor
// ---------------------------------------------------------------------------------------

/// `size` below the process's own `min_heap_size` is `badarg`; the floor itself is accepted.
///
/// The probe is `#{size => 232}` / `#{size => 233}` / `#{size => 234}` around this runtime's
/// default `min_heap_size` of 233 words (the oracle's own default), plus `size => 0`, the bare
/// integer `0`, a negative integer, a float and a map without `size`.
///
/// Measured, identical on both runtimes:
/// `[badarg,accepted,accepted,accepted,accepted,badarg,badarg,badarg]`.
#[test]
fn the_floor_is_the_processes_own_min_heap_size() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "floor", Some(0)) else {
        return;
    };
    assert_eq!(
        ours.out,
        "[badarg,accepted,accepted,accepted,accepted,badarg,badarg,badarg]",
        "one below the floor is `badarg`, the floor and above are accepted"
    );
}

// ---------------------------------------------------------------------------------------
// 4 — capacity, not occupancy
// ---------------------------------------------------------------------------------------

/// The bound is checked against the **capacity a collection installs**, never against the live
/// words: the victim holds a 100-element list (under 100 words of live data) for its whole life and
/// still breaches `#{size => 700}`.
///
/// Measured: both runtimes print `{survived,{100,true}}`; the report's `Total Heap Size` is this
/// runtime's 1220 — far above the 700 bound while the live data is a fraction of it. The figure is
/// pinned because it is this runtime's own; the oracle's is 1220 as well here, but it is not
/// compared (a heap capacity is runtime-specific).
#[test]
fn the_bound_is_on_the_collected_capacity_not_the_live_occupancy() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "capacity_not_occupancy", None) else {
        return;
    };
    let blocks = blocks(&ours.err);
    assert!(!blocks.is_empty(), "the breach must report");
    assert_block(&blocks[0], |kill, total| {
        assert_eq!(kill, "false");
        assert!(
            total.parse::<u64>().expect("an integer") > 700,
            "the capacity is far above the bound while the live data is not"
        );
    });
    assert_eq!(
        field(&blocks[0], "Max Heap Size"),
        "700",
        "the bound the process set"
    );
    assert!(
        ours.out.ends_with("{100,true}}"),
        "all 100 elements survived the collections, got {}",
        ours.out
    );
}

// ---------------------------------------------------------------------------------------
// 5 — `spawn_opt`'s `{max_heap_size, Map}` carries the flags
// ---------------------------------------------------------------------------------------

/// `spawn_opt`'s map is installed with its **flags**, not just its `size`: `kill => false` +
/// `error_logger => true` logs and lets the child finish.
///
/// Measured: both runtimes print `{survived,true,completed}`; this runtime emits 3 blocks, the
/// oracle 13 (cadence, never compared).
#[test]
fn spawn_opt_logs_and_continues() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "spawn_opt_log", None) else {
        return;
    };
    let blocks = blocks(&ours.err);
    assert!(!blocks.is_empty(), "`error_logger => true` must report");
    for block in &blocks {
        assert_block(block, |kill, _| assert_eq!(kill, "false"));
    }
}

/// The same shape with `kill => true`: the child dies at the bound and the monitor sees `killed`
/// — the flags are no longer dropped.
///
/// Measured: both runtimes print `{died,true,true,killed}`, one report.
#[test]
fn spawn_opt_kills() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "spawn_opt_kill", Some(1)) else {
        return;
    };
    let blocks = blocks(&ours.err);
    assert_block(&blocks[0], |kill, _| assert_eq!(kill, "true"));
}

/// `process_info(P, max_heap_size)` reports the setting `spawn_opt` installed, in the oracle's own
/// key order.
///
/// Measured, identical on both runtimes:
/// `[{error_logger,false},{include_shared_binaries,false},{kill,false},{size,2000},4]`.
#[test]
fn spawn_opt_installs_the_whole_setting() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "spawn_opt_info", Some(0)) else {
        return;
    };
    assert_eq!(
        ours.out,
        "[{error_logger,false},{include_shared_binaries,false},{kill,false},{size,2000},4]",
        "the whole flag map, in the C's key order"
    );
}

/// `spawn_opt`'s `max_heap_size` honours the child's **own** floor: with `{min_heap_size, 1000}`
/// (rounded up through the growth table to 1597), `size => 1000` and `size => 1597` are `badarg`
/// while `size => 1598` is accepted.
///
/// Measured, identical on both runtimes: `[badarg,badarg,accepted]`.
#[test]
fn spawn_opt_respects_the_childs_floor() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(ours) = agrees(&oracle, "spawn_opt_floor", Some(0)) else {
        return;
    };
    assert_eq!(ours.out, "[badarg,badarg,accepted]");
}
