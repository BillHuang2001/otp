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

//! **Peak RSS is bounded for the allocate-and-abandon churn loop.**
//!
//! The `gc_bound` benchmark workload ([`churn.beam`](fixtures/churn.erl), mirrored from
//! `tools/bench/src/workloads/gc_bound.rs`'s embedded source) is the allocate-and-abandon
//! shape the collector is on the hook for: every iteration builds a 256-element list, a
//! 256-byte binary and a 256-element tuple, discards all three, and checks the two
//! O(1)-checkable ones. `churn:run(N)` therefore returns exactly `N`.
//!
//! This suite is the acceptance evidence for the item-#10/#10b fix in
//! `RuntimeContext::call_registered`: a registered-BIF result is copied into the running
//! process's **scanned** heap (and the pooled BIF heap is rewound), so a term the process
//! holds across the loop is no longer an *unscanned root* that makes the collector's
//! valve decline every process-heap collection. It pins both halves of that:
//!
//! 1. [`peak_rss_does_not_scale_with_the_churn_count`] — a 10× larger `N` must not add more
//!    than a few MiB to the process's `VmHWM`. The pre-fix commit grew roughly **linearly**
//!    (the same workload's documented pre-fix curve, `tools/bench/RESULTS.md` §6.7, is
//!    29 MiB → 227 MiB for a 20× `N` step, i.e. ~+190 MiB for this suite's 10× step): at the
//!    fix's HEAD the churn payload itself is flat — the 10× step adds **~0.1 MiB**. (The
//!    absolute `VmHWM`, ≈381 MiB on this host, is the runtime's *fixed boot cost*: every
//!    action now drives the boot script and eagerly loads its modules, so the *bound* is the
//!    growth across `N`, not the absolute figure.)
//! 2. [`the_churn_loop_really_collects`] — the *deterministic* half, with no RSS at all:
//!    driven through [`Runtime::run_scheduled`], the loop's value is exactly `N` **and**
//!    [`Runtime::gc_stats`] reports non-zero collections. Before the fix the valve declined
//!    every collection and the counter stayed `0`.
//!
//! ## Fixture
//!
//! `tests/fixtures/churn.beam` is a committed `.beam` image produced by the system `erlc`
//! (`+deterministic`; the recipe is in `tests/fixtures/README.md`), so the suite needs no
//! compiler — only the system Erlang installation the boot sequence resolves. A missing
//! installation is a **loud skip**, never a silent pass.
//!
//! ## Host-load tolerance
//!
//! `VmHWM` is the kernel's own monotonic high-water mark — it is **not** a wall-clock
//! measurement — so a loaded shared host does not inflate it. The bound is nevertheless
//! given generous slack (12 MiB, against the ~190 MiB a linear pre-fix run adds) and the
//! measured figures plus `/proc/loadavg` are printed, so a failure is diagnosable rather
//! than mysterious.

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use beam_rt::Runtime;
use beam_term::{is_small, make_small, signed_value};

/// The committed churn image. It is passed to the runtime by **path** (the `run` action /
/// `run_scheduled`), never through a `-pa` on the fixtures directory — that directory holds a
/// `lists.beam` which would shadow stdlib's.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/churn.beam");

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The two churn counts the RSS bound is measured at — a 10× step.
const SMALL_N: i64 = 2_000;
/// The larger churn count (see [`SMALL_N`]).
const LARGE_N: i64 = 20_000;

/// The churn count the deterministic (non-RSS) collection test drives.
const COLLECT_N: i64 = 2_000;

/// How close to the child's death the peak `VmHWM` is sampled. `VmHWM` is a watermark, so a
/// small interval is enough to observe it before the process is reaped.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(5);

/// How long the CLI child may run before it is killed. `N = 20_000` is a few seconds of
/// interpreted work, so this is a safety net, not the normal path.
const CLI_TIMEOUT: Duration = Duration::from_secs(300);

