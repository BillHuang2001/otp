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

//! The `{trace, …}` **send** event (contract A.8, item 1) end to end through the runtime.
//!
//! A traced process's `send` produces an event *message* for its tracer: the runtime's send
//! seam delivers the payload and then hands `{trace, Sender, send, Msg, To}` — or, with one of
//! the three time-stamp flags set, `{trace_ts, …}` with the flavour's trailing element — to the
//! tracer's mailbox. The events are pinned here at the same `RuntimeContext` seam the engine
//! calls, so the case is a real send through the runtime's own delivery path
//! (`ExecutionContext::send_message`) with the flags `erlang:trace/3` installed.
//!
//! The sequential-trace `{seq_trace, Label, {send, …}}` event is pinned where its tracer can be
//! configured (`src/trace.rs`'s unit tests): `erlang:system_flag(sequential_tracer, _)` is not
//! implemented, so no system tracer can be set from outside the crate. What is observable here —
//! that a `send`-flagged token does not disturb the mailbox and that the send stamps the token's
//! serial — is pinned below.
use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_process::{ProcessId, ProcessTable, SignalAtoms};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::prelude::AtomTable;
use beam_term::{is_small, signed_value, Eterm};

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// A **sender** and a distinct **tracer** process in a caller-owned table.
///
/// ERTS discards an event whose tracer IS the tracee
/// (`erts/emulator/nifs/common/erl_tracer_nif.c:143-146`), so the two must differ for any event
/// to be delivered at all. `erts_internal:trace/3` always installs the **calling** process as
/// the tracer, so the tracer half of the pair is the context that calls `erlang:trace/3` and the
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
///
/// The scan position is reset first and the loop removes each message it reads, so the order is
/// the queue's own regardless of where an earlier `loop_rec_end` left the scan. `pid` runs
/// against the caller-owned table `RuntimeContext::for_process` is built for.
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

/// Every message in the mailbox of a context that runs against the runtime's **own** table.
fn drain_self(ctx: &mut RuntimeContext<'_>) -> Vec<Eterm> {
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

/// A `[send]`-traced process's `send` delivers the payload and then `{trace, Sender, send, Msg,
/// To}` to its tracer — `Sender` the sending process, `To` the destination the send carried and
/// `Msg` the message.
#[test]
fn a_send_traced_sender_emits_the_trace_send_event() {
    let mut rt = runtime();
    let how_true = rt.intern("true");
    let send_flags = rt.eval("[send]").expect("[send]");
    let message = rt.intern("hello");
    let trace = rt.intern("trace");
    let send = rt.intern("send");

    let mut sink = NullSink;
    let (mut table, sender, tracer) = two_processes(&mut rt);
    let sender_term = sender.to_eterm();

    // The **tracer** traces the sender: `erlang:trace(Sender, true, [send])` from the tracer's
    // own context installs the tracer as the tracee's reporter.
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, tracer);
        let count = ctx
            .call_bif("erlang", "trace", 3, &[sender_term, how_true, send_flags])
            .expect("trace/3");
        assert_eq!(signed_value(count), 1, "the sender became the tracee");
    }
    // The sender sends to itself: the payload lands in its mailbox, the event goes to the tracer.
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, sender);
        ctx.send_message(sender_term, message).expect("send");
        let _ = &mut sink;
    }

    // The sender's mailbox holds the payload alone.
    assert_eq!(drain(&mut rt, &mut table, sender), vec![message]);

    // The tracer's mailbox holds the event: `{trace, Sender, send, Msg, To}`.
    let messages = drain(&mut rt, &mut table, tracer);
    assert_eq!(messages.len(), 1, "the event: {messages:?}");
    let event = messages[0];
    assert_eq!(terms::tuple_arity(event).expect("a tuple"), 5);
    assert_eq!(element(event, 0), trace);
    assert_eq!(element(event, 1), sender_term, "the *sending* process");
    assert_eq!(element(event, 2), send);
    assert_eq!(element(event, 3), message);
    assert_eq!(element(event, 4), sender_term, "the destination, as the send carried it");
}

/// The `timestamp` flag makes the tag `trace_ts` and appends `{MegaSecs, Secs, MicroSecs}`.
#[test]
fn the_timestamp_flag_yields_a_trace_ts_with_a_trailing_triple() {
    let mut rt = runtime();
    let how_true = rt.intern("true");
    let flags = rt.eval("[send, timestamp]").expect("[send, timestamp]");
    let message = rt.intern("hello");
    let trace_ts = rt.intern("trace_ts");
    let send = rt.intern("send");

    let mut sink = NullSink;
    let (mut table, sender, tracer) = two_processes(&mut rt);
    let sender_term = sender.to_eterm();

    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, tracer);
        let count = ctx
            .call_bif("erlang", "trace", 3, &[sender_term, how_true, flags])
            .expect("trace/3");
        assert_eq!(signed_value(count), 1);
    }
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, sender);
        ctx.send_message(sender_term, message).expect("send");
        let _ = &mut sink;
    }

    let messages = drain(&mut rt, &mut table, tracer);
    assert_eq!(messages.len(), 1, "{messages:?}");
    let event = messages[0];
    assert_eq!(terms::tuple_arity(event).expect("a tuple"), 6, "the stamp is appended");
    assert_eq!(element(event, 0), trace_ts, "a time-stamped event is a trace_ts");
    assert_eq!(element(event, 1), sender_term);
    assert_eq!(element(event, 2), send);
    assert_eq!(element(event, 3), message);
    assert_eq!(element(event, 4), sender_term);

    // The trailing element is the wall-clock `erlang:timestamp/0` triple.
    let stamp = element(event, 5);
    assert_eq!(terms::tuple_arity(stamp).expect("a tuple"), 3);
    let mega = element(stamp, 0);
    let secs = element(stamp, 1);
    let micro = element(stamp, 2);
    for value in [mega, secs, micro] {
        assert!(is_small(value), "a small integer, got {value:?}");
    }
    assert!(signed_value(mega) > 0, "a wall clock after the epoch");
    assert!(signed_value(secs) < 1_000_000, "the split's remainder");
    assert!(signed_value(micro) < 1_000_000, "microseconds");
}

