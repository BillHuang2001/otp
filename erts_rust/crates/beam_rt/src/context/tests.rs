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

use beam_bifs::{AtomInterner, BifRegistry, BytesSink, LocalFunIdentity, LocalNode};
use beam_engine::RunOutcome;
use beam_term::{atom_val, is_small, make_small, signed_value, Atom, AtomTable, NIL};

use super::*;
use crate::config::RuntimeConfig;

fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("the default runtime")
}

#[test]
fn dispatches_a_registered_erlang_bif() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    let sum = ctx
        .call_bif("erlang", "+", 2, &[make_small(2), make_small(3)])
        .expect("2 + 3");
    assert!(is_small(sum));
    assert_eq!(signed_value(sum), 5);
    assert_eq!(
        signed_value(
            ctx.call_bif("erlang", "*", 2, &[make_small(6), make_small(7)])
                .expect("6 * 7")
        ),
        42
    );
}

#[test]
fn a_bif_builds_an_atom_from_the_shared_table() {
    let mut rt = runtime();
    let a = rt.intern("a");
    let mut ctx = RuntimeContext::new(&mut rt);
    let is_atom = ctx
        .call_bif("erlang", "is_atom", 1, &[a])
        .expect("is_atom(a)");
    // `ctx` now owns a shared-sink handle (and therefore a destructor), so the runtime
    // is reached through the context that borrows it rather than the outer binding.
    assert_eq!(
        ctx.runtime.atoms().name(atom_val(is_atom)),
        Some("true"),
        "the atom the BIF built resolves through the shared table"
    );
}

#[test]
fn the_pending_interner_predicts_append_only_indices_and_dedups() {
    let mut interner = PendingInterner::new(10);
    assert_eq!(interner.intern("foo").raw(), 10);
    assert_eq!(interner.intern("bar").raw(), 11);
    // A duplicate within the same call reuses its predicted index.
    assert_eq!(interner.intern("foo").raw(), 10);
    assert_eq!(interner.intern("baz").raw(), 12);

    // Committing lands each recorded name at exactly the index it predicted. The table
    // must already hold `base` names (`AtomTable` assigns indices in insertion order).
    let mut atoms = AtomTable::new();
    for i in 0..10 {
        atoms.intern(&format!("pad{i}"));
    }
    assert_eq!(atoms.len(), 10);
    interner.commit(&mut atoms);
    assert_eq!(atoms.name(Atom::from_raw(10)), Some("foo"));
    assert_eq!(atoms.name(Atom::from_raw(11)), Some("bar"));
    assert_eq!(atoms.name(Atom::from_raw(12)), Some("baz"));
    assert_eq!(atoms.len(), 13);
}

#[test]
fn the_pending_interner_reports_emptiness_so_the_commit_can_be_skipped() {
    // `is_empty` is the guard `call_registered`/`try_call_nif` use to skip the
    // `Rc::make_mut` commit (`atoms_mut`) on a call that interned nothing -- otherwise the
    // whole shared `AtomTable` is deep-cloned on every registered BIF call. It must be
    // true before any name is recorded and false once one is.
    let mut interner = PendingInterner::new(0);
    assert!(interner.is_empty(), "a fresh interner has recorded nothing");
    interner.intern("first");
    assert!(!interner.is_empty(), "a recorded name is not empty");
    let mut atoms = AtomTable::new();
    interner.commit(&mut atoms);
    assert!(
        interner.is_empty(),
        "commit drains the recorded names, so the interner is empty again"
    );
}

#[test]
fn a_bif_interns_a_new_atom_into_the_shared_table() {
    let mut rt = runtime();
    let name = "a_fresh_atom_zzz";
    assert!(rt.atoms().lookup(name).is_none(), "the name starts unknown");
    let base = rt.atoms().len();
    let mut ctx = RuntimeContext::new(&mut rt);
    let image = terms::make_heap_binary(&mut ctx, name.as_bytes()).expect("a binary");
    let atom = ctx
        .call_bif("erlang", "binary_to_atom", 1, &[image])
        .expect("binary_to_atom/1 interns a new name");
    // The atom the BIF built resolves through the one shared table: the name was
    // committed after the call, at exactly the index the interner predicted.
    // (`ctx` holds a shared-sink handle with a destructor, so the runtime is reached
    // through the context that borrows it.)
    assert_eq!(ctx.runtime.atoms().name(atom_val(atom)), Some(name));
    assert_eq!(ctx.runtime.atoms().len(), base + 1);
}