/// The allowed `VmHWM` growth for the 10× `N` step.
///
/// The churn payload is collected, so nothing accumulates: the measured 10× step adds
/// **~0.1 MiB** (the absolute `VmHWM` — ≈381 MiB on this host — is the runtime's fixed boot
/// cost, not the bound), so this is pure headroom for a loaded host. For scale: the documented
/// pre-fix curve (`tools/bench/RESULTS.md` §6.7) is 29 MiB → 227 MiB for a 20× `N` step — a
/// 10× step at this suite's counts therefore added ~190 MiB pre-fix, ~15× this bound.
const RSS_SLACK_KIB: u64 = 12 * 1024;

/// The looser second bound: even with a pathological allocator the larger run must not reach
/// 1.4× the smaller.
const RSS_MAX_RATIO: f64 = 1.4;

/// One measured CLI run: the churn count, the peak `VmHWM` observed, what it printed, and how
/// many `/proc` samples were taken.
#[derive(Debug, Clone)]
struct Measurement {
    /// The churn count the child was asked for.
    n: i64,
    /// The largest `VmHWM` (kB) observed while the child lived.
    vmhwm_kib: u64,
    /// The child's trimmed stdout (the `run` action prints `~w~n`).
    stdout: String,
    /// The number of successful `/proc/<pid>/status` samples (`0` ⇒ the whole run was missed).
    samples: usize,
}