/// A process that traces **itself** (`erlang:trace(self(), true, [send])`, whose default tracer
/// is the calling process) observes **no** event of its own: ERTS discards an event whose tracer
/// IS the tracee (`erl_tracer_nif.c:143-146`). Oracle probe (OTP 29.1.1 / erts-17.1):
/// `erlang:trace(self(), true, [send]), self() ! hello` leaves only `hello` in the mailbox.
#[test]
fn a_self_traced_process_observes_no_event_of_its_own() {
    let mut rt = runtime();
    let pid = rt.self_pid().to_eterm();
    let how_true = rt.intern("true");
    let send_flags = rt.eval("[send]").expect("[send]");
    let message = rt.intern("hello");

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);

    let count = ctx
        .call_bif("erlang", "trace", 3, &[pid, how_true, send_flags])
        .expect("trace/3");
    assert_eq!(signed_value(count), 1, "the calling process became the tracee");

    ctx.send_message(pid, message).expect("send");
    assert_eq!(
        {
            ctx.receive_reset().expect("timeout");
            let mut messages = Vec::new();
            while let Some(term) = ctx.receive_next().expect("receive_next") {
                messages.push(term);
                ctx.receive_remove().expect("remove_message");
                assert!(messages.len() < 8, "the mailbox keeps draining: {messages:?}");
            }
            messages
        },
        vec![message],
        "no `{{trace, …}}` event is delivered to the tracee itself"
    );
}

/// An **untraced** sender emits nothing, and neither does one traced with a flag set that does
/// not include `send`: the mailbox holds the payload and nothing else.
#[test]
fn an_untraced_sender_emits_nothing() {
    let mut rt = runtime();
    let pid = rt.self_pid().to_eterm();
    let how_true = rt.intern("true");
    let call_flags = rt.eval("[call]").expect("[call]");
    let message = rt.intern("hello");

    // No trace flags at all.
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
    ctx.send_message(pid, message).expect("send");
    assert_eq!(drain_self(&mut ctx), vec![message], "only the payload");

    // `[call]` — a real tracee, but the `send` bit is not set (the flag lives on the *sending*
    // process, and here it is set without `send`).
    let count = ctx
        .call_bif("erlang", "trace", 3, &[pid, how_true, call_flags])
        .expect("trace/3");
    assert_eq!(signed_value(count), 1);
    ctx.send_message(pid, message).expect("send");
    assert_eq!(drain_self(&mut ctx), vec![message], "no event for a `[call]` tracee");
}

/// A `send`-flagged sequential-trace token on the sender: the send **stamps** the token's serial
/// (`seq_trace.erl:88-90`) but delivers no message while no system tracer is configured —
/// `erlang:system_flag(sequential_tracer, _)` is the capability gap, so the event is built and
/// dropped exactly as `seq_trace:print/1`'s is.
#[test]
fn a_seq_trace_send_token_stamps_the_serial_and_emits_no_message_without_a_tracer() {
    let mut rt = runtime();
    let pid = rt.self_pid().to_eterm();
    let how_true = rt.intern("true");
    let how_false = rt.intern("false");
    let send_component = rt.intern("send");
    let label_component = rt.intern("label");
    let serial_component = rt.intern("serial");
    let label = rt.intern("my-label");
    let message = rt.intern("hello");

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);

    // `erlang:seq_trace(send, true)` -> the previous flag value, `false`.
    let previous = ctx
        .call_bif("erlang", "seq_trace", 2, &[send_component, how_true])
        .expect("seq_trace(send, true)");
    assert_eq!(previous, how_false);
    let previous = ctx
        .call_bif("erlang", "seq_trace", 2, &[label_component, label])
        .expect("seq_trace(label, L)");
    assert_eq!(signed_value(previous), 0, "the fresh token's label is the integer 0");

    ctx.send_message(pid, message).expect("send");
    assert_eq!(
        drain_self(&mut ctx),
        vec![message],
        "no system tracer is configured, so no seq_trace event is delivered"
    );

    // The send stamped the token: `serial` is the documented `{Previous, Current}` pair, with the
    // clock bumped by the one send.
    let info = ctx
        .call_bif("erlang", "seq_trace_info", 1, &[serial_component])
        .expect("seq_trace_info(serial)");
    let pair = element(info, 1);
    assert_eq!(terms::tuple_arity(pair).expect("a tuple"), 2);
    assert_eq!(signed_value(element(pair, 0)), 0, "the previous counter");
    assert_eq!(signed_value(element(pair, 1)), 1, "the counter this send attached");
}