#[test]
fn an_unknown_erlang_bif_raises_undef_not_a_panic() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    // `erlang` is the BIF namespace, never a loadable module: a name the release does
    // not have raises the oracle's bare `error:undef`, never a fabricated value.
    let error = ctx
        .call_bif("erlang", "nope", 0, &[])
        .expect_err("not a BIF");
    assert_eq!(error.as_atom(), Some("undef"), "{error}");
    assert!(ctx
        .call_bif("erlang", "+", 2, &[make_small(1)])
        .is_err(), "wrong arity");
}

#[test]
fn io_format_writes_to_the_installed_sink() {
    let mut rt = runtime();
    let format: Vec<Eterm> = "hi ~w\n".bytes().map(|b| make_small(i64::from(b))).collect();
    let mut sink = BytesSink::new();
    let result = {
        let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
        let format = terms::make_list(&mut ctx, &format).expect("a format list");
        let args = terms::make_list(&mut ctx, &[make_small(7)]).expect("an argument list");
        ctx.call_bif("io", "format", 2, &[format, args])
            .expect("io:format/2")
    };
    assert_eq!(rt.atoms().name(atom_val(result)), Some("ok"));
    assert_eq!(sink.as_str(), "hi 7\n");
}

#[test]
fn ets_inside_an_applied_fun_reaches_the_outer_table() {
    let mut rt = runtime();
    // `lists:map/2` applies its fun argument through `FunBridge`; the `ets:*` calls
    // inside that fun must reach the runtime's shared tables. Before `call_registered`
    // stopped holding the ETS cell for a re-entrancy-capable call, the nested
    // `ets:insert` raised `unsupported_bif` (the cell was already borrowed by the
    // outer `lists:map/2` call), never a wrong result — but never a right one either.
    let term = rt
        .eval(
            "begin T = ets:new(t, [set]), \
             lists:map(fun(X) -> ets:insert(T, {X, X*X}) end, [1,2,3]), \
             ets:lookup(T, 2) end",
        )
        .expect("ets:* inside a fun applied by lists:map/2");
    let printed = rt.format_term(term, None).expect("~p renders the lookup");
    assert_eq!(printed.trim_end(), "[{2,4}]");
}

#[test]
fn a_nested_io_format_reaches_the_callers_sink() {
    let mut rt = runtime();
    let mut sink = BytesSink::new();
    {
        let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
        // The `io:format/2` runs inside the fun `lists:map/2` applies; it must write to
        // the caller's sink (shared through `SharedSink`), not to stdout.
        crate::eval::eval(
            &mut ctx,
            "lists:map(fun(X) -> io:format(\"~w \", [X]) end, [1,2,3])",
        )
        .expect("io:format inside a fun applied by lists:map/2");
    }
    assert_eq!(sink.as_str(), "1 2 3 ");
}

#[test]
fn a_nested_call_without_a_fun_still_sees_the_outer_tables() {
    let mut rt = runtime();
    // The mirror case: a *non*-re-entrancy-capable BIF (`ets:new/2`, `ets:insert/2` here)
    // is given the shared tables, and a fun it does not apply cannot observe them.
    let term = rt
        .eval("begin T = ets:new(t, [set]), ets:insert(T, {1, 11}), ets:lookup(T, 1) end")
        .expect("a plain ets:* sequence");
    let printed = rt.format_term(term, None).expect("~p renders the lookup");
    assert_eq!(printed.trim_end(), "[{1,11}]");
}

#[test]
fn the_arena_is_unbounded_in_milestone_one() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    assert_eq!(ctx.heap_available(), usize::MAX);
    let mut x = [make_small(0)];
    let mut stack = [make_small(0)];
    assert!(ctx.ensure_heap(u32::MAX, 1, &mut x, &mut stack).is_ok());
    assert!(!ctx.alloc_words(4).expect("arena allocates").is_null());
}

#[test]
fn the_registry_is_the_core_bif_set() {
    let rt = runtime();
    let core = BifRegistry::with_core_bifs();
    for (module, function, arity) in [
        ("erlang", "+", 2),
        ("lists", "reverse", 1),
        ("io", "format", 2),
    ] {
        assert!(rt.registry.contains(module, function, arity));
        // Value-for-value agreement with a freshly built core registry proves this
        // is the real `beam_bifs` one, not a hand-rolled subset.
        assert_eq!(
            rt.registry.lookup(module, function, arity).is_some(),
            core.lookup(module, function, arity).is_some()
        );
    }
    assert!(!rt.registry.contains("erlang", "nope", 0));
    assert!(!core.contains("erlang", "nope", 0));
}

