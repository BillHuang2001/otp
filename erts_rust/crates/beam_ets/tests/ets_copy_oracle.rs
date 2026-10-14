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

//! The **oracle's** side of the `copy_bound/ets_copy` memory question.
//!
//! `tests/ets_memory.rs` measures this crate's own footprint and process RSS for
//! the exact `ets_copy` loop shape; this file measures the *other* side of every
//! ratio `PERF.md` quotes, so neither figure is copied from a previous benchmark
//! run:
//!
//! * **table memory** — the loop's one-key `set` table's `ets:info(T, memory)`
//!   (empty, after the first insert, after `N` inserts, and after the key is
//!   deleted), i.e. the oracle's counterpart of [`Table::memory`](beam_ets::Table::memory);
//! * **process peak RSS** — the oracle child's `VmHWM` (Linux `/proc/<pid>/status`,
//!   the same metric `tools/bench` polls; `tools/bench/RESULTS.md` §6.5) at three
//!   loop counts, so "the oracle's peak RSS does *not* track `N`" is measured here
//!   rather than asserted from the bench table.
//!
//! `ets:info(T, memory)` is a **word** count in ERTS exactly as
//! [`Table::memory`](beam_ets::Table::memory) is in this crate, so the two sides are
//! the same unit — but they are deliberately *not* expected to be equal (the
//! interiors, the copy trimming and the allocator bookkeeping all differ). What is
//! comparable, and what this test pins on the oracle side, is the **shape**: the
//! figure rises on the first insert, is flat across the rest of the loop, and
//! returns to the empty-table floor when the key is deleted.
//!
//! The oracle is required by default (see `common::oracle`); `BEAM_ETS_ALLOW_NO_ORACLE=1`
//! opts out with a loud skip.

mod common;

use std::fs;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::oracle;

/// The loop counts the oracle is run at: none (pure startup), the bench's *rust*
/// `ets_copy` `N`, and the bench's *sys* `ets_copy` `N` — a 20× range.
const LOOP_COUNTS: [usize; 3] = [0, 100_000, 2_000_000];

/// The wall-clock bound for one oracle run (the 2 000 000-op run takes ~0.5 s).
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// The `ets_copy` loop, verbatim from `tools/bench/src/workloads/copy_bound.rs`'s
/// generated module (the very `payload/0`, object `{k, T}` and `[N × PER_OP]`
/// accumulator the workload uses), extended with the one-key table's
/// `ets:info(memory)` readings.
///
/// `payload/0` is generated from that crate's constants (`PAYLOAD_LIST_LEN = 10`,
/// `PAYLOAD_BIN_BYTES = 12`, `PAYLOAD_FLOAT = 3.5`, `PAYLOAD_STRING = "a char list"`,
/// `PAYLOAD_NEG = -42`), which are mirrored here with the same values.
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main([NStr]) ->
    N = list_to_integer(NStr),
    Tab = ets:new(ets_copy, [set, public]),
    Empty = ets:info(Tab, memory),
    After1 = begin T0 = payload(), true = ets:insert(Tab, {k, T0}), ets:info(Tab, memory) end,
    Result = loop(N, Tab, 0),
    AfterN = ets:info(Tab, memory),
    ets:delete(Tab, k),
    Floor = ets:info(Tab, memory),
    ets:delete(Tab),
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    P("result", Result),
    P("mem.empty", Empty),
    P("mem.after_1_insert", After1),
    P("mem.after_n_inserts", AfterN),
    P("mem.after_delete", Floor),
    P("mem.flat_over_loop", AfterN =:= After1),
    P("mem.returns_to_floor", Floor =:= Empty),
    ok.

payload() ->
    #{a => [1,2,3,4,5,6,7,8,9,10],
      b => {ok, <<"hello world">>, 3.5},
      c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
      d => "a char list",
      e => -42}.

loop(0, _Tab, Acc) -> Acc;
loop(N, Tab, Acc) ->
    T = payload(),
    true = ets:insert(Tab, {k, T}),
    case ets:lookup(Tab, k) of
        [{k, T}] -> loop(N - 1, Tab, Acc + 1)
    end.
"#;

/// One oracle run: its parsed `key -> value` lines plus the child's peak RSS.
struct Run {
    lines: std::collections::BTreeMap<String, String>,
    peak_kib: Option<u64>,
}

impl Run {
    fn word(&self, key: &str) -> i64 {
        self.lines
            .get(key)
            .unwrap_or_else(|| panic!("the oracle printed no {key:?} line"))
            .parse()
            .unwrap_or_else(|_| panic!("the oracle's {key:?} is not an integer"))
    }

    fn flag(&self, key: &str) -> bool {
        match self.lines.get(key).map(String::as_str) {
            Some("true") => true,
            Some("false") => false,
            other => panic!("the oracle's {key:?} is not a boolean: {other:?}"),
        }
    }
}