impl Measurement {
    /// The peak in MiB, for the printed report.
    fn mib(&self) -> f64 {
        self.vmhwm_kib as f64 / 1024.0
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

/// Runs `beam_rt run <FIXTURE> churn:run/1 <n>` and samples its `/proc/<pid>/status` for
/// `VmHWM` until it exits.
///
/// The child is **polled** rather than waited on, so a regression is a loud failure (killed,
/// reaped and reported) instead of a hang — the same discipline as `support::run_probe`.
/// Because `VmHWM` is a monotonic watermark, the largest sample is the process's true peak.
fn measure_peak_rss(erlang_n: i64) -> Measurement {
    // `-noshell`: the `run` action is banner-free by construction (it records the `noshell`
    // init argument, so the booted kernel prints no OTP slogan) and prints only its own `~w~n`
    // line (the `N` the loop returned); it is kept to mirror the oracle probe.
    let args = [
        "-noshell".to_string(),
        "run".to_string(),
        FIXTURE.to_string(),
        "churn:run/1".to_string(),
        erlang_n.to_string(),
    ];
    let mut child: Child = Command::new(BINARY)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn {BINARY}: {error}"));
    let pid = child.id();

    let mut vmhwm_kib = 0u64;
    let mut samples = 0usize;
    let deadline = Instant::now() + CLI_TIMEOUT;
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
            "`{BINARY} {}` did not terminate within {CLI_TIMEOUT:?}\n--- stderr ---\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    assert!(
        status.success(),
        "`{BINARY} {}` exited with {status}\n--- stderr ---\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Measurement {
        n: erlang_n,
        vmhwm_kib,
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        samples,
    }
}

/// **Test A — peak RSS does not scale with `N`.**
///
/// The same churn loop at a 10× larger `N` must not add more than [`RSS_SLACK_KIB`] to the
/// process's peak resident set, **and** both runs must print exactly their own `N` (the loop's
/// deterministic contract — a fabricated, silently-skipped allocation would show up here).
///
/// A missing system Erlang installation (the boot sequence needs the preloaded modules and
/// the code path) is a **loud skip**.
#[test]
fn peak_rss_does_not_scale_with_the_churn_count() {
    if !Path::new("/proc/self/status").exists() {
        support::skip_loudly(
            "rss_bound",
            "no procfs at /proc (the peak-RSS sampling reads /proc/<pid>/status)",
        );
        return;
    }
    let Some(_oracle) = support::require_oracle("rss_bound") else {
        return;
    };

    let load = loadavg();
    let small = measure_peak_rss(SMALL_N);
    let large = measure_peak_rss(LARGE_N);
    let baseline = measure_peak_rss(0);

    eprintln!("rss_bound: /proc/loadavg at measurement time: {load}");
    for m in [&baseline, &small, &large] {
        if m.samples == 0 {
            eprintln!(
                "rss_bound: N = {:>6} -> VmHWM unmeasured (the child exited before a first \
                 sample); stdout {:?}",
                m.n, m.stdout
            );
            continue;
        }
        eprintln!(
            "rss_bound: N = {:>6} -> VmHWM {:>8} kB ({:>6.2} MiB) from {} sample(s); stdout {:?}",
            m.n, m.vmhwm_kib, m.mib(), m.samples, m.stdout
        );
    }
    eprintln!(
        "rss_bound: 10x N step added {} kB ({:.2} MiB); slack {} kB",
        large.vmhwm_kib as i64 - small.vmhwm_kib as i64,
        large.mib() - small.mib(),
        RSS_SLACK_KIB
    );

    assert!(
        small.samples > 0 && large.samples > 0,
        "both runs must yield at least one /proc sample; small={} samples, large={} samples",
        small.samples,
        large.samples
    );
    // The loop's deterministic contract: run(N) == N, so a run cannot silently do less work
    // (which would make the RSS comparison meaningless).
    assert_eq!(small.stdout, SMALL_N.to_string(), "churn:run/1 stdout");
    assert_eq!(large.stdout, LARGE_N.to_string(), "churn:run/1 stdout");

    // Pre-fix this asserted nothing: the peak grew roughly linearly with N (RESULTS.md §6.7:
    // 29 MiB -> 227 MiB for a 20x step), i.e. ~+190 MiB for this 10x step. After the fix
    // (call_registered copies the registered-BIF result onto the running process's scanned
    // heap, so the collector no longer declines) the peak is flat.
    assert!(
        large.vmhwm_kib <= small.vmhwm_kib + RSS_SLACK_KIB,
        "the 10x larger churn run must not add more than {} kB to the peak RSS \
         (small N={} : {} kB, large N={} : {} kB)",
        RSS_SLACK_KIB,
        SMALL_N,
        small.vmhwm_kib,
        LARGE_N,
        large.vmhwm_kib
    );
    let ratio = large.vmhwm_kib as f64 / small.vmhwm_kib as f64;
    assert!(
        ratio < RSS_MAX_RATIO,
        "peak RSS ratio must stay below {RSS_MAX_RATIO} (small N={SMALL_N} : {} kB, \
         large N={LARGE_N} : {} kB, ratio {ratio:.3})",
        small.vmhwm_kib,
        large.vmhwm_kib
    );
}

/// **Test B — the churn loop really collects** (deterministic; no RSS involved).
///
/// Driven through [`Runtime::run_scheduled`] — a real scheduled process on the full scheduler —
/// `churn:run/1` must return exactly `N` **and** the runtime must report non-zero collections.
/// Before the item-#10b fix the registered-BIF result was an *unscanned root*: the
/// collector's valve declined every process-heap collection for the duration of the loop and
/// `gc_stats().collections` stayed `0`.
#[test]
fn the_churn_loop_really_collects() {
    let Some(_oracle) = support::require_oracle("rss_bound") else {
        return;
    };

    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(FIXTURE).expect("the churn fixture loads");

    let before = rt.gc_stats();
    let value = rt
        .run_scheduled(FIXTURE, "run", 1, &[make_small(COLLECT_N)])
        .expect("churn:run/1 runs to completion on the scheduled path");
    let after = rt.gc_stats();

    assert!(
        is_small(value),
        "churn:run/1 returns a small integer, got a boxed term"
    );
    assert_eq!(
        signed_value(value),
        COLLECT_N,
        "churn:run/1 returns exactly its argument"
    );
    eprintln!(
        "rss_bound: churn:run/{COLLECT_N} collected {} time(s) (before {} -> after {}), \
         fallbacks {}, errors {}, disabled {}",
        after.collections as i64 - before.collections as i64,
        before.collections,
        after.collections,
        after.fallbacks,
        after.errors,
        after.disabled
    );
    assert!(
        after.collections > 0,
        "the churn loop must have driven at least one process-heap collection \
         (item #10b: pre-fix the unscanned-root valve declined them all and this stayed 0); \
         {after:?}"
    );
    assert!(
        after.collections > before.collections,
        "this run itself must have collected; before={before:?} after={after:?}"
    );
    assert_eq!(after.errors, 0, "no collection failed; {after:?}");
    assert!(!after.disabled, "the escape-store valve did not trip; {after:?}");
}