// -----------------------------------------------------------------------
// the receive/mailbox seam over the running process's real mailbox
// -----------------------------------------------------------------------

/// The committed mailbox fixture (regeneration recipe in `tests/fixtures/README.md`).
const MAILBOX_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/mailbox.beam"
);

/// The fixture loaded as an owned image, so it outlives the borrowing context
/// (the receive-seam cases need `Machine::run_export_outcome`, which borrows both).
fn mailbox_fixture() -> Module {
    beam_loader::load_file(MAILBOX_BEAM).expect("load mailbox.beam")
}

/// The fixture's pid term (the destination `self()` reports).
fn self_term(rt: &Runtime) -> Eterm {
    rt.self_pid().to_eterm()
}

#[test]
fn send_message_resolves_the_target_and_rejects_a_non_pid() {
    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut ctx = RuntimeContext::new(&mut rt);

    // A self-delivery is the `Enqueued` case (nothing is blocked).
    ctx.send_message(pid, make_small(7)).expect("deliver to self");

    // A non-pid destination is the typed `BadArg` — never a wrong delivery.
    assert!(matches!(
        ctx.send_message(make_small(7), make_small(7)),
        Err(BeamError::BadArg)
    ));

    // The message is in the running process's mailbox.
    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox.peek().expect("the delivered message").payload(), make_small(7));
}

#[test]
fn send_message_to_an_unknown_pid_is_the_tables_typed_error() {
    let mut rt = runtime();
    let mut ctx = RuntimeContext::new(&mut rt);
    // A pid whose (number, serial) names no live process: the table's typed not-found.
    let ghost = ProcessId::new(9999, 7).to_eterm();
    let error = ctx
        .send_message(ghost, make_small(1))
        .expect_err("no such process");
    assert!(
        matches!(error, BeamError::TermNotFound),
        "expected the table's typed not-found error, got {error:?}"
    );
}

#[test]
fn receive_next_peeks_and_the_scan_operations_position_the_mailbox() {
    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut ctx = RuntimeContext::new(&mut rt);

    // An empty mailbox is `Ok(None)` (not an error) — that is what makes the
    // compiled code jump to the `wait` that blocks.
    assert_eq!(ctx.receive_next().expect("empty mailbox"), None);

    ctx.send_message(pid, make_small(1)).expect("first");
    ctx.send_message(pid, make_small(2)).expect("second");

    // `loop_rec` peeks: repeated calls yield the same candidate.
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));

    // `loop_rec_end` advances past it.
    ctx.receive_skip().unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(2)));

    // `remove_message` unlinks the candidate and repositions at the first message.
    ctx.receive_remove().unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));

    // `timeout` resets the scan (already first here).
    ctx.receive_reset().unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
}

#[test]
fn remove_message_unlinks_only_the_matched_candidate() {
    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut ctx = RuntimeContext::new(&mut rt);
    ctx.send_message(pid, make_small(1)).unwrap();
    ctx.send_message(pid, make_small(2)).unwrap();
    ctx.send_message(pid, make_small(3)).unwrap();

    ctx.receive_skip().unwrap(); // candidate `2`
    ctx.receive_remove().unwrap();

    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    let left: Vec<Eterm> = mailbox.iter().map(beam_message::Message::payload).collect();
    assert_eq!(left, vec![make_small(1), make_small(3)]);
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
}

#[test]
fn an_empty_mailbox_blocks_and_the_plain_entry_is_the_typed_blocked_error() {
    // Mirrors the reference wait-on-an-empty-mailbox case: the engine reports `Wait`,
    // and the non-blocking entry folds that into the documented typed error.
    let image = mailbox_fixture();
    let wait = image.atom_table.lookup("wait").expect("wait/0");
    let mut rt = runtime();
    let mut machine = Machine::new();

    let outcome = {
        let mut ctx = RuntimeContext::new(&mut rt);
        machine
            .run_export_outcome(&image, wait, 0, &[], &mut ctx)
            .expect("wait/0")
    };
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "an empty mailbox suspends the receive, got {outcome:?}"
    );

    // The non-blocking entry folds the suspension into the documented typed error.
    let error = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.run_export(&image, "wait", 0, &[])
            .expect_err("run_export is non-blocking")
    };
    assert_eq!(error.to_string(), beam_engine::blocked().to_string());
}

