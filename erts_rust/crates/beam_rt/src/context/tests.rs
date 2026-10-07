use beam_bifs::{AtomInterner, BifRegistry, BytesSink, LocalNode};
use beam_engine::RunOutcome;
use beam_term::{atom_val, is_small, make_small, signed_value, Atom, AtomTable};

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
