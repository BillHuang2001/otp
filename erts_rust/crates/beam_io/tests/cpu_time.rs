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

//! Integration gate for [`beam_io::cpu_time`] — the `erlang:statistics(runtime)` floor.
//!
//! These cases exercise the **real** syscalls (`getrusage(RUSAGE_SELF)`,
//! `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`), so they assert only **monotone/structural**
//! properties (never exact values): a value that goes backwards would be a real bug, but a
//! value that does not advance in a sub-millisecond burn is not. The `RuntimeClock` cases pin
//! the C's anchor semantics — the first call's `SinceLastCall` equals its `Total` — which is
//! the one behavioural claim the oracle makes observable.

use beam_io::cpu_time::{process_cpu_times, thread_cpu_times, CpuTimes, RuntimeClock};
use std::time::{Duration, Instant};

/// Burn CPU for at least `min_ms` of **wall** time in a tight, optimizer-proof loop.
///
/// Wall time (not CPU time) bounds the loop, so the test always terminates; the loop itself
/// does no I/O and no sleeping, so nearly all of that wall time is CPU time charged to this
/// thread and process.
fn burn_cpu(min_ms: u64) {
    let start = Instant::now();
    let mut acc: u64 = 0x9E37_79B9_7F4A_7C15;
    while start.elapsed() < Duration::from_millis(min_ms) {
        for _ in 0..1_000 {
            acc = acc
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
        }
        std::hint::black_box(acc);
    }
}

/// Assert both components are non-negative millisecond counts.
///
/// `u64` cannot hold a negative value, so the meaningful check is that the value fits a
/// non-negative `i64` — a `time_t`/`timeval` that had gone negative would have wrapped into a
/// huge `u64` (exactly as the C's own unsigned cast would), and this catches that.
fn assert_plausible_ms(times: CpuTimes) {
    assert!(
        i64::try_from(times.user_ms).is_ok(),
        "user_ms is a non-negative millisecond count, got {}",
        times.user_ms
    );
    assert!(
        i64::try_from(times.system_ms).is_ok(),
        "system_ms is a non-negative millisecond count, got {}",
        times.system_ms
    );
}

#[test]
fn process_cpu_times_reports_non_negative_user_and_system_milliseconds() {
    let times = process_cpu_times().expect("getrusage(RUSAGE_SELF) succeeds");
    assert_plausible_ms(times);
    // Reading twice in a row is fine and cannot go backwards within the same process.
    let again = process_cpu_times().expect("getrusage(RUSAGE_SELF) succeeds");
    assert!(again.user_ms >= times.user_ms);
    assert!(again.system_ms >= times.system_ms);
}

#[test]
fn process_cpu_times_user_component_is_monotone_across_a_busy_loop() {
    let before = process_cpu_times().expect("getrusage(RUSAGE_SELF) succeeds");
    burn_cpu(50);
    let after = process_cpu_times().expect("getrusage(RUSAGE_SELF) succeeds");
    assert!(
        after.user_ms >= before.user_ms,
        "user CPU time must not decrease: before={} after={}",
        before.user_ms,
        after.user_ms
    );
    assert!(after.system_ms >= before.system_ms);
}

#[test]
fn thread_cpu_times_reports_combined_time_in_the_user_field_only() {
    let before = thread_cpu_times().expect("clock_gettime(CLOCK_THREAD_CPUTIME_ID) succeeds");
    assert_plausible_ms(before);
    // The thread clock has no user/system split: the system field is always 0 by contract.
    assert_eq!(before.system_ms, 0);

    burn_cpu(50);

    let after = thread_cpu_times().expect("clock_gettime(CLOCK_THREAD_CPUTIME_ID) succeeds");
    assert_eq!(after.system_ms, 0);
    assert!(
        after.user_ms >= before.user_ms,
        "thread CPU time must not decrease: before={} after={}",
        before.user_ms,
        after.user_ms
    );
    // A 50 ms burn outlasts any real clock granularity, so the thread really did accrue time.
    assert!(
        after.user_ms > before.user_ms,
        "a 50 ms CPU burn must advance the thread clock: before={} after={}",
        before.user_ms,
        after.user_ms
    );
}

#[test]
fn runtime_clock_first_call_reports_the_total_as_since_last_call() {
    let mut clock = RuntimeClock::new();
    let (total, since_last) = clock.runtime().expect("the runtime counter is readable");
    // The C anchors `runtime_prev` at 0, so the first reading's difference *is* the total.
    assert_eq!(since_last, total);
}

#[test]
fn runtime_clock_second_call_diffs_against_the_first_and_increases_the_total() {
    let mut clock = RuntimeClock::new();
    let (total, _) = clock.runtime().expect("the runtime counter is readable");

    burn_cpu(50);

    let (total2, delta) = clock.runtime().expect("the runtime counter is readable");
    assert!(
        total2 >= total,
        "the total must not decrease: first={total} second={total2}"
    );
    assert_eq!(
        delta,
        total2 - total,
        "SinceLastCall is the difference against the previous total"
    );
    // A 50 ms burn outlasts any real clock granularity.
    assert!(
        delta > 0,
        "a 50 ms CPU burn must show up in SinceLastCall, got {delta}"
    );
}

#[test]
fn runtime_clock_deltas_telescope_to_the_latest_total() {
    let mut clock = RuntimeClock::new();
    let (first_total, first_delta) = clock.runtime().expect("the runtime counter is readable");
    // The anchor starts at 0, so the first difference *is* the first total …
    assert_eq!(first_delta, first_total);
    let mut sum = first_total;
    let mut last = first_total;

    // … and every later difference is the increment, so the running sum equals the latest
    // total (this holds without pinning any absolute value, so it cannot be flaky).
    for _ in 0..20 {
        burn_cpu(2);
        let (total, delta) = clock.runtime().expect("the runtime counter is readable");
        assert!(
            total >= last,
            "the total must not decrease: previous={last} current={total}"
        );
        assert_eq!(delta, total - last);
        sum += delta;
        last = total;
    }
    assert_eq!(sum, last);
}

#[test]
fn runtime_clock_reset_re_anchors_so_the_next_pair_is_equal() {
    let mut clock = RuntimeClock::new();
    let (first, _) = clock.runtime().expect("the runtime counter is readable");
    burn_cpu(20);
    let (second, _) = clock.runtime().expect("the runtime counter is readable");
    assert!(second >= first);

    clock.reset();
    let (total, since_last) = clock.runtime().expect("the runtime counter is readable");
    // After re-anchoring to 0 the pair is equal again (the first-call semantics).
    assert_eq!(since_last, total);
    assert!(total >= second);
}

#[test]
fn runtime_clock_default_is_the_same_as_new() {
    let mut clock = RuntimeClock::default();
    let (total, since_last) = clock.runtime().expect("the runtime counter is readable");
    assert_eq!(since_last, total);

    // And it behaves like a fresh `new()` on the second call too.
    let (total2, delta) = clock.runtime().expect("the runtime counter is readable");
    assert!(total2 >= total);
    assert_eq!(delta, total2 - total);
}