#[test]
fn a_suspended_receive_resumes_against_the_runtime_stored_scan_position() {
    // The point of storing the scan state per pid: the resume happens through a
    // **fresh context**, and it still finds the position where the receive blocked.
    let image = mailbox_fixture();
    let wait = image.atom_table.lookup("wait").expect("wait/0");
    let mut rt = runtime();
    let mut machine = Machine::new();

    let outcome = {
        let mut ctx = RuntimeContext::new(&mut rt);
        machine
            .run_export_outcome(&image, wait, 0, &[], &mut ctx)
            .expect("wait/0")
    };
    let (func, pc) = match outcome {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("expected a wait, got {other:?}"),
    };

    // Deliver a message to the runtime's own mailbox (a `receive` waiting on it).
    let pid = rt.self_pid();
    rt.processes
        .deliver_simple(pid, pid.to_eterm(), make_small(42))
        .expect("deliver");

    let mut ctx = RuntimeContext::new(&mut rt);
    let resumed = machine
        .resume_outcome(&image, func, pc, &mut ctx)
        .expect("resume at the wait target");
    assert_eq!(resumed, RunOutcome::Returned(make_small(42)));
    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    assert!(
        mailbox.is_empty(),
        "the resumed receive removed the matched message"
    );
}

#[test]
fn scan_skips_a_non_matching_head_and_keeps_it_queued() {
    // Mirrors the reference `scan_skips_a_non_matching_head_before_matching`.
    let image = mailbox_fixture();
    let mut rt = runtime();
    let result = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.run_export(&image, "scan", 0, &[]).expect("scan/0")
    };
    assert_eq!(result, make_small(7), "the second message matched");

    let mailbox = rt
        .processes
        .get(rt.self_pid())
        .expect("the runtime process")
        .mailbox();
    assert_eq!(mailbox.len(), 1, "only the matched message was removed");
    assert_eq!(
        mailbox.peek().expect("the scanned-past head").payload(),
        make_small(42)
    );
}

#[test]
fn a_marker_reserved_on_an_empty_queue_is_the_shared_undefined_atom() {
    let mut rt = runtime();
    // The atom `receive_marker_reserve` interns is the C `am_undefined`.
    let undefined = rt.intern("undefined");
    let mut ctx = RuntimeContext::new(&mut rt);

    let marker = ctx.receive_marker_reserve().expect("reserve");
    assert_eq!(marker, undefined, "an empty queue has no position to record");
    assert!(!is_small(marker), "`undefined` is an atom, not a marker id");

    // Binding it is a silent no-op, and using the reference cannot move the scan.
    ctx.receive_marker_bind(marker, make_small(1)).unwrap();
    ctx.receive_marker_use(make_small(1)).unwrap();
    assert_eq!(ctx.receive_next().unwrap(), None);
}

#[test]
fn the_marker_prologue_repositions_the_scan() {
    // Mirrors the reference `the_mailbox_repositions_the_scan_at_a_marker`, driven
    // through the real context (so it exercises `RuntimeContext::mailbox_parts`).
    let mut rt = runtime();
    let pid = self_term(&rt);
    let mut ctx = RuntimeContext::new(&mut rt);
    ctx.send_message(pid, make_small(1)).unwrap();

    ctx.receive_skip().unwrap(); // scan past `1`
    let marker = ctx.receive_marker_reserve().expect("reserve");
    assert!(is_small(marker), "a recorded marker is a small integer");
    ctx.receive_marker_bind(marker, make_small(999)).unwrap();

    // A later message (self-delivered through the same context).
    ctx.send_message(pid, make_small(2)).unwrap();

    ctx.receive_reset().unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(1)));
    ctx.receive_marker_use(make_small(999)).unwrap();
    assert_eq!(
        ctx.receive_next().unwrap(),
        Some(make_small(2)),
        "the marker moved the scan past the first message"
    );

    ctx.receive_marker_clear(make_small(999)).unwrap();
    ctx.receive_reset().unwrap();
    ctx.receive_marker_use(make_small(999)).unwrap();
    assert_eq!(
        ctx.receive_next().unwrap(),
        Some(make_small(1)),
        "a cleared marker no longer repositions the scan"
    );
}

