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

//! Process tracing (contract A.8, item 1) end to end through the runtime.
//!
//! The trace-flag state, `erlang:trace_info/2`, `erlang:trace_delivered/1` and
//! `erts_internal:trace/3` (the body `erlang:trace/2,3` calls) are pinned here at the BIF
//! boundary — the same dispatch (`RuntimeContext::call_bif`) the `erlang` wrappers and compiled
//! code reach. Emitting `{trace, …}` events and `trace_pattern` are deliberately out of scope, so
//! nothing here waits for a trace event.
use beam_bifs::NullSink;
use beam_common::BeamError;
use beam_engine::{terms, ExecutionContext};
use beam_process::ProcessId;
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{signed_value, Eterm, NIL};

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// The element at `index` of the `{Item, Value}` pair `pair`.
fn pair_value(pair: Eterm, index: usize) -> Eterm {
    terms::tuple_element(pair, index).expect("the pair element")
}

/// The elements of a proper list term.
fn elems(list: Eterm) -> Vec<Eterm> {
    terms::list_elements(list).expect("a proper list")
}

/// `erlang:trace/3` returns `1` for a live pid, and every `trace_info/2` item reports what was
/// set — `flags`, `tracer`, `match_spec`, `traced` and the OTP-30 `all` list (in that order).
#[test]
fn trace_sets_flags_and_trace_info_reports_every_item() {
    let mut rt = runtime();
    let the_pid = rt.self_pid().to_eterm();
    // The expected atom terms, interned before the context borrows the runtime (the shared atom
    // table never renumbers, so these are exactly the atoms the BIF floor builds).
    let how_true = rt.intern("true");
    let call = rt.intern("call");
    let item_flags = rt.intern("flags");
    let item_tracer = rt.intern("tracer");
    let item_match_spec = rt.intern("match_spec");
    let item_traced = rt.intern("traced");
    let item_all = rt.intern("all");
    let flags_list = rt.eval("[call]").expect("[call]");

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);

    // `erlang:trace(self(), true, [call])` -> 1.
    let count = ctx
        .call_bif("erlang", "trace", 3, &[the_pid, how_true, flags_list])
        .expect("trace/3");
    assert_eq!(signed_value(count), 1, "one tracee was affected");

    // `flags` -> `{flags, [call]}`.
    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_flags])
        .expect("trace_info(flags)");
    assert_eq!(pair_value(info, 0), item_flags);
    assert_eq!(elems(pair_value(info, 1)), vec![call]);

    // `tracer` -> `{tracer, self()}`.
    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_tracer])
        .expect("trace_info(tracer)");
    assert_eq!(pair_value(info, 0), item_tracer);
    assert_eq!(pair_value(info, 1), the_pid);

    // `match_spec` -> `{match_spec, []}` (match specs are not implemented yet).
    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_match_spec])
        .expect("trace_info(match_spec)");
    assert_eq!(pair_value(info, 0), item_match_spec);
    assert_eq!(pair_value(info, 1), NIL);

    // `traced` -> `{traced, true}`.
    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_traced])
        .expect("trace_info(traced)");
    assert_eq!(pair_value(info, 0), item_traced);
    assert_eq!(pair_value(info, 1), how_true);

    // `all` -> the four pairs, in `flags, tracer, match_spec, traced` order.
    let all = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_all])
        .expect("trace_info(all)");
    let pairs = elems(all);
    assert_eq!(pairs.len(), 4, "all four items, in order");
    assert_eq!(pair_value(pairs[0], 0), item_flags);
    assert_eq!(elems(pair_value(pairs[0], 1)), vec![call]);
    assert_eq!(pair_value(pairs[1], 0), item_tracer);
    assert_eq!(pair_value(pairs[1], 1), the_pid);
    assert_eq!(pair_value(pairs[2], 0), item_match_spec);
    assert_eq!(pair_value(pairs[2], 1), NIL);
    assert_eq!(pair_value(pairs[3], 0), item_traced);
    assert_eq!(pair_value(pairs[3], 1), how_true);
}