/// The `VmHWM` (peak resident set size) of `pid`, in KiB, from
/// `/proc/<pid>/status` — the metric `tools/bench` polls (`RESULTS.md` §6.5).
fn peak_kib(pid: u32) -> Option<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines().find_map(|line| {
        let rest = line.strip_prefix("VmHWM:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// Spawn the oracle and poll its `VmHWM` while it runs, returning its output plus
/// the largest reading seen.
///
/// The poll can only see a live process, so it samples every millisecond from the
/// moment of `spawn` until `try_wait` reports the exit; the loop is long enough
/// (hundreds of milliseconds) that the peak is always observed, and a bounded
/// timeout kills a wedged oracle instead of hanging the suite.
fn run_polling(escript: &std::path::Path, program: &std::path::Path, n: usize) -> (Output, Option<u64>) {
    let mut command = Command::new(escript);
    command
        .arg(program)
        .arg(n.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child: Child = command.spawn().expect("the oracle escript spawns");
    let pid = child.id();
    let mut peak: Option<u64> = None;
    let mut child = child;
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        if let Some(hwm) = peak_kib(pid) {
            peak = Some(peak.map_or(hwm, |seen| seen.max(hwm)));
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the oracle escript did not exit within {RUN_TIMEOUT:?}");
            }
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("the oracle escript could not be waited for: {error}"),
        }
    }
    let output = child
        .wait_with_output()
        .expect("the oracle escript's output is readable");
    assert!(
        output.status.success(),
        "the oracle escript exited with {}:\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output, peak)
}

/// The oracle's `ets_copy` loop: its peak RSS does not track the loop count, and
/// its one-key table's `ets:info(memory)` rises on the first insert, stays flat
/// across the loop, and returns to the empty-table floor on delete.
#[test]
fn the_oracles_ets_copy_loop_keeps_its_peak_rss_and_one_key_memory_flat() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "the_oracles_ets_copy_loop_keeps_its_peak_rss_and_one_key_memory_flat",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
        );
        return;
    };
    if peak_kib(std::process::id()).is_none() {
        // The RSS half of the measurement needs Linux `/proc`; the memory half
        // would still be meaningful, but a partial run is worse than a loud skip.
        oracle::skip(
            "the_oracles_ets_copy_loop_keeps_its_peak_rss_and_one_key_memory_flat",
            "this host exposes no /proc/<pid>/status VmHWM field",
        );
        return;
    }

    let scratch = oracle::Scratch::new("ets_copy_rss");
    let program = scratch.write("ets_copy.escript", ORACLE_PROGRAM);
    let loadavg = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!(
        "[ets_copy_oracle] escript={} loadavg={}",
        escript.display(),
        loadavg.trim()
    );

    let mut runs: Vec<(usize, Run)> = Vec::new();
    for n in LOOP_COUNTS {
        let (output, peak) = run_polling(&escript, &program, n);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let run = Run {
            lines: common::parse_lines(&stdout),
            peak_kib: peak,
        };

        // The loop's own deterministic counter: `N` ops, `N × PER_OP` with
        // `PER_OP = 1` — the same self-validation the workload relies on.
        assert_eq!(
            run.word("result"),
            n as i64,
            "the oracle's own accumulator disagrees with N={n}:\n{stdout}"
        );
        // The oracle's reclamation shape for this loop's one-key table.
        assert!(
            run.word("mem.after_1_insert") > run.word("mem.empty"),
            "N={n}: the oracle's empty-table memory did not rise on the first insert"
        );
        assert!(
            run.flag("mem.flat_over_loop"),
            "N={n}: the oracle's one-key table memory grew over the loop \
             ({} -> {})",
            run.word("mem.after_1_insert"),
            run.word("mem.after_n_inserts")
        );
        assert!(
            run.flag("mem.returns_to_floor"),
            "N={n}: the oracle's table memory did not return to its empty floor on delete \
             ({} -> {})",
            run.word("mem.empty"),
            run.word("mem.after_delete")
        );

        println!(
            "[ets_copy_oracle] N={n:>8} result={} mem.empty={} words mem.after_1_insert={} words \
             mem.after_n_inserts={} words mem.after_delete={} words peak VmHWM={:?} KiB",
            run.word("result"),
            run.word("mem.empty"),
            run.word("mem.after_1_insert"),
            run.word("mem.after_n_inserts"),
            run.word("mem.after_delete"),
            run.peak_kib,
        );
        runs.push((n, run));
    }

    // The peak RSS must be loop-length invariant: the oracle's table never grows,
    // so the only per-op memory is the ephemeral copy the BEAM's own GC reclaims.
    let (n0, first) = runs.first().expect("at least one run");
    let (nmax, last) = runs.last().expect("at least one run");
    let peaks: Vec<Option<u64>> = runs.iter().map(|(_, run)| run.peak_kib).collect();
    assert!(
        peaks.iter().all(Option::is_some),
        "a polled oracle run produced no VmHWM reading: {peaks:?}"
    );
    let growth = last.peak_kib.unwrap().saturating_sub(first.peak_kib.unwrap());
    println!(
        "[ets_copy_oracle] peak VmHWM N={n0} -> N={nmax}: {} -> {} KiB (growth {growth} KiB = {:.2} MiB)",
        first.peak_kib.unwrap(),
        last.peak_kib.unwrap(),
        growth as f64 / 1024.0
    );
    assert!(
        growth < 8 * 1024,
        "the oracle's peak RSS tracked the loop count: {growth} KiB over N={n0}..{nmax} \
         (peaks {peaks:?}); the ets_copy loop must not grow the oracle's footprint"
    );
}