#[test]
fn for_process_resolves_the_mailbox_against_the_supplied_table() {
    // The scheduler-pass seam: a context running a pid in a table it owns separately
    // from the runtime, with the scan state still keyed by pid in the runtime.
    let mut rt = runtime();
    let mut table = beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut beam_term::prelude::AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("spawn a process");
    let undefined = rt.intern("undefined");

    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    assert_eq!(ctx.pid(), pid);
    // Empty: `Ok(None)` from the external table's process, `undefined` marker.
    assert_eq!(ctx.receive_next().unwrap(), None);
    assert_eq!(ctx.receive_marker_reserve().unwrap(), undefined);

    // A self-delivery lands in the external table's process mailbox.
    ctx.send_message(pid.to_eterm(), make_small(5)).unwrap();
    assert_eq!(ctx.receive_next().unwrap(), Some(make_small(5)));

    // `erlang:self/0` reports the context's pid, not the runtime's own.
    let self_term = ctx.call_bif("erlang", "self", 0, &[]).expect("self/0");
    assert_eq!(self_term, pid.to_eterm());

    // Drop the context to release its borrow of `table`, then inspect the mailbox.
    drop(ctx);
    assert_eq!(table.get(pid).expect("the process").mailbox().len(), 1);
}

#[test]
fn for_process_reports_a_missing_process_as_the_typed_capability_gap() {
    let mut rt = runtime();
    let mut table = beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut beam_term::prelude::AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("spawn a process");

    // Remove the process *before* building the context: a receive-seam call must be
    // the typed `unsupported` capability gap, never a panic and never a wrong result.
    table.take(pid).expect("take the process");
    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    let error = ctx.receive_next().expect_err("no mailbox for the pid");
    assert_eq!(
        crate::error::unsupported_reason(&error).as_deref(),
        Some(format!("receive: process {pid:?} has no mailbox").as_str())
    );
}