/// An alive but untraced pid reports `{flags, []}`, `{tracer, []}` and `{traced, false}`.
#[test]
fn an_untraced_pid_reports_empty_flags() {
    let mut rt = runtime();
    let the_pid = rt.self_pid().to_eterm();
    let item_flags = rt.intern("flags");
    let item_tracer = rt.intern("tracer");
    let item_traced = rt.intern("traced");
    let how_false = rt.intern("false");

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);

    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_flags])
        .expect("trace_info(flags)");
    assert_eq!(elems(pair_value(info, 1)), Vec::new());

    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_tracer])
        .expect("trace_info(tracer)");
    assert_eq!(pair_value(info, 1), NIL);

    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_traced])
        .expect("trace_info(traced)");
    assert_eq!(pair_value(info, 1), how_false);
}

/// `erlang:trace(P, false)` clears the tracee, and `trace_info` reports it untraced again.
#[test]
fn trace_false_clears_the_tracee() {
    let mut rt = runtime();
    let the_pid = rt.self_pid().to_eterm();
    let how_true = rt.intern("true");
    let how_false = rt.intern("false");
    let item_flags = rt.intern("flags");
    let item_traced = rt.intern("traced");
    let flags_list = rt.eval("[call]").expect("[call]");

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);

    let count = ctx
        .call_bif("erlang", "trace", 3, &[the_pid, how_true, flags_list])
        .expect("trace/3");
    assert_eq!(signed_value(count), 1);

    // `erlang:trace(self(), false)` (the /2 wrapper) clears it and still counts the tracee.
    let count = ctx
        .call_bif("erlang", "trace", 2, &[the_pid, how_false])
        .expect("trace/2");
    assert_eq!(signed_value(count), 1, "the tracee was still affected");

    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_flags])
        .expect("trace_info(flags)");
    assert_eq!(elems(pair_value(info, 1)), Vec::new(), "flags cleared");
    let info = ctx
        .call_bif("erlang", "trace_info", 2, &[the_pid, item_traced])
        .expect("trace_info(traced)");
    assert_eq!(pair_value(info, 1), how_false, "no longer traced");
}

/// `erlang:trace_delivered/1` mints a fresh reference, delivers
/// `{trace_delivered, Tracee, Ref}` to the calling process and returns `Ref`.
#[test]
fn trace_delivered_returns_a_ref_and_delivers_the_barrier_message() {
    let mut rt = runtime();
    let the_pid = rt.self_pid().to_eterm();
    let tag = rt.intern("trace_delivered");
    let all = rt.intern("all");

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);

    let reference = ctx
        .call_bif("erlang", "trace_delivered", 1, &[the_pid])
        .expect("trace_delivered/1");

    // The barrier message is delivered to the caller synchronously.
    let message = ctx
        .receive_next()
        .expect("receive_next")
        .expect("the barrier message was delivered");
    assert_eq!(terms::tuple_arity(message).expect("a tuple"), 3);
    assert_eq!(pair_value(message, 0), tag);
    assert_eq!(pair_value(message, 1), the_pid);
    let delivered = pair_value(message, 2);

    // `all` is a valid Tracee and mints a distinct reference.
    let via_all = ctx
        .call_bif("erlang", "trace_delivered", 1, &[all])
        .expect("trace_delivered(all)");

    // The delivered reference is structurally the returned one (the message holds a copy on the
    // receiver's heap) and a later delivery is a fresh reference.
    drop(ctx);
    let atoms = rt.code_server().atoms();
    assert!(
        beam_term::compare::eq(delivered, reference, atoms),
        "the delivered ref is the returned ref"
    );
    assert!(
        !beam_term::compare::eq(via_all, reference, atoms),
        "each delivery is a fresh reference"
    );
}

