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
//! The **monotonic** trace time stamps are `erlang:monotonic_time()` readings — the ERTS
//! monotonic epoch, not a runtime-relative zero (contract A.8).
//!
//! A `trace_ts` event's trailing element is the flavour's own stamp: the wall-clock
//! `{MegaSecs, Secs, MicroSecs}` triple for `timestamp`, the native-unit monotonic-time value for
//! `monotonic_timestamp`, and `{Nanoseconds, UniqueMonotonicInteger}` for
//! `strict_monotonic_timestamp`. The two monotonic flavours must report the **same clock**
//! `erlang:monotonic_time/0` reports, i.e. `erlang:system_info(start_time)` plus the elapsed
//! time — a large negative integer that grows toward zero — because a tool that correlates a
//! trace time stamp with a `erlang:monotonic_time()` reading taken elsewhere in the node
//! (`dbg`, `fprof`, an OTP-internal `logger` handler) depends on the shared origin.
//!
//! # What is pinned, and against what
//!
//! The events are produced at the same `RuntimeContext` seam the engine's `send` reaches
//! (`ExecutionContext::send_message`), with the flags `erlang:trace/3` installed — the model
//! `tests/trace_send_event.rs` uses. Three independent anchors are then compared:
//!
//! * the runtime's **own** `erlang:system_info(start_time)`/`end_time` window, which the stamp
//!   must lie inside (they are the ERTS monotonic-time interface constants);
//! * the runtime's **own** `erlang:monotonic_time()` reading, taken right next to the event;
//! * the live system `erl`'s `erlang:monotonic_time()`, from a separate VM on the same host.
//!
//! The stamp and a reading never agree **exactly**: each runtime counts from its own VM start,
//! so the difference is bounded by the two uptimes at the two reads. [`ANCHOR_SLACK_NS`] is that
//! bound, and it only has to be far below the ~5.76e17 ns (18 years) that separates the ERTS
//! epoch from a runtime-relative zero — which is what makes "same epoch" decidable.
//!
//! A missing `erl` is a **loud** skip, never a silent pass ([`support::require_oracle`]).
// The shared harness is compiled in whole, but this suite uses only its toolchain discovery and
// oracle probe, so the unused remainder is expected here.
#[allow(dead_code)]
mod support;
use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_process::{ProcessId, ProcessTable, SignalAtoms};
use beam_rt::{InfoValue, Runtime, RuntimeContext};
use beam_term::prelude::AtomTable;
use beam_term::{is_small, signed_value, Eterm};
/// How far a trace time stamp and a `erlang:monotonic_time()` reading may differ and still be
/// the **same clock**: both count from their own VM's start, so the gap is the two uptimes at the
/// two reads. It is a loose bound (five minutes) because the host may be heavily loaded and the
/// point of the assertion is the **epoch**, not the instant — the divergence this rules out is
/// 5.76e17 ns (18 years).
const ANCHOR_SLACK_NS: i64 = 300 * 1_000_000_000;
/// The default runtime.
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}
/// A **sender** and a distinct **tracer** process in a caller-owned table.
///
/// ERTS discards an event whose tracer IS the tracee
/// (`erts/emulator/nifs/common/erl_tracer_nif.c:143-146`), so the two must differ for any event
/// to be delivered at all. `erts_internal:trace/3` always installs the **calling** process as the
/// tracer, so the tracer half of the pair is the context that calls `erlang:trace/3` and the
/// sender half is the context that sends.
fn two_processes(rt: &mut Runtime) -> (ProcessTable, ProcessId, ProcessId) {
    let _ = rt;
    let mut atoms = AtomTable::new();
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut atoms));
    let sender = table.spawn_default().expect("spawn the sender");
    let tracer = table.spawn_default().expect("spawn the tracer");
    (table, sender, tracer)
}
/// Every message in `pid`'s mailbox, in order, drained.
fn drain(rt: &mut Runtime, table: &mut ProcessTable, pid: ProcessId) -> Vec<Eterm> {
    let mut ctx = RuntimeContext::for_process(rt, table, pid);
    ctx.receive_reset().expect("timeout");
    let mut messages = Vec::new();
    while let Some(term) = ctx.receive_next().expect("receive_next") {
        messages.push(term);
        ctx.receive_remove().expect("remove_message");
        assert!(messages.len() < 16, "the mailbox keeps draining: {messages:?}");
    }
    messages
}
/// The element at `index` of the tuple `term`.
fn element(term: Eterm, index: usize) -> Eterm {
    terms::tuple_element(term, index).expect("the tuple element")
}
/// The `{Item, Value}` integer `system_info/1` answers for `key`.
fn int_info(rt: &mut Runtime, key: &str) -> i64 {
    match rt.system_info_value(key).expect(key) {
        InfoValue::Int(value) => value,
        other => panic!("system_info({key}) is an integer, got {other:?}"),
    }
}
/// The trailing time-stamp element of the `trace_ts` event a traced `send` produces under
/// `flags` (an Erlang flag list such as `"[send, monotonic_timestamp]"`), and the runtime, so a
/// caller can compare it with the runtime's other clocks.
///
/// The stamp is read out of the **delivered** event — a real send through the runtime's own
/// delivery path — not from the stamp builder directly.
fn stamped_send(rt: &mut Runtime, flags: &str) -> Eterm {
    let how_true = rt.intern("true");
    let flags = rt.eval(flags).unwrap_or_else(|error| panic!("{flags}: {error}"));
    let message = rt.intern("hello");
    let mut sink = NullSink;
    let (mut table, sender, tracer) = two_processes(rt);
    let sender_term = sender.to_eterm();
    {
        let mut ctx = RuntimeContext::for_process(rt, &mut table, tracer);
        let count = ctx
            .call_bif("erlang", "trace", 3, &[sender_term, how_true, flags])
            .expect("trace/3");
        assert_eq!(signed_value(count), 1, "the sender became the tracee");
    }
    {
        let mut ctx = RuntimeContext::for_process(rt, &mut table, sender);
        ctx.send_message(sender_term, message).expect("send");
        let _ = &mut sink;
    }
    let messages = drain(rt, &mut table, tracer);
    assert_eq!(messages.len(), 1, "{messages:?}");
    let event = messages[0];
    assert_eq!(
        terms::tuple_arity(event).expect("a tuple"),
        6,
        "a time-stamped event appends one element: {event:?}"
    );
    element(event, 5)
}
/// The integer a term is, requiring a 60-bit small (the shape a realistic uptime produces).
fn small_value(term: Eterm, what: &str) -> i64 {
    assert!(is_small(term), "{what} is a 60-bit small integer, got {term:?}");
    signed_value(term)
}
/// A `monotonic_timestamp` event's stamp is an `erlang:monotonic_time()` reading: inside the
/// runtime's own `start_time`/`end_time` window, negative (the ERTS epoch is negative and the
/// stamp grows toward zero), and within [`ANCHOR_SLACK_NS`] of both this runtime's and the live
/// oracle's `erlang:monotonic_time()`.
#[test]
fn a_monotonic_trace_stamp_is_an_erts_epoch_monotonic_time() {
    let Some(oracle) = support::require_oracle("trace_timestamp") else {
        return;
    };
    let mut rt = runtime();
    let start = int_info(&mut rt, "start_time");
    let end = int_info(&mut rt, "end_time");
    assert_eq!(start, -576_460_752_000_000_000, "the ERTS monotonic epoch");
    assert_eq!(end, 8_646_911_283_000_000_000, "the ERTS monotonic end");

    let stamp = small_value(stamped_send(&mut rt, "[send, monotonic_timestamp]"), "the stamp");
    assert!(
        stamp >= start && stamp <= end,
        "the stamp {stamp} is inside the ERTS window {start}..={end}"
    );
    assert!(
        stamp < 0,
        "an ERTS-epoch stamp is negative (it grows toward zero), got {stamp}"
    );

    // This runtime's own reading is the same clock.
    let reading = small_value(
        rt.eval("erlang:monotonic_time()").expect("monotonic_time/0"),
        "the reading",
    );
    assert!(
        (stamp - reading).abs() <= ANCHOR_SLACK_NS,
        "the stamp {stamp} and this runtime's monotonic_time() {reading} are the same clock"
    );

    // ... and so is the live oracle's, from its own VM.
    let oracle_reading = support::oracle_eval(
        &oracle,
        None,
        "io:format(\"~p~n\",[erlang:monotonic_time()]), halt(0).",
    )
    .unwrap_or_else(|| panic!("the live oracle could not answer erlang:monotonic_time/0"))
    .parse::<i64>()
    .unwrap_or_else(|error| panic!("the oracle's monotonic_time/0 is an integer: {error}"));
    assert!(
        oracle_reading >= start && oracle_reading <= end,
        "the oracle's reading {oracle_reading} is inside the same window {start}..={end}"
    );
    assert!(
        (stamp - oracle_reading).abs() <= ANCHOR_SLACK_NS,
        "the stamp {stamp} and the oracle's monotonic_time() {oracle_reading} are on the same \
         epoch (they differ by at most {ANCHOR_SLACK_NS} ns)"
    );
}
/// A `strict_monotonic_timestamp` event's stamp is `{Nanoseconds, UniqueMonotonicInteger}`: the
/// nanoseconds are the same `erlang:monotonic_time()` clock (and within [`ANCHOR_SLACK_NS`] of the
/// live oracle's own reading), and the unique integer strictly increases from one event to the
/// next.
#[test]
fn the_strict_monotonic_stamp_is_the_same_clock_plus_a_unique_count() {
    let Some(oracle) = support::require_oracle("trace_timestamp") else {
        return;
    };
    let mut rt = runtime();
    let start = int_info(&mut rt, "start_time");
    let end = int_info(&mut rt, "end_time");

    let mut nanos = Vec::new();
    let mut uniques = Vec::new();
    for _ in 0..2 {
        let stamp = stamped_send(&mut rt, "[send, strict_monotonic_timestamp]");
        assert_eq!(
            terms::tuple_arity(stamp).expect("a tuple"),
            2,
            "a strict stamp is {{Nanoseconds, Unique}}: {stamp:?}"
        );
        nanos.push(small_value(element(stamp, 0), "the nanoseconds"));
        uniques.push(small_value(element(stamp, 1), "the unique integer"));
    }
    for (index, value) in nanos.iter().enumerate() {
        assert!(
            *value >= start && *value <= end,
            "stamp {index}: {value} is inside the ERTS window {start}..={end}"
        );
        assert!(*value < 0, "stamp {index}: an ERTS-epoch reading is negative, got {value}");
    }
    assert!(nanos[1] >= nanos[0], "the clock only goes forwards: {nanos:?}");
    assert!(
        uniques[1] > uniques[0],
        "the unique integer strictly increases: {uniques:?}"
    );
    assert!(uniques[0] >= 1, "it is 1-based, as the C's is: {uniques:?}");

    // The nanoseconds ride the same epoch as the live oracle's `erlang:monotonic_time()`.
    let oracle_reading = support::oracle_eval(
        &oracle,
        None,
        "io:format(\"~p~n\",[erlang:monotonic_time()]), halt(0).",
    )
    .unwrap_or_else(|| panic!("the live oracle could not answer erlang:monotonic_time/0"))
    .parse::<i64>()
    .unwrap_or_else(|error| panic!("the oracle's monotonic_time/0 is an integer: {error}"));
    assert!(
        (nanos[0] - oracle_reading).abs() <= ANCHOR_SLACK_NS,
        "the strict stamp's nanoseconds {} and the oracle's monotonic_time() {oracle_reading} are \
         on the same epoch",
        nanos[0]
    );
}