#[test]
fn the_record_source_predicate_selects_only_its_own_bif() {
    // Exactly one of the five mutually exclusive bridges is installed per call, so the
    // predicates must be pairwise disjoint for the name each one claims.
    assert!(RuntimeContext::uses_record_source(
        "records",
        "get_definition",
        2
    ));
    for (module, function, arity) in [
        ("records", "get", 2),
        ("records", "get_definition", 1),
        ("recs", "get_definition", 2),
        ("erlang", "load_nif", 2),
        ("erlang", "get", 1),
        ("erlang", "send", 2),
    ] {
        assert!(
            !RuntimeContext::uses_record_source(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
    assert!(RuntimeContext::uses_record_source(
        "records",
        "get_definition",
        2
    ));
    // It is not the NIF-loading arm either: the two bridges are mutually exclusive.
    assert!(!RuntimeContext::uses_nif_loader(
        "records",
        "get_definition",
        2
    ));
}

#[test]
fn the_send_predicate_selects_only_the_message_sending_bifs() {
    // The fifth bridge (`SendBridge`, the delivery seam) is installed for the two names
    // `beam_bifs` registers under **one** implementation — `erlang:send/2` and
    // `erlang:'!'/2` (`../beam_bifs/src/proc.rs`) — and nothing else.
    assert!(RuntimeContext::uses_send_hook("erlang", "send", 2));
    assert!(RuntimeContext::uses_send_hook("erlang", "!", 2));
    for (module, function, arity) in [
        ("erlang", "send", 1),
        ("erlang", "send", 3),
        ("erlang", "send_after", 3),
        ("erlang", "send_nosuspend", 2),
        ("erlang", "self", 0),
        ("erlang", "!", 1),
        ("erlang", "!", 3),
        ("lists", "send", 2),
        ("erlang", "get", 1),
        ("erlang", "load_nif", 2),
        ("records", "get_definition", 2),
    ] {
        assert!(
            !RuntimeContext::uses_send_hook(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
    // Pairwise disjoint with the other four arms: `send/2` needs the delivery bridge (it
    // borrows the whole runtime) and neither a dictionary, a NIF loader, a record source
    // nor the fun caller.
    for (function, arity) in [("send", 2), ("!", 2)] {
        assert!(!RuntimeContext::uses_dict("erlang", function, arity));
        assert!(!RuntimeContext::uses_nif_loader("erlang", function, arity));
        assert!(!RuntimeContext::uses_record_source("erlang", function, arity));
    }
}

#[test]
fn the_memory_hook_gate_selects_only_the_two_memory_bifs() {
    // `RuntimeMemory::new` folds every loaded module's `.beam` size (one `stat` syscall
    // each), so it must be built **only** for the two BIFs that read the `MemoryHook`
    // seam. A too-narrow gate would turn `erlang:memory/0,1` into the typed
    // `unsupported_bif` gap; a too-wide one would put the ~10 us scan back on the
    // arithmetic hot path.
    assert!(RuntimeContext::uses_memory_hook("erlang", "memory", 0));
    assert!(RuntimeContext::uses_memory_hook("erlang", "memory", 1));
    for (module, function, arity) in [
        ("erlang", "memory", 2),
        ("erlang", "memory", 3),
        ("erlang", "processes", 0),
        ("erts_internal", "memory", 0),
        ("erlang", "-", 2),
        ("erlang", "+", 2),
        ("lists", "memory", 1),
    ] {
        assert!(
            !RuntimeContext::uses_memory_hook(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
}

#[test]
fn the_fun_identity_gate_selects_only_the_external_image_bifs() {
    // The `LocalFunIdentity` seam is read by exactly one `beam_bifs` code path
    // (`binconv::encode_body`'s first statement), which `term_to_binary/1,2` reaches
    // directly and `external_size/1,2` reaches through `plain_image_of`. Every other
    // registered BIF must skip the `Arc` clone + `PathBuf` allocation the bridge does.
    for (function, arity) in [
        ("term_to_binary", 1),
        ("term_to_binary", 2),
        ("external_size", 1),
        ("external_size", 2),
    ] {
        assert!(
            RuntimeContext::uses_fun_identity("erlang", function, arity),
            "erlang:{function}/{arity}"
        );
    }
    for (module, function, arity) in [
        ("erlang", "term_to_binary", 0),
        ("erlang", "term_to_binary", 3),
        ("erlang", "external_size", 0),
        ("erlang", "external_size", 3),
        ("erlang", "binary_to_term", 1),
        ("erlang", "-", 2),
        ("lists", "term_to_binary", 1),
    ] {
        assert!(
            !RuntimeContext::uses_fun_identity(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
}

#[test]
fn the_local_node_identity_is_nonode_at_nohost_with_creation_zero() {
    // The runtime's boot seed interns the node-name atom, so the identity resolves and
    // `term_to_binary/1` of an internal pid can emit `NEW_PID_EXT` (88).
    let rt = runtime();
    let expected = rt.atoms().lookup("nonode@nohost").expect("seeded at boot");
    let mut identity = NodeIdentity {
        node: rt.atoms().lookup("nonode@nohost"),
    };
    assert_eq!(identity.local_node(), Some((expected, 0)));

    // A table without the atom answers `None` — an unencodable id, never a
    // fabricated node.
    let mut identity = NodeIdentity { node: None };
    assert_eq!(identity.local_node(), None);
}

#[test]
fn term_to_binary_of_an_internal_pid_is_a_new_pid_ext() {
    // End to end through the real dispatch: the `LocalNode` hook installed by
    // `call_registered` turns the runtime's own pid into the oracle's `NEW_PID_EXT`
    // (`88`) image. Oracle: `binary:at(term_to_binary(self()),1)` ⇒ `88`.
    let mut rt = runtime();
    let pid = rt.self_pid();
    let mut ctx = RuntimeContext::new(&mut rt);
    let image = ctx
        .call_bif("erlang", "term_to_binary", 1, &[pid.to_eterm()])
        .expect("term_to_binary(self())");
    let tag = ctx
        .call_bif("binary", "at", 2, &[image, make_small(1)])
        .expect("binary:at/2");
    assert_eq!(signed_value(tag), 88);
}

#[test]
fn the_local_fun_identity_hook_is_a_clean_miss_when_the_answer_is_unknown() {
    let mut rt = runtime();
    let node = rt.atoms().lookup("nonode@nohost");
    // A boxed non-fun term: the runtime's own `term_to_binary(7)` image (a heap bitstring).
    let image = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("erlang", "term_to_binary", 1, &[make_small(7)])
            .expect("term_to_binary(7)")
    };
    // `beam_bifs` consults this hook for **every** `term_to_binary` argument, so an
    // immediate (`7`, `[]`) and a boxed non-fun (the image) must all be clean misses —
    // never an error, never a fabricated identity. With no calling module (the `-eval`
    // context) there is nothing to derive from at all.
    let mut hook = FunIdentityBridge::new(rt.code_server(), None, node);
    // `image` is a binary on the pooled, pointer-stable BIF heap, so it outlives `ctx`.
    for term in [make_small(7), NIL, image] {
        assert_eq!(
            hook.local_fun_identity(term).expect("a miss is never an error"),
            None,
            "a non-local-fun term must be a clean no-op"
        );
    }
}

#[test]
fn the_runtime_hooks_predicate_selects_only_its_own_bifs() {
    // The `RuntimeHooksBridge` arm is one of the mutually exclusive bridges installed per
    // call, so its predicate must claim exactly the keys the bridge serves — the timer
    // family, the loaded-export check, the link set, the dynamic-tracing tag pair and
    // `erlang:statistics/1` — and no others.
    for (module, function, arity) in [
        ("erlang", "start_timer", 3),
        ("erlang", "start_timer", 4),
        ("erlang", "send_after", 4),
        ("erlang", "cancel_timer", 2),
        ("erlang", "function_exported", 3),
        ("erlang", "link", 1),
        ("erlang", "unlink", 1),
        ("erlang", "dt_spread_tag", 1),
        ("erlang", "dt_restore_tag", 1),
        ("erlang", "statistics", 1),
    ] {
        assert!(
            RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
    // Near misses: a sibling arity of a served key, a BIF another arm owns, an unrelated
    // module's function of the same name.
    for (module, function, arity) in [
        ("erlang", "start_timer", 2),
        ("erlang", "send_after", 3),
        ("erlang", "cancel_timer", 1),
        ("erlang", "function_exported", 2),
        ("erlang", "link", 2),
        ("erlang", "unlink", 0),
        ("erlang", "dt_spread_tag", 2),
        ("erlang", "dt_restore_tag", 0),
        ("erlang", "statistics", 0),
        ("erlang", "statistics", 2),
        ("timer", "send_after", 4),
        ("erlang", "send", 2),
        ("erlang", "get", 1),
        ("erlang", "load_nif", 2),
        ("records", "get_definition", 2),
        ("persistent_term", "get", 1),
        ("erlang", "get_module_info", 1),
    ] {
        assert!(
            !RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
    // Pairwise disjoint with every other mutually exclusive arm: each served key needs
    // `&mut Runtime` plus a `&mut ProcessTable`, so none of the other bridges may claim it.
    for (function, arity) in [
        ("start_timer", 3),
        ("start_timer", 4),
        ("send_after", 4),
        ("cancel_timer", 2),
        ("function_exported", 3),
        ("link", 1),
        ("unlink", 1),
        ("dt_spread_tag", 1),
        ("dt_restore_tag", 1),
        ("statistics", 1),
    ] {
        assert!(!RuntimeContext::uses_dict("erlang", function, arity));
        assert!(!RuntimeContext::uses_process_hook("erlang", function, arity));
        assert!(!RuntimeContext::uses_nif_loader("erlang", function, arity));
        assert!(!RuntimeContext::uses_record_source("erlang", function, arity));
        assert!(!RuntimeContext::uses_send_hook("erlang", function, arity));
        assert!(!RuntimeContext::uses_module_info("erlang", function, arity));
        assert!(!RuntimeContext::uses_persistent_term(
            "erlang", function, arity
        ));
    }
    // And symmetrically: none of the other arms' keys is claimed by this predicate.
    for (module, function, arity) in [
        ("erlang", "send", 2),
        ("erlang", "get", 1),
        ("erlang", "load_nif", 2),
        ("erlang", "get_module_info", 1),
        ("erlang", "whereis", 1),
        ("records", "get_definition", 2),
        ("persistent_term", "get", 1),
    ] {
        assert!(
            !RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
}

#[test]
fn the_runtime_hooks_bridge_serves_the_timer_and_export_families() {
    // End to end through the real dispatch: `erlang:start_timer/3` schedules on the
    // runtime's timer heap and answers the timer reference (the oracle's bare `#Ref<>`,
    // not `{ok, Ref}`), and `erlang:cancel_timer/2` accepts that same reference — so the
    // reference really indexes the runtime's timer heap rather than being fabricated.
    let mut rt = runtime();
    let me = rt.self_pid().to_eterm();
    let hi = rt.intern("hi");
    let ok = rt.intern("ok");
    let erlang = rt.intern("erlang");
    let start_timer = rt.intern("start_timer");
    let true_atom = rt.intern("true");
    let false_atom = rt.intern("false");
    let mut ctx = RuntimeContext::new(&mut rt);
    let reference = ctx
        .call_bif("erlang", "start_timer", 3, &[make_small(60_000), me, hi])
        .expect("erlang:start_timer/3");
    let cancelled = ctx
        .call_bif("erlang", "cancel_timer", 2, &[reference, NIL])
        .expect("erlang:cancel_timer/2");
    assert_eq!(cancelled, ok, "the default option list answers `ok`");
    // `erlang:function_exported/3` answers from this runtime's real registry (the
    // arity-shifted variant is not exported, so it answers `false`).
    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[erlang, start_timer, make_small(3)]
        )
        .expect("function_exported/3"),
        true_atom
    );
    assert_eq!(
        ctx.call_bif(
            "erlang",
            "function_exported",
            3,
            &[erlang, start_timer, make_small(5)]
        )
        .expect("function_exported/3"),
        false_atom
    );
    // A key this bridge does **not** claim still answers the typed capability gap the
    // defaulted `RuntimeHooks` seam reports — never a fabricated value.
    let error = ctx
        .call_bif("erlang", "read_timer", 1, &[reference])
        .expect_err("read_timer/1 is not served by this bridge");
    assert_eq!(
        crate::error::unsupported_reason(&error).as_deref(),
        Some("bif")
    );
}

#[test]
fn the_runtime_hooks_bridge_serves_the_dynamic_tag_and_statistics_arms() {
    // The dynamic-tracing tag pair `file:call/2` brackets every file-server round trip with
    // (`lib/kernel/src/file.erl:3040-3043`). With dynamic tracing compiled out the oracle's
    // two BIFs are pure argument checks (`erts/emulator/beam/bif.c:6080-6112` / `6162-6167`):
    // `dt_spread_tag/1` accepts `true` and `false` and answers `true`, while
    // `dt_restore_tag/1` accepts **only** `true`. Both are cross-checked against the live
    // oracle by `tests/runtime_hooks.rs`.
    let mut rt = runtime();
    let true_atom = rt.intern("true");
    let false_atom = rt.intern("false");
    let run_queue = rt.intern("run_queue");
    let wall_clock = rt.intern("wall_clock");
    let reductions = rt.intern("reductions");
    let atom_count = rt.intern("atom_count");
    // Captured before the context takes its `&mut Runtime` borrow: a fresh runtime queues
    // nothing, so this is the run-queue figure `statistics(run_queue)` must reproduce.
    let queued = rt.run_queue_len();
    let mut ctx = RuntimeContext::new(&mut rt);

    assert_eq!(queued, 0, "a fresh runtime queues no process");
    for tag in [true_atom, false_atom] {
        assert_eq!(
            ctx.call_bif("erlang", "dt_spread_tag", 1, &[tag])
                .expect("dt_spread_tag/1"),
            true_atom,
            "dt_spread_tag/1 accepts both booleans and answers `true`"
        );
    }
    assert_eq!(
        ctx.call_bif("erlang", "dt_restore_tag", 1, &[true_atom])
            .expect("dt_restore_tag(true)"),
        true_atom
    );
    // A non-boolean is `badarg` for the spreader; for the restorer `false` is one too.
    for (function, argument) in [
        ("dt_spread_tag", make_small(42)),
        ("dt_spread_tag", NIL),
        ("dt_restore_tag", false_atom),
        ("dt_restore_tag", make_small(42)),
    ] {
        let error = ctx
            .call_bif("erlang", function, 1, &[argument])
            .expect_err("a rejected tag is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{function}: {error}");
    }

    // `statistics(run_queue)` reads this runtime's **real** run queue, so it reproduces the
    // accessor's own figure rather than a constant.
    assert_eq!(
        ctx.call_bif("erlang", "statistics", 1, &[run_queue])
            .expect("statistics(run_queue)"),
        make_small(queued as i64)
    );

    // `statistics(wall_clock)` is a real monotonic clock: `{Total, SinceLastRead}`, both
    // whole milliseconds. Sleeping between two reads must advance the total and make the
    // delta positive — a fabricated or frozen counter would fail this.
    let wall_clock_pair = |ctx: &mut RuntimeContext<'_>| {
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[wall_clock])
            .expect("statistics(wall_clock)");
        assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2);
        let total = terms::tuple_element(value, 0).expect("the total");
        let since = terms::tuple_element(value, 1).expect("the delta");
        assert!(is_small(total) && is_small(since), "two small integers");
        (signed_value(total), signed_value(since))
    };
    let (first_total, first_since) = wall_clock_pair(&mut ctx);
    assert!(first_total >= 0 && first_since >= 0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (second_total, second_since) = wall_clock_pair(&mut ctx);
    assert!(
        second_total > first_total,
        "the wall clock advanced: {second_total} > {first_total}"
    );
    assert!(
        second_since > 0,
        "the delta since the previous read is real: {second_since}"
    );

    // A valid statistics key this runtime cannot measure is the typed capability gap (never
    // a fabricated number); a name that is not a statistics key at all is the oracle's
    // `badarg`.
    let error = ctx
        .call_bif("erlang", "statistics", 1, &[reductions])
        .expect_err("reductions is an honest gap");
    assert_eq!(
        crate::error::unsupported_reason(&error).as_deref(),
        Some("erlang:statistics/1: the `reductions` counter is not implemented")
    );
    for key in [atom_count, make_small(1)] {
        let error = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect_err("an unknown statistics key is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }
}