/// `erts_internal:trace/3` targets: a pid, and the group atoms `all`/`existing`/
/// `existing_processes`/`processes` select the whole process table; the port/`new` PidSpecs are
/// the honest typed capability gap.
#[test]
fn erts_trace_targets_and_capability_gaps() {
    let mut rt = runtime();
    let how_true = rt.intern("true");
    let flags_list = rt.eval("[call]").expect("[call]");
    let all = rt.intern("all");
    let existing = rt.intern("existing");
    let existing_processes = rt.intern("existing_processes");
    let processes = rt.intern("processes");
    let new = rt.intern("new");
    let ports = rt.intern("ports");

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);

    // Every group atom selects the runtime's own one process.
    for target in [all, existing, existing_processes, processes] {
        let count = ctx
            .call_bif("erts_internal", "trace", 3, &[target, how_true, flags_list])
            .expect("erts_internal:trace/3");
        assert_eq!(signed_value(count), 1, "one process in the table");
    }

    // A port/new PidSpec is the honest capability gap, never a fabricated count.
    for target in [new, ports] {
        let error = ctx
            .call_bif("erts_internal", "trace", 3, &[target, how_true, flags_list])
            .expect_err("a port/new PidSpec is unsupported");
        assert!(
            beam_rt::error::unsupported_reason(&error).is_some(),
            "a typed capability gap, got {error:?}"
        );
    }

    // A pid the table does not hold selects nothing (the oracle's `0`).
    let ghost = ProcessId::new(9_999_999, 0).to_eterm();
    let count = ctx
        .call_bif("erts_internal", "trace", 3, &[ghost, how_true, flags_list])
        .expect("erts_internal:trace/3");
    assert_eq!(signed_value(count), 0);
}

/// The badarg paths: an unknown item or a non-pid first argument to `trace_info/2`, an unknown
/// flag / bad `How` / bad `PidSpec` to `erts_internal:trace/3`, and a bad `Tracee` to
/// `trace_delivered/1`. A dead/unknown pid is `undefined` for `trace_info/2`.
#[test]
fn the_badarg_paths() {
    let mut rt = runtime();
    let the_pid = rt.self_pid().to_eterm();
    let how_true = rt.intern("true");
    let flags_item = rt.intern("flags");
    let undefined = rt.intern("undefined");
    let unknown_item = rt.intern("not_a_trace_item");
    let unknown_flag = rt.eval("[not_a_trace_flag]").expect("[not_a_trace_flag]");
    let unknown_atom = rt.intern("not_a_pid_spec");
    let empty = NIL;
    let ghost = ProcessId::new(9_999_999, 0).to_eterm();

    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);

    // `trace_info/2`: an unknown item atom, and a first argument that is not a pid.
    for args in [[the_pid, unknown_item], [flags_item, flags_item]] {
        assert!(
            matches!(
                ctx.call_bif("erlang", "trace_info", 2, &args),
                Err(BeamError::BadArg)
            ),
            "trace_info is badarg for {args:?}"
        );
    }

    // `trace_info/2` for a pid the table does not hold is `undefined`.
    assert_eq!(
        ctx.call_bif("erlang", "trace_info", 2, &[ghost, flags_item])
            .expect("trace_info of a dead pid"),
        undefined
    );

    // `erts_internal:trace/3`: an unknown flag, a bad `How`, and a bad `PidSpec`.
    for args in [
        [the_pid, how_true, unknown_flag],
        [the_pid, unknown_atom, empty],
        [unknown_atom, how_true, empty],
    ] {
        assert!(
            matches!(
                ctx.call_bif("erts_internal", "trace", 3, &args),
                Err(BeamError::BadArg)
            ),
            "erts_internal:trace/3 is badarg for {args:?}"
        );
    }

    // `trace_delivered/1`: a non-atom target and an unknown pid are both badarg.
    for target in [unknown_atom, ghost] {
        assert!(
            matches!(
                ctx.call_bif("erlang", "trace_delivered", 1, &[target]),
                Err(BeamError::BadArg)
            ),
            "trace_delivered is badarg for {target:?}"
        );
    }
}
