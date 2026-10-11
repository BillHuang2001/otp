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
use beam_term::{atom_val, is_small, make_atom, make_small, signed_value, Atom, AtomTable, NIL};

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

// -----------------------------------------------------------------------
// Escaped off-heap refc binaries (a nested run's payload)
// -----------------------------------------------------------------------

/// The off-heap payload owner a `beam_codec::term::make_refc_binary` call needs — exactly the
/// role `beam_engine`'s `Machine::refc_binaries` plays for a **nested run**
/// ([`RuntimeContext::run_export_blocking`]/[`RuntimeContext::run_label_blocking`], a boot
/// `{apply, …}` directive, the `{get_until, …}` io-device continuation), whose short-lived
/// machine `beam_rt` drops the moment the run returns.
struct NestedRunPayloads(Vec<beam_alloc::RefcBinary>);

impl beam_codec::OffHeapBinaries for NestedRunPayloads {
    fn alloc_refc_payload(&mut self, bytes: &[u8]) -> Result<*mut Eterm> {
        let handle = beam_alloc::RefcBinary::new(bytes)?;
        let data = handle.data_ptr();
        self.0.push(handle);
        Ok(data.cast::<Eterm>())
    }
}

/// Builds the frozen 8-word `[BinRef][ErlSubBits]` pair over a fresh off-heap payload of 200
/// bytes — the genuine representation `bs_create_bin` produces above
/// [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`]. Returns the source heap (which owns the pair and
/// must outlive the term), the payload owner, its bytes and the term.
fn escaped_off_heap_binary() -> (beam_alloc::Heap, NestedRunPayloads, Vec<u8>, Eterm) {
    let bytes: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    assert!(bytes.len() > beam_term::refc::ERL_ONHEAP_BINARY_LIMIT);
    let mut heap = beam_alloc::Heap::new(16).expect("the source heap");
    let mut payloads = NestedRunPayloads(Vec::new());
    let term =
        beam_codec::term::make_refc_binary(&mut heap, &mut payloads, &bytes, 0, bytes.len() * 8)
            .expect("the source refc-binary term");
    assert_eq!(payloads.0.len(), 1, "one off-heap payload");
    (heap, payloads, bytes, term)
}

/// **The escaped-payload delivery, end to end.** A message carrying an off-heap refc binary whose
/// only owner was a **nested run's** machine — already dropped by the time the message is sent —
/// must still reach the receiver.
///
/// This is the defect `A = base64:encode(<<…>>), X1 = binary:encode_hex(A), _ =
/// base64:encode(<<…>>), X2 = binary:encode_hex(A)` (`X1 == X2` on the oracle: the second
/// allocation used to reuse the first binary's freed block) and
/// `B = compile:forms(F, [no_spawn_compiler_process]), self() ! B` (`error:badarg`) pin, reproduced
/// without the compiler. The copy engine resolves a refc binary **by its data pointer** through the
/// copy's source table, and a sender holding no handles of its own (`erlang:send/2`/`'!'/2` through
/// the `SendBridge`, and every runtime-internal delivery) passes an empty one — so only the
/// runtime's escaped-payload store can resolve it.
#[test]
fn a_delivery_resolves_an_escaped_off_heap_payload_through_the_runtime_store() {
    let (_source_heap, payloads, bytes, term) = escaped_off_heap_binary();

    let mut rt = runtime();
    let pid = self_term(&rt);

    // The pre-fix behaviour, asserted first: the payload's only owner is still the (here:
    // simulated) nested-run machine and the store is empty, so the receiver-side copy cannot
    // resolve the term and refuses the whole message.
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        assert!(
            matches!(ctx.send_message(pid, term), Err(BeamError::BadArg)),
            "without the escaped-payload store the delivery is refused"
        );
    }
    assert!(
        queued_payloads(&rt, rt.self_pid()).is_empty(),
        "a refused delivery enqueues nothing"
    );

    // The nested run returns and its machine is dropped — `beam_rt` adopts the handles first.
    rt.adopt_escaped_binaries(&payloads.0);
    drop(payloads);

    // The sender holds **no** handles of its own, yet the delivery must resolve the escaped
    // payload: the store's clone is the only thing keeping it alive.
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message(pid, term).expect("the escaped payload resolves");
    }
    let queued = queued_payloads(&rt, rt.self_pid());
    assert_eq!(queued.len(), 1, "the message arrived");
    let mut reference_heap = beam_alloc::Heap::new(64).expect("the reference heap");
    let reference =
        beam_codec::term::make_binary(&mut reference_heap, &bytes).expect("the reference");
    assert!(
        beam_term::compare::eq(queued[0], reference, rt.atoms()),
        "the delivered binary reads back byte-identical to the payload that escaped"
    );
    assert_eq!(
        rt.eval_off_heap.binaries()[0].refcount(),
        2,
        "the receiver's message owns its own reference to the shared payload"
    );
}

/// The on-heap reference thing for `reference`, built on a pooled BIF heap — the production
/// route (`src/native/process.rs::reference_term`), so the term stays valid after the heap goes
/// back to the pool (a `beam_alloc::Heap` never resets).
fn alias_reference_term(rt: &mut Runtime, reference: MonitorRef) -> Eterm {
    let mut heap = rt.take_bif_heap().expect("a pooled BIF heap");
    let term = reference.to_eterm(&mut heap).expect("the ref thing");
    rt.put_bif_heap(heap);
    term
}

/// The payloads of every message queued for `pid`, oldest first.
fn queued_payloads(rt: &Runtime, pid: ProcessId) -> Vec<Eterm> {
    rt.processes
        .get(pid)
        .expect("the process is live")
        .mailbox()
        .iter()
        .map(|message| message.payload())
        .collect()
}

/// The compiled `Dest ! Msg` operator (`gen_server:reply/2`'s `Alias ! {Tag, Reply}`) reaches
/// the alias **owner**, and a `reply_demonitor` alias is consumed by that delivery.
///
/// `Instr::Send` calls `ExecutionContext::send_message_with_off_heap` directly
/// (`../beam_engine/src/families/receive.rs:110-113`), so this is the seam that has to resolve a
/// reference destination — without it the reply could never reach its alias.
///
/// Oracle (system `erl`, OTP 29 / `erts-17.1`):
/// `A = alias(), A ! hi, receive X -> X end` ⇒ `hi`, and after the alias is consumed a second
/// reference send no longer resolves (that half is `beam_process`'s own contract,
/// `tests/alias.rs`).
#[test]
fn a_compiled_send_to_an_alias_reference_reaches_the_owner_and_consumes_the_alias() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let reference = rt
        .processes
        .create_alias(owner, beam_process::UnaliasMode::ReplyDemonitor)
        .expect("a live owner mints an alias");
    let to = alias_reference_term(&mut rt, reference);
    assert!(
        !ProcessId::is_process(to),
        "an alias is a reference term, never a pid"
    );

    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message_with_off_heap(to, make_small(7), &[])
            .expect("a send to a live alias is delivered to its owner");
        assert_eq!(
            queued_payloads(ctx.runtime, owner),
            vec![make_small(7)],
            "the alias send reached the owner's mailbox"
        );
    }

    assert_eq!(
        rt.processes.resolve_alias(reference),
        None,
        "a reply_demonitor alias is consumed by the message that went through it"
    );
}

/// An `explicit_unalias` alias **survives** a delivery — only `erlang:unalias/1` clears it (the
/// C's `handle_altact_msg` drops the alias only for `ERTS_ML_STATE_ALIAS_ONCE`).
#[test]
fn an_explicit_unalias_alias_survives_the_send() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let reference = rt
        .processes
        .create_alias(owner, beam_process::UnaliasMode::ExplicitUnalias)
        .expect("a live owner mints an alias");
    let to = alias_reference_term(&mut rt, reference);

    {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.send_message(to, make_small(1))
            .expect("a send to a live alias is delivered to its owner");
    }

    assert_eq!(queued_payloads(&rt, owner), vec![make_small(1)]);
    assert_eq!(
        rt.processes.resolve_alias(reference),
        Some(owner),
        "explicit_unalias is not a reply-time expiry"
    );
}

/// `erlang:send(Alias, Msg)` — the BIF route (the `SendHook` bridge) — resolves the same alias
/// and returns the message, as the oracle does.
#[test]
fn the_send_bif_delivers_through_an_alias_reference() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let reference = rt
        .processes
        .create_alias(owner, beam_process::UnaliasMode::ReplyDemonitor)
        .expect("a live owner mints an alias");
    let to = alias_reference_term(&mut rt, reference);

    {
        let mut ctx = RuntimeContext::new(&mut rt);
        let answer = ctx
            .call_bif("erlang", "send", 2, &[to, make_small(9)])
            .expect("erlang:send/2 to a live alias");
        assert_eq!(signed_value(answer), 9, "a send returns the message");
        assert_eq!(
            queued_payloads(ctx.runtime, owner),
            vec![make_small(9)],
            "the BIF send reached the owner's mailbox"
        );
    }

    assert_eq!(
        rt.processes.resolve_alias(reference),
        None,
        "the BIF delivery consumes a reply_demonitor alias too"
    );
}

/// `resolve_send_target`'s contract on both seams: a pid passes through with **nothing** to
/// consume, a live alias maps to its owner plus the reference to consume, and a reference that
/// is not a live alias is the typed `badarg` (both as the compiled send and as the BIF).
///
/// The non-alias reference is a **table-minted** one (a plain `erlang:monitor/2` reference), not
/// an `erlang:make_ref/0` one: `make_ref/0` mints from the *runtime's* reference allocator while
/// monitors and aliases mint from the **process table's**, so the two id spaces can hand out the
/// same id — see the hazard note in [`super::alias`].
#[test]
fn resolve_send_target_maps_pids_and_live_aliases_and_rejects_other_references() {
    let mut rt = runtime();
    let owner = rt.self_pid();
    let alias = rt
        .processes
        .create_alias(owner, beam_process::UnaliasMode::ReplyDemonitor)
        .expect("a live owner mints an alias");
    let alias_term = alias_reference_term(&mut rt, alias);
    // A reference from the table's own allocator that is **not** an alias: a monitor.
    let monitor = rt
        .processes
        .monitor_with_tag(owner, owner, beam_process::MonitorKind::Process, None)
        .expect("a monitor reference");
    let monitor_term = alias_reference_term(&mut rt, monitor);

    {
        let mut ctx = RuntimeContext::new(&mut rt);

        let (target, consume) = ctx
            .resolve_send_target(owner.to_eterm())
            .expect("a pid is a destination");
        assert_eq!(target, owner, "a pid passes through");
        assert_eq!(consume, None, "a pid has no alias to consume");

        let (target, consume) = ctx
            .resolve_send_target(alias_term)
            .expect("a live alias is a destination");
        assert_eq!(target, owner, "an alias resolves to its owner");
        assert_eq!(
            consume,
            Some(alias),
            "the reference must be consumed after delivery"
        );

        assert!(
            matches!(ctx.resolve_send_target(monitor_term), Err(BeamError::BadArg)),
            "a monitor reference is not an alias"
        );
        assert!(
            matches!(ctx.send_message(monitor_term, make_small(4)), Err(BeamError::BadArg)),
            "a monitor reference is not a compiled-send destination"
        );
        assert!(
            matches!(
                ctx.call_bif("erlang", "send", 2, &[monitor_term, make_small(4)]),
                Err(BeamError::BadArg)
            ),
            "a monitor reference is not a send-BIF destination"
        );

        assert_eq!(
            queued_payloads(ctx.runtime, owner),
            Vec::<Eterm>::new(),
            "no refused destination delivered anything"
        );
    }
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
fn a_pump_resumes_a_cross_module_receive_whose_mailbox_the_pump_filled() {
    // `run_export` is **blocking-aware**: a cross-module callee whose bare `receive`
    // (`mailbox:wait/0` is `receive X -> X end`) finds an empty mailbox is driven by the
    // scheduler — the very `Runtime::pump_scheduler` the `-eval` evaluator's `receive`
    // drives — rather than folded into the typed `beam_engine::blocked()` gap.
    //
    // The message the pump delivers into the runtime's own mailbox (the process `run_export`
    // runs as) arrives by the same deliver+wake path a runnable sender's `send` takes, so
    // the suspended callee really re-scans a mailbox the pump changed: an `erlang:send_after/3`
    // deadline owed to ourselves is fired by the pass (`Runtime::fire_due_timers`). Without
    // the pump this exact call is the typed `blocked()` error (see the sibling case above and
    // the slice case below).
    let image = mailbox_fixture();
    let mut rt = runtime();
    let me = self_term(&rt);
    let message = make_small(42);

    let mut ctx = RuntimeContext::new(&mut rt);
    assert!(
        ctx.may_pump_scheduler(),
        "a plain context may advance the scheduler between resumes"
    );
    // A 0 ms deadline to our own mailbox. It has not fired yet, so the mailbox is still
    // empty when the callee suspends and the value can only come from the pump.
    let _reference = ctx
        .call_bif("erlang", "send_after", 3, &[make_small(0), me, message])
        .expect("arm a 0 ms timer to the runtime's own mailbox");

    let result = ctx
        .run_export(&image, "wait", 0, &[])
        .expect("the pump delivers the timed message and the receive resumes");
    assert_eq!(
        result, message,
        "the resumed receive took the message the pump delivered"
    );

    // The resume removed the matched message.
    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    assert!(mailbox.is_empty(), "the receive removed the message");
}

#[test]
fn a_slice_context_that_may_not_pump_reports_the_typed_blocked_gap() {
    // A context built by `RuntimeContext::for_process` is itself **inside** a scheduler
    // slice, so it must never re-enter the scheduler (that would run a second process from
    // within a slice). A cross-module callee that suspends on an empty mailbox therefore
    // keeps the plain non-blocking entry's typed `beam_engine::blocked()` capability gap —
    // never a wrong result and never a hang.
    let image = mailbox_fixture();
    let mut rt = runtime();
    let mut table = beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut beam_term::prelude::AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("spawn a process");

    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    assert!(
        !ctx.may_pump_scheduler(),
        "a slice context is inside a pass and may not re-enter the scheduler"
    );
    let error = ctx
        .run_export(&image, "wait", 0, &[])
        .expect_err("an empty mailbox with nothing runnable is the typed gap");
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
    // call. Its gate is neither the list of keys the bridge **serves** nor a list kept here:
    // it is `beam_bifs`' own `RUNTIME_HOOKS_BIFS` roster (every BIF registered through the
    // `runtime_bif!` macro, each reading the single `RuntimeHooks` seam), so the two sides
    // cannot drift. The keys the bridge serves are the timer family, the loaded-export
    // check, the link set, the dynamic-tracing tag families, the code-store/loading probes,
    // the internal process predicates and `notsup` probe, the trace-cleaner/error-logger
    // constants, the `code:*` coverage surface and `erlang:statistics/1`; every other key of
    // the roster answers the same typed capability gap the *defaulted* seam reports (the
    // served-key behaviour is pinned by the `the_runtime_hooks_bridge_serves_*` tests).
    let served = [
        ("erlang", "start_timer", 3),
        ("erlang", "start_timer", 4),
        ("erlang", "send_after", 4),
        ("erlang", "cancel_timer", 2),
        ("erlang", "function_exported", 3),
        ("erlang", "link", 1),
        ("erlang", "unlink", 1),
        ("erlang", "dt_spread_tag", 1),
        ("erlang", "dt_restore_tag", 1),
        ("erlang", "dt_get_tag", 0),
        ("erlang", "dt_get_tag_data", 0),
        ("erlang", "dt_put_tag", 1),
        ("erlang", "dt_append_vm_tag_data", 1),
        ("erlang", "dt_prepend_vm_tag_data", 1),
        ("erlang", "system_profile", 0),
        ("erlang", "check_old_code", 1),
        ("erlang", "finish_loading", 1),
        ("erlang", "call_on_load_function", 1),
        ("erlang", "has_prepared_code_on_load", 1),
        ("erlang", "finish_after_on_load", 2),
        ("erts_internal", "is_system_process", 1),
        ("erts_internal", "is_process_executing_dirty", 1),
        ("erts_internal", "erase_persistent_terms", 0),
        ("erts_literal_area_collector", "release_area_switch", 0),
        ("erts_trace_cleaner", "check", 0),
        ("error_logger", "warning_map", 0),
        ("code", "coverage_support", 0),
        ("code", "get_coverage_mode", 0),
        ("code", "set_coverage_mode", 1),
        ("erlang", "statistics", 1),
    ];
    for (module, function, arity) in served {
        assert!(
            RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity}"
        );
        assert!(
            beam_bifs::RUNTIME_HOOKS_BIFS.contains(&(module, function, arity)),
            "{module}:{function}/{arity} is served by the bridge but absent from the roster"
        );
    }
    // Roster keys the bridge does **not** serve are claimed too — that is the point of
    // gating on the whole roster: `beam_bifs` can never route a key through the seam without
    // the bridge being installed. The reasons these are not in `served` are per-key gaps.
    for (module, function, arity) in [
        ("erlang", "read_timer", 1),
        ("erlang", "system_info", 1),
        ("erl_ddll", "info", 2),
    ] {
        assert!(
            RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity} reads the seam, so the bridge must be installed"
        );
        assert!(
            !served.contains(&(module, function, arity)),
            "{module}:{function}/{arity} is not one of the keys the bridge serves"
        );
    }
    // Near misses — every one of them from `beam_bifs`' own must-miss set for this table:
    // a sibling arity of a roster key, an unrelated module's function of the same name, and
    // a key that belongs to another seam entirely (the memory hook, the ETS registry, the
    // module-info seam).
    for (module, function, arity) in [
        ("erlang", "start_timer", 2),
        ("erlang", "nodes", 3),
        ("erlang", "memory", 0),
        ("lists", "reverse", 1),
        ("ets", "lookup", 2),
        ("erlang", "get_module_info", 1),
    ] {
        assert!(
            !RuntimeContext::uses_runtime_hooks(module, function, arity),
            "{module}:{function}/{arity}"
        );
    }
    // Pairwise disjoint with every other mutually exclusive arm: each roster key needs
    // `&mut Runtime` plus a `&mut ProcessTable`, so none of the other bridges may claim it.
    for (function, arity) in served.iter().map(|(_, function, arity)| (*function, *arity)) {
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

/// A `(module, function, arity)` BIF key.
type HookKey = (&'static str, &'static str, u32);

/// A `needs_*` / `uses_*` hook predicate over a BIF key.
type HookPred = fn(&str, &str, u32) -> bool;

/// One `beam_bifs` hook roster: its name, its keys, its own predicate, the `beam_rt` gate
/// that must delegate to it (`None` where this crate installs the seam on **every** arm, so
/// there is no gate: the port provider and the local-node identity) and the keys the
/// predicate must **not** claim — the same near misses `beam_bifs`' own `hook_sets` tests
/// pin.
struct HookRoster {
    name: &'static str,
    table: &'static [HookKey],
    needs: HookPred,
    gate: Option<HookPred>,
    misses: &'static [HookKey],
}

/// Every exported hook roster, so the agreement and disjointness assertions below are
/// written once against `beam_bifs`' own tables rather than a second copy of the lists.
fn hook_rosters() -> [HookRoster; 13] {
    [
        HookRoster {
            name: "MEMORY_HOOK_BIFS",
            table: beam_bifs::MEMORY_HOOK_BIFS,
            needs: beam_bifs::needs_memory_hook,
            gate: Some(RuntimeContext::uses_memory_hook as HookPred),
            misses: &[
                ("erlang", "memory", 2),
                ("erlang", "system_info", 1),
                ("lists", "memory", 1),
                ("erlang", "binary_to_term", 1),
            ],
        },
        HookRoster {
            name: "LOCAL_FUN_IDENTITY_BIFS",
            table: beam_bifs::LOCAL_FUN_IDENTITY_BIFS,
            needs: beam_bifs::needs_local_fun_identity,
            gate: Some(RuntimeContext::uses_fun_identity as HookPred),
            misses: &[
                ("erlang", "term_to_binary", 0),
                ("erlang", "term_to_binary", 3),
                ("erlang", "external_size", 0),
                ("erlang", "binary_to_term", 1),
                ("lists", "term_to_binary", 1),
                ("erlang", "-", 2),
            ],
        },
        HookRoster {
            name: "DICT_HOOK_BIFS",
            table: beam_bifs::DICT_HOOK_BIFS,
            needs: beam_bifs::needs_dict_hook,
            gate: Some(RuntimeContext::uses_dict as HookPred),
            misses: &[
                ("erlang", "put", 3),
                ("erlang", "erase", 2),
                ("erlang", "memory", 0),
                ("proc_lib", "put", 2),
                ("erlang", "process_info", 1),
            ],
        },
        HookRoster {
            name: "PROCESS_HOOK_BIFS",
            table: beam_bifs::PROCESS_HOOK_BIFS,
            needs: beam_bifs::needs_process_hook,
            gate: Some(RuntimeContext::uses_process_hook as HookPred),
            misses: &[
                ("erlang", "whereis", 0),
                ("erlang", "process_info", 3),
                ("erlang", "group_leader", 1),
                ("erlang", "nodes", 0),
                ("erlang", "get_module_info", 1),
            ],
        },
        HookRoster {
            name: "PORT_HOOK_BIFS",
            table: beam_bifs::PORT_HOOK_BIFS,
            needs: beam_bifs::needs_port_hook,
            // No gate: `call_registered` installs the port provider on every arm.
            gate: None,
            misses: &[
                ("erlang", "open_port", 1),
                ("erlang", "port_info", 3),
                ("erlang", "list_to_port", 1),
                ("erlang", "port_to_list", 1),
                ("erts_internal", "port_close", 1),
            ],
        },
        HookRoster {
            name: "SEND_HOOK_BIFS",
            table: beam_bifs::SEND_HOOK_BIFS,
            needs: beam_bifs::needs_send_hook,
            gate: Some(RuntimeContext::uses_send_hook as HookPred),
            misses: &[
                ("erlang", "send", 1),
                ("erlang", "self", 0),
                ("erlang", "!", 1),
                ("maps", "send", 2),
            ],
        },
        HookRoster {
            name: "NIF_LOADER_BIFS",
            table: beam_bifs::NIF_LOADER_BIFS,
            needs: beam_bifs::needs_nif_loader,
            gate: Some(RuntimeContext::uses_nif_loader as HookPred),
            misses: &[
                ("erlang", "load_nif", 1),
                ("erlang", "load_nif", 3),
                ("code", "load_nif", 2),
            ],
        },
        HookRoster {
            name: "RECORD_SOURCE_BIFS",
            table: beam_bifs::RECORD_SOURCE_BIFS,
            needs: beam_bifs::needs_record_source,
            gate: Some(RuntimeContext::uses_record_source as HookPred),
            misses: &[
                ("records", "get_definition", 1),
                ("records", "get", 2),
                ("records", "is_exported", 1),
                ("erlang", "get_definition", 2),
            ],
        },
        HookRoster {
            name: "MODULE_INFO_BIFS",
            table: beam_bifs::MODULE_INFO_BIFS,
            needs: beam_bifs::needs_module_info,
            gate: Some(RuntimeContext::uses_module_info as HookPred),
            misses: &[
                ("erlang", "get_module_info", 0),
                ("erlang", "get_module_info", 3),
                ("code", "get_module_info", 1),
            ],
        },
        HookRoster {
            name: "PERSISTENT_TERM_BIFS",
            table: beam_bifs::PERSISTENT_TERM_BIFS,
            needs: beam_bifs::needs_persistent_term,
            gate: Some(RuntimeContext::uses_persistent_term as HookPred),
            misses: &[
                ("persistent_term", "put", 3),
                ("persistent_term", "erase", 0),
                ("persistent_term", "put_new", 3),
                ("persistent_term", "keys", 0),
                ("erlang", "persistent_term", 1),
            ],
        },
        HookRoster {
            name: "FUN_CALLER_BIFS",
            table: beam_bifs::FUN_CALLER_BIFS,
            needs: beam_bifs::needs_fun_caller,
            // No gate: the fun caller is the fallback arm, installed for every key no gated
            // arm claims (`Self::uses_fun_caller` does not exist).
            gate: None,
            misses: &[
                ("lists", "map", 3),
                ("lists", "foldl", 2),
                ("lists", "usort", 1),
                ("lists", "sort", 1),
                ("lists", "reverse", 1),
                ("maps", "next", 1),
                ("maps", "merge", 2),
                ("sets", "map", 2),
            ],
        },
        HookRoster {
            name: "LOCAL_NODE_BIFS",
            table: beam_bifs::LOCAL_NODE_BIFS,
            needs: beam_bifs::needs_local_node,
            // No gate: the local-node identity is installed on every arm.
            gate: None,
            misses: &[
                ("erlang", "node", 2),
                ("erlang", "term_to_binary", 0),
                ("erlang", "list_to_pid", 1),
                ("erlang", "local_node", 0),
                ("erlang", "open_port", 2),
            ],
        },
        HookRoster {
            name: "RUNTIME_HOOKS_BIFS",
            table: beam_bifs::RUNTIME_HOOKS_BIFS,
            needs: beam_bifs::needs_runtime_hooks,
            gate: Some(RuntimeContext::uses_runtime_hooks as HookPred),
            misses: &[
                ("erlang", "start_timer", 2),
                ("erlang", "nodes", 3),
                ("erlang", "memory", 0),
                ("lists", "reverse", 1),
                ("ets", "lookup", 2),
                ("erlang", "get_module_info", 1),
            ],
        },
    ]
}

#[test]
fn the_rt_gates_delegate_to_the_exported_hook_predicates() {
    let rosters = hook_rosters();
    // Every key every roster claims, plus every near miss: the gate comparison below runs on
    // all of them, so a `beam_rt` gate can neither claim a key `beam_bifs` does not route
    // through its seam nor drop one it does.
    let keys: Vec<HookKey> = rosters
        .iter()
        .flat_map(|roster| roster.table.iter().chain(roster.misses.iter()).copied())
        .collect();
    for roster in &rosters {
        for key in roster.table {
            let (module, function, arity) = *key;
            assert!(
                (roster.needs)(module, function, arity),
                "{} must claim {module}:{function}/{arity}",
                roster.name
            );
        }
        for key in roster.misses {
            let (module, function, arity) = *key;
            assert!(
                !(roster.needs)(module, function, arity),
                "{} must reject {module}:{function}/{arity}",
                roster.name
            );
        }
        // The `beam_rt` gate answers **exactly** like the exported predicate, on its own
        // keys and on every other roster's — the delegation is what keeps the two from
        // drifting (`beam_bifs`' own `hook_sets` tests pin the tables themselves against
        // each module's `register`).
        let Some(gate) = roster.gate else { continue };
        for key in &keys {
            let (module, function, arity) = *key;
            assert_eq!(
                gate(module, function, arity),
                (roster.needs)(module, function, arity),
                "the {} gate drifted from `beam_bifs` on {module}:{function}/{arity}",
                roster.name
            );
        }
    }
}

#[test]
fn the_mutually_exclusive_dispatch_arms_are_pairwise_disjoint() {
    // `call_registered` installs exactly **one** arm per call — first match wins — so two
    // arms claiming the same key would silently starve the later one's seam (a fun-taking
    // BIF routed to the dict arm would lose its fun caller, and so on). The nine arms are
    // the eight gated bridges plus the fallback (the fun caller).
    let arms: [(&str, HookPred); 9] = [
        ("DICT_HOOK_BIFS", beam_bifs::needs_dict_hook),
        ("PROCESS_HOOK_BIFS", beam_bifs::needs_process_hook),
        ("NIF_LOADER_BIFS", beam_bifs::needs_nif_loader),
        ("RECORD_SOURCE_BIFS", beam_bifs::needs_record_source),
        ("SEND_HOOK_BIFS", beam_bifs::needs_send_hook),
        ("MODULE_INFO_BIFS", beam_bifs::needs_module_info),
        ("PERSISTENT_TERM_BIFS", beam_bifs::needs_persistent_term),
        ("RUNTIME_HOOKS_BIFS", beam_bifs::needs_runtime_hooks),
        ("FUN_CALLER_BIFS", beam_bifs::needs_fun_caller),
    ];
    // Every roster key and every near miss is a probe: a key two arms share would surface
    // here even if neither arm's own table lists it.
    let rosters = hook_rosters();
    let keys: Vec<HookKey> = rosters
        .iter()
        .flat_map(|roster| roster.table.iter().chain(roster.misses.iter()).copied())
        .collect();
    for key in &keys {
        let (module, function, arity) = *key;
        let claimants: Vec<&str> = arms
            .iter()
            .filter(|(_, predicate)| predicate(module, function, arity))
            .map(|(name, _)| *name)
            .collect();
        assert!(
            claimants.len() <= 1,
            "{module}:{function}/{arity} is claimed by more than one mutually exclusive arm: {claimants:?}"
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
    // A key this bridge does **not** serve still answers a typed capability gap — never a
    // fabricated value. The gate claims `beam_bifs`' whole `RUNTIME_HOOKS_BIFS` roster, so
    // `read_timer/1` reaches the bridge (rather than the registry's defaulted seam) and the
    // bridge's fallback arm names the key in the reason instead of the bare `bif`.
    let error = ctx
        .call_bif("erlang", "read_timer", 1, &[reference])
        .expect_err("read_timer/1 is not served by this bridge");
    assert_eq!(
        crate::error::unsupported_reason(&error).as_deref(),
        Some("erlang:read_timer/1")
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
    let run_queues = rt.intern("run_queues");
    let run_queue_lengths = rt.intern("run_queue_lengths");
    let run_queue_lengths_all = rt.intern("run_queue_lengths_all");
    let total_run_queue_lengths = rt.intern("total_run_queue_lengths");
    let total_run_queue_lengths_all = rt.intern("total_run_queue_lengths_all");
    let scheduler_wall_time_all = rt.intern("scheduler_wall_time_all");
    let undefined = rt.intern("undefined");
    let active_tasks = rt.intern("active_tasks");
    let microstate_accounting = rt.intern("microstate_accounting");
    let wall_clock = rt.intern("wall_clock");
    let runtime_key = rt.intern("runtime");
    let garbage_collection = rt.intern("garbage_collection");
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

    // The whole run-queue family reads the same real queue: `run_queues` is the per-queue
    // **tuple** (one element — this runtime has one run queue and no dirty schedulers),
    // `run_queue_lengths`/`_all` the per-queue **list**, and `total_*` the sum.
    let value = ctx
        .call_bif("erlang", "statistics", 1, &[run_queues])
        .expect("statistics(run_queues)");
    assert_eq!(terms::tuple_arity(value).expect("a 1-tuple"), 1, "one real run queue");
    assert_eq!(
        terms::tuple_element(value, 0).expect("the queue length"),
        make_small(queued as i64)
    );
    for key in [run_queue_lengths, run_queue_lengths_all] {
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect("statistics(run_queue_lengths[_all])");
        assert_eq!(
            terms::list_elements(value).expect("a proper list"),
            vec![make_small(queued as i64)],
            "one element per real run queue"
        );
    }
    for key in [total_run_queue_lengths, total_run_queue_lengths_all] {
        assert_eq!(
            ctx.call_bif("erlang", "statistics", 1, &[key])
                .expect("statistics(total_run_queue_lengths[_all])"),
            make_small(queued as i64),
            "the sum over the one real run queue"
        );
    }

    // `scheduler_wall_time_all` is the `_all` sibling of `scheduler_wall_time`: the oracle's
    // own "not enabled" answer, exactly as this runtime never enables it.
    assert_eq!(
        ctx.call_bif("erlang", "statistics", 1, &[scheduler_wall_time_all])
            .expect("statistics(scheduler_wall_time_all)"),
        undefined,
        "never enabled, so `undefined` is exact"
    );

    // `garbage_collection` is the C's `{GCs, WordsReclaimed, 0}` shape, summed from this
    // runtime's own collector counters. A fresh runtime has run no collection yet — the
    // real-counter direction (a collection that really ran) is pinned by
    // `statistics_garbage_collection_reads_the_runtimes_own_collector_counters`.
    let value = ctx
        .call_bif("erlang", "statistics", 1, &[garbage_collection])
        .expect("statistics(garbage_collection)");
    assert_eq!(terms::tuple_arity(value).expect("a 3-tuple"), 3);
    assert_eq!(terms::tuple_element(value, 0).expect("GCs"), make_small(0));
    assert_eq!(terms::tuple_element(value, 1).expect("WordsReclaimed"), make_small(0));
    assert_eq!(
        terms::tuple_element(value, 2).expect("the reserved third element"),
        make_small(0),
        "the C's `gather_gc_info_result/1` always answers 0 here"
    );

    // A valid statistics key this runtime cannot measure is the typed capability gap (never
    // a fabricated number); a name that is not a statistics key at all is the oracle's
    // `badarg`. `active_tasks*` needs the per-run-queue "a scheduler is executing from this
    // queue" state `beam_process::Scheduler` does not keep, and `microstate_accounting` the
    // MSACC per-thread machinery; the counters it *can* measure are served (see
    // `the_served_statistics_counters_read_the_runtimes_own_state`).
    for (key, name) in [
        (active_tasks, "active_tasks"),
        (microstate_accounting, "microstate_accounting"),
    ] {
        let error = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect_err("an unmeasurable statistics key is an honest gap");
        let expected = format!("erlang:statistics/1: the `{name}` counter is not implemented");
        assert_eq!(
            crate::error::unsupported_reason(&error).as_deref(),
            Some(expected.as_str()),
            "the gap names the counter"
        );
    }
    for key in [atom_count, make_small(1)] {
        let error = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect_err("an unknown statistics key is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

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

    // `statistics(runtime)` is served from the runtime's one long-lived CPU-time clock
    // (`beam_io::cpu_time::RuntimeClock`): `{Total, SinceLastCall}` in **milliseconds of user
    // CPU time** (the C's `getrusage(RUSAGE_SELF)` measurement), so both elements are small
    // integers. The clock's anchor starts at 0, so the very first read answers two equal
    // elements — exactly the C's `runtime_prev.data.user = 0` semantics.
    let runtime_pair = |ctx: &mut RuntimeContext<'_>| {
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[runtime_key])
            .expect("statistics(runtime)");
        assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2);
        let total = terms::tuple_element(value, 0).expect("the total");
        let since = terms::tuple_element(value, 1).expect("the delta");
        assert!(is_small(total) && is_small(since), "two small integers");
        (signed_value(total), signed_value(since))
    };
    let (cpu_total, cpu_since) = runtime_pair(&mut ctx);
    assert_eq!(
        cpu_since, cpu_total,
        "the first read's delta is the whole total (the clock anchor starts at 0)"
    );
    assert!(cpu_total >= 0, "a CPU-time reading is non-negative");

    // A valid statistics key this runtime cannot measure is the typed capability gap (never
    // a fabricated number); a name that is not a statistics key at all is the oracle's
    // `badarg`. These two are asserted above, next to the served statistics arms.
}

/// `statistics(garbage_collection)` reads the runtime's **own real** collector counters: a
/// process that has really collected over its own young heap is reported, and the two figures
/// are the collector's own cumulative ones (not a placeholder).
#[test]
fn statistics_garbage_collection_reads_the_runtimes_own_collector_counters() {
    let mut rt = runtime();
    // A live process whose own collector really runs: put a scratch tuple on its young heap
    // (unrooted, so it is garbage) and collect.
    let pid = rt.processes.spawn_default().expect("a spawned process");
    {
        let process = rt.processes.get_mut(pid).expect("the process is live");
        process
            .heap_mut()
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("a scratch tuple on the young heap");
    }
    let collection = rt
        .processes
        .get_mut(pid)
        .expect("the process is live")
        .collect()
        .expect("a process-heap collection");
    assert!(
        collection.stats.words_reclaimed > 0,
        "the unrooted tuple was garbage the collector reclaimed"
    );

    // The expected answer is exactly what the live processes' collectors say now.
    let expected: (i64, i64) = rt
        .processes()
        .pids()
        .filter_map(|live| rt.processes().get(live))
        .fold((0, 0), |(collections, reclaimed), process| {
            let stats = process.collector().stats();
            (
                collections + (stats.minor_collections + stats.major_collections) as i64,
                reclaimed + stats.words_reclaimed as i64,
            )
        });
    assert!(expected.0 > 0, "a collection really ran");

    let garbage_collection = rt.intern("garbage_collection");
    let mut ctx = RuntimeContext::new(&mut rt);
    let value = ctx
        .call_bif("erlang", "statistics", 1, &[garbage_collection])
        .expect("statistics(garbage_collection)");
    assert_eq!(terms::tuple_arity(value).expect("a 3-tuple"), 3);
    assert_eq!(
        signed_value(terms::tuple_element(value, 0).expect("GCs")),
        expected.0,
        "the collections the live collectors really ran"
    );
    assert_eq!(
        signed_value(terms::tuple_element(value, 1).expect("WordsReclaimed")),
        expected.1,
        "the words those collections really reclaimed"
    );
    assert_eq!(
        terms::tuple_element(value, 2).expect("the reserved third element"),
        make_small(0)
    );
}

/// The counters this runtime keeps for `erlang:statistics/1` are read from its **own real
/// state**: the node-global reduction/context-switch counters the runner and the scheduler
/// charge, and the port subsystem's byte counters. Every figure is a real measurement — a
/// fabricated or frozen counter would fail these assertions.
#[test]
fn the_served_statistics_counters_read_the_runtimes_own_state() {
    let mut rt = runtime();
    // The two cumulative counters are credited by their real charge sites
    // (`Runtime::note_reductions`/`note_context_switches`, driven end to end by the
    // scheduler unit tests); here the anchor and read semantics are pinned directly.
    rt.note_reductions(7);
    rt.note_context_switches(3);
    let reductions = rt.intern("reductions");
    let exact_reductions = rt.intern("exact_reductions");
    let context_switches = rt.intern("context_switches");
    let io = rt.intern("io");
    let scheduler_wall_time = rt.intern("scheduler_wall_time");
    let input = rt.intern("input");
    let output = rt.intern("output");
    let undefined = rt.intern("undefined");
    let mut ctx = RuntimeContext::new(&mut rt);

    // A `{Total, SinceLast}` read of a served counter key.
    let pair = |ctx: &mut RuntimeContext<'_>, key: Eterm| {
        let value = ctx
            .call_bif("erlang", "statistics", 1, &[key])
            .expect("a served statistics key");
        assert_eq!(terms::tuple_arity(value).expect("a 2-tuple"), 2);
        let total = terms::tuple_element(value, 0).expect("the total");
        let since = terms::tuple_element(value, 1).expect("the delta");
        assert!(is_small(total) && is_small(since), "two small integers");
        (signed_value(total), signed_value(since))
    };

    // `reductions`: the first read's delta is the whole total (the anchor starts at zero) and
    // the very next read of the same key is a zero delta.
    assert_eq!(pair(&mut ctx, reductions), (7, 7));
    assert_eq!(pair(&mut ctx, reductions), (7, 0));
    // `exact_reductions` names the same quantity but keeps its **own** anchor, so it has not
    // been read yet — and reading it does not consume `reductions`' cursor.
    assert_eq!(pair(&mut ctx, exact_reductions), (7, 7));
    assert_eq!(pair(&mut ctx, reductions), (7, 0));
    assert_eq!(pair(&mut ctx, exact_reductions), (7, 0));
    // `context_switches`, same rule.
    assert_eq!(pair(&mut ctx, context_switches), (3, 3));
    assert_eq!(pair(&mut ctx, context_switches), (3, 0));

    // `scheduler_wall_time` is the oracle's own "not enabled" answer.
    assert_eq!(
        ctx.call_bif("erlang", "statistics", 1, &[scheduler_wall_time])
            .expect("scheduler_wall_time"),
        undefined
    );

    // `io` is `{{input, In}, {output, Out}}`; this runtime has written nothing to a port and
    // read nothing from one, so both byte counts are the honest zero.
    let value = ctx
        .call_bif("erlang", "statistics", 1, &[io])
        .expect("statistics(io)");
    let in_pair = terms::tuple_element(value, 0).expect("the input pair");
    let out_pair = terms::tuple_element(value, 1).expect("the output pair");
    assert_eq!(terms::tuple_element(in_pair, 0).expect("the tag"), input);
    assert_eq!(terms::tuple_element(out_pair, 0).expect("the tag"), output);
    assert_eq!(
        signed_value(terms::tuple_element(in_pair, 1).expect("the input count")),
        0
    );
    assert_eq!(
        signed_value(terms::tuple_element(out_pair, 1).expect("the output count")),
        0
    );
}

/// A module that imports `erlang:load_nif/2` is recognised as one whose own code **reads its
/// call site** (see [`module_observes_its_call_site`]), so the cross-module call seam never
/// enters it in-machine under the *caller's* identity. Every other import is ignored —
/// including `erlang:term_to_binary/1,2` and `erlang:external_size/1,2`, whose local-fun
/// `NEW_FUN_EXT` identity is derived by searching every loaded module ([`FunIdentityBridge`]),
/// not the caller alone, so a stale call site can no longer make it name the wrong module.
#[test]
fn only_the_load_nif_import_makes_a_module_site_observant() {
    /// A synthetic module importing exactly `imports` (as `{module, function, arity}`).
    fn module_importing(imports: &[(&str, &str, u32)]) -> Module {
        let mut module = Module::default();
        for (name, function, arity) in imports {
            let module_atom = module.atom_table.intern(name);
            let function_atom = module.atom_table.intern(function);
            module.imports.push(beam_loader::Import {
                module: module_atom,
                function: function_atom,
                arity: *arity,
            });
        }
        module
    }

    for observant in [("erlang", "load_nif", 2)] {
        assert!(
            module_observes_its_call_site(&module_importing(&[observant])),
            "{observant:?} reads the call site"
        );
    }
    // The same *names* in a foreign module, a different arity of the same name, and the
    // fun-image BIFs (now module-independent) are all not the runtime's call-site-reading BIF.
    for ordinary in [
        ("erlang", "load_nif", 1),
        ("erlang", "load_nif", 3),
        ("erlang", "term_to_binary", 1),
        ("erlang", "term_to_binary", 2),
        ("erlang", "external_size", 1),
        ("erlang", "external_size", 2),
        ("some_module", "load_nif", 2),
        ("some_module", "term_to_binary", 1),
        ("erlang", "+", 2),
        ("erlang", "send", 2),
    ] {
        assert!(
            !module_observes_its_call_site(&module_importing(&[ordinary])),
            "{ordinary:?} must not make a module site-observant"
        );
    }
    // The predicate is "any import matches", not "the first does".
    assert!(module_observes_its_call_site(&module_importing(&[
        ("erlang", "+", 2),
        ("lists", "map", 2),
        ("erlang", "load_nif", 2),
    ])));
    assert!(!module_observes_its_call_site(&module_importing(&[])));
}

// ---------------------------------------------------------------------------------------
// The cross-module call seam vs the GC float guard (contract A.9's E0 route)
// ---------------------------------------------------------------------------------------

/// The float-using fixture (`tests/fixtures/gc_floats.beam`) — its code really lowers a
/// float-family instruction, which is the property this case needs.
const GC_FLOATS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/gc_floats.beam"
);

/// **The seam enters a float-using callee in-machine.** [`RuntimeContext::resolve_external_call`]
/// used to refuse a module whose code touches the machine's float-register file, which routed
/// the call onto the synchronous `call_bif` → `run_export_blocking` path: that path builds a
/// **nested** machine whose admission point is never armed, so a `receive` in the callee cannot
/// suspend a scheduled slice and folds into the typed `blocked()` gap — exactly the A.9 E0
/// blocker (`compile:file/2`, whose code lowers `fconv`).
///
/// It no longer refuses: the door the engine reaches on a slice's own machine is
/// [`ExecutionContext::ensure_heap_full`], which publishes the machine's **whole FR file** as a
/// root (`crate::context::gc::publish_roots_full`), so a collection over a callee that left a
/// float register live relocates it instead of dangling it — for any module.
///
/// The GC doors keep their own copy of the caller-module test (the narrow
/// [`ExecutionContext::ensure_heap`] door cannot see the FR file), so the module is still *not*
/// float-free — the check that makes the resolution below meaningful.
#[test]
fn the_call_seam_enters_a_float_using_module_in_machine() {
    let mut rt = runtime();
    rt.load_module(GC_FLOATS_BEAM)
        .expect("the float fixture loads into the code server");
    let floats = rt
        .code_server()
        .module_handle("gc_floats")
        .expect("the fixture is cached under its own name");
    assert!(
        !rt.module_is_float_free("gc_floats", &floats),
        "the fixture lowers a float-family instruction, so the GC doors still refuse to \
         collect over it"
    );
    assert!(
        super::gc::module_uses_floats(&floats),
        "the property the guard keys on is the lowered float-family instruction"
    );

    let mut ctx = RuntimeContext::new(&mut rt);
    let resolved = ctx
        .resolve_external_call("gc_floats", "loop", 2)
        .expect("the seam reports a routing, never an error");
    let (module, label) = resolved.expect(
        "a float-using callee is entered in-machine: the call site, not the callee's \
         instruction set, is refused or admitted",
    );
    assert!(
        Arc::ptr_eq(&module, &floats),
        "the seam hands back the code server's own handle (the engine dedups contexts on it)"
    );
    let function = floats.atom_table.lookup("loop").expect("loop is interned");
    assert_eq!(
        Some(label),
        floats.export(function, 2),
        "the resolved label is the callee's own export entry"
    );

    // The remaining refusals are unchanged: an export the callee does not have is still
    // `Ok(None)`, so the synchronous path reports the oracle's `undef` byte for byte.
    assert!(
        ctx.resolve_external_call("gc_floats", "loop", 3)
            .expect("the seam reports a routing")
            .is_none(),
        "an arity the callee does not export is refused"
    );
}
// ---------------------------------------------------------------------------
// The `RuntimeHooksBridge` arms beyond the timer/tag families.
//
// Each test drives the real dispatch (`RuntimeContext::call_bif`), so the arm is reached
// through `call_registered`'s gate. The oracle-cross-checked answers (OTP 29 / `erts-17.1`)
// are pinned alongside adversarial arguments: a rejected argument is the oracle's `badarg`,
// a facility this runtime lacks is the typed `unsupported` gap (never a wrong index, never a
// fabricated value), and a wrong arity misses the arm and falls through to the seam's
// defaulted gap.
// ---------------------------------------------------------------------------

/// `erlang:dt_get_tag/0`, `dt_get_tag_data/0`, `dt_put_tag/1` (all the atom `undefined`) and
/// `dt_append_vm_tag_data/1` / `dt_prepend_vm_tag_data/1` (the argument, unchanged).
#[test]
fn the_runtime_hooks_bridge_serves_the_dynamic_tracing_tag_family() {
    let mut rt = runtime();
    let undefined = rt.intern("undefined");
    let me = rt.self_pid().to_eterm();
    let mut scratch = beam_alloc::Heap::new(64).expect("a scratch heap");
    let tuple = scratch
        .tuple(&[make_small(1), undefined])
        .expect("a two-tuple");
    let mut ctx = RuntimeContext::new(&mut rt);

    for function in ["dt_get_tag", "dt_get_tag_data"] {
        assert_eq!(
            ctx.call_bif("erlang", function, 0, &[]).expect(function),
            undefined,
            "{function}/0 answers the atom `undefined`"
        );
    }
    // `dt_put_tag/1` ignores whatever it is handed.
    for argument in [undefined, make_small(42), NIL, me] {
        assert_eq!(
            ctx.call_bif("erlang", "dt_put_tag", 1, &[argument])
                .expect("dt_put_tag/1"),
            undefined,
            "dt_put_tag/1 ignores its argument"
        );
    }
    // The VM-probe helpers apply no type check at all: the argument comes back unchanged.
    for function in ["dt_append_vm_tag_data", "dt_prepend_vm_tag_data"] {
        for argument in [make_small(42), undefined, NIL, tuple] {
            assert_eq!(
                ctx.call_bif("erlang", function, 1, &[argument])
                    .expect(function),
                argument,
                "{function}/1 returns its argument unchanged"
            );
        }
    }
    // A wrong arity is not a `RuntimeHooks` roster key at all, so the bridge is never
    // installed for it and the call falls through to the ordinary namespace path — a bare
    // `error:undef`, never a neighbouring arity's behaviour. (`needs_runtime_hooks` is
    // per `(module, function, arity)`, so only the listed arity reaches the seam.)
    for (function, arity) in [("dt_get_tag", 1u32), ("dt_put_tag", 0), ("dt_get_tag_data", 2)] {
        let error = ctx
            .call_bif("erlang", function, arity, &[make_small(1)])
            .expect_err("an unlisted arity is not a RuntimeHooks key");
        assert_eq!(
            error.as_atom(),
            Some("undef"),
            "{function}/{arity} is outside the roster, so no bridge is installed: {error}"
        );
    }
}

/// `net_kernel:dflag_unicode_io/1` — the io-device unicode fast-path probe.
///
/// Oracle-verified (OTP 29/`erts-17.1`): a **local** pid answers the atom `true` (the
/// C's `net_kernel_dflag_unicode_io_1` short-circuits for a pid on *this* node,
/// `dist.c:6693-6709`); anything that is not a pid is `badarg`. A remote pid is the
/// typed capability gap (no peer-flag accessor) rather than a fabricated `true`. A wrong
/// arity is outside the roster, so no bridge is installed (bare `error:undef`).
#[test]
fn the_runtime_hooks_bridge_serves_the_dflag_unicode_io_probe() {
    let mut rt = runtime();
    let true_atom = rt.intern("true");
    let me = rt.self_pid().to_eterm();
    let mut ctx = RuntimeContext::new(&mut rt);

    assert_eq!(
        ctx.call_bif("net_kernel", "dflag_unicode_io", 1, &[me])
            .expect("dflag_unicode_io/1 on a local pid"),
        true_atom,
        "a local pid answers `true`"
    );
    for argument in [make_small(42), NIL] {
        let error = ctx
            .call_bif("net_kernel", "dflag_unicode_io", 1, &[argument])
            .expect_err("a non-pid is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }
    let error = ctx
        .call_bif("net_kernel", "dflag_unicode_io", 0, &[])
        .expect_err("an unlisted arity is not a RuntimeHooks key");
    assert_eq!(error.as_atom(), Some("undef"), "{error}");
}

/// `erlang:system_profile/0`, `erlang:check_old_code/1`, `erlang:finish_loading/1` and the
/// three `badarg`-always on-load-hook primitives.
#[test]
fn the_runtime_hooks_bridge_serves_the_code_store_and_loading_probes() {
    let mut rt = runtime();
    let undefined = rt.intern("undefined");
    let false_atom = rt.intern("false");
    let ok = rt.intern("ok");
    let some_module = rt.intern("some_module");
    let mut scratch = beam_alloc::Heap::new(16).expect("a scratch heap");
    let one_item = scratch.cons(some_module, NIL).expect("a one-element list");
    let mut ctx = RuntimeContext::new(&mut rt);

    // No profiling subsystem runs, so the oracle's `system_profile/0` answers `undefined`.
    assert_eq!(
        ctx.call_bif("erlang", "system_profile", 0, &[])
            .expect("system_profile/0"),
        undefined
    );

    // `check_old_code/1`: any atom answers `false` (this runtime never replaces a live
    // module); anything that is not an atom is `badarg`.
    for argument in [some_module, ok] {
        assert_eq!(
            ctx.call_bif("erlang", "check_old_code", 1, &[argument])
                .expect("check_old_code/1"),
            false_atom
        );
    }
    for argument in [make_small(42), NIL] {
        let error = ctx
            .call_bif("erlang", "check_old_code", 1, &[argument])
            .expect_err("a non-atom is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

    // `finish_loading/1`: only the empty list is accepted; the non-empty list a real
    // `code:load_binary` would pass, and any non-list, are `badarg`.
    assert_eq!(
        ctx.call_bif("erlang", "finish_loading", 1, &[NIL])
            .expect("finish_loading([])"),
        ok
    );
    for argument in [one_item, make_small(42), some_module] {
        let error = ctx
            .call_bif("erlang", "finish_loading", 1, &[argument])
            .expect_err("anything but [] is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }

    // The on-load-hook primitives always answer `badarg` on this BEAMASM build: they need an
    // internal prepared-code term only `prepare_loading/2` can mint.
    let trio: [(&str, u32, [beam_term::Eterm; 2]); 3] = [
        ("call_on_load_function", 1, [one_item, NIL]),
        ("has_prepared_code_on_load", 1, [ok, NIL]),
        ("finish_after_on_load", 2, [ok, NIL]),
    ];
    for (function, arity, args) in trio {
        let error = ctx
            .call_bif("erlang", function, arity, &args[..arity as usize])
            .expect_err("the on-load-hook primitives are always badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{function}/{arity}: {error}");
    }
}

/// `erts_internal:is_system_process/1` / `is_process_executing_dirty/1`,
/// `erts_literal_area_collector:release_area_switch/0`, `erts_trace_cleaner:check/0`,
/// `error_logger:warning_map/0` and the `code:*` coverage-mode surface.
#[test]
fn the_runtime_hooks_bridge_serves_the_internal_notsup_and_constants() {
    let mut rt = runtime();
    let false_atom = rt.intern("false");
    let true_atom = rt.intern("true");
    let none = rt.intern("none");
    let line = rt.intern("line");
    let call = rt.intern("call");
    let warning = rt.intern("warning");
    let me = rt.self_pid().to_eterm();
    let remote_node = rt.intern("elsewhere@host");
    let mut remote_heap = beam_alloc::Heap::new(64).expect("a scratch heap");
    let remote_pid =
        beam_codec::term::make_external_pid(&mut remote_heap, remote_node, 1, 0, 0x1234_5678)
            .expect("an external pid");
    let mut ctx = RuntimeContext::new(&mut rt);

    for function in ["is_system_process", "is_process_executing_dirty"] {
        assert_eq!(
            ctx.call_bif("erts_internal", function, 1, &[me])
                .expect(function),
            false_atom,
            "{function}/1 answers `false` for a local pid"
        );
        // A remote pid needs the peer's process record (distribution, contract A.5), so it
        // is the typed gap rather than a fabricated `false`.
        let error = ctx
            .call_bif("erts_internal", function, 1, &[remote_pid])
            .expect_err("a remote pid is a typed gap");
        shared_gap(
            &error,
            &format!(
                "erts_internal:{function}/1 on a remote pid: distribution is not implemented"
            ),
        );
        // A term that is not a pid is `badarg`.
        let error = ctx
            .call_bif("erts_internal", function, 1, &[make_small(42)])
            .expect_err("a non-pid is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{function}: {error}");
    }

    // `release_area_switch/0` **raises** `error notsup` (the facility is not compiled in) —
    // never the bare atom.
    let error = ctx
        .call_bif("erts_literal_area_collector", "release_area_switch", 0, &[])
        .expect_err("release_area_switch/0 raises");
    let raise = error.into_raise().expect("a raise, not a reason variant");
    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
    assert_eq!(raise.reason.as_atom(), Some("notsup"));

    // The trace-cleaner and error-logger constants.
    assert_eq!(
        ctx.call_bif("erts_trace_cleaner", "check", 0, &[])
            .expect("check/0"),
        false_atom
    );
    assert_eq!(
        ctx.call_bif("error_logger", "warning_map", 0, &[])
            .expect("warning_map/0"),
        warning
    );

    // The `code:*` coverage surface: support is `true`, the current mode `none`, and
    // `set_coverage_mode/1` accepts only `none`/`line`, answering the previous `none`.
    assert_eq!(
        ctx.call_bif("code", "coverage_support", 0, &[])
            .expect("coverage_support/0"),
        true_atom
    );
    assert_eq!(
        ctx.call_bif("code", "get_coverage_mode", 0, &[])
            .expect("get_coverage_mode/0"),
        none
    );
    for mode in [none, line] {
        assert_eq!(
            ctx.call_bif("code", "set_coverage_mode", 1, &[mode])
                .expect("set_coverage_mode/1"),
            none
        );
    }
    for bad in [call, make_small(1), NIL] {
        let error = ctx
            .call_bif("code", "set_coverage_mode", 1, &[bad])
            .expect_err("an unknown coverage mode is badarg");
        assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    }
}

/// `erts_internal:erase_persistent_terms/0` answers `true` **and** really clears the
/// runtime's own persistent-term store.
#[test]
fn the_runtime_hooks_bridge_erases_the_persistent_term_store() {
    let mut rt = runtime();
    let key = rt.intern("a_persistent_key");
    let value = rt.intern("a_persistent_value");
    let default = rt.intern("the_default");
    let ok = rt.intern("ok");
    let true_atom = rt.intern("true");
    let mut ctx = RuntimeContext::new(&mut rt);

    // Store a term through the `PersistentTerm` arm and read it back.
    assert_eq!(
        ctx.call_bif("persistent_term", "put", 2, &[key, value])
            .expect("put/2"),
        ok
    );
    assert_eq!(
        ctx.call_bif("persistent_term", "get", 2, &[key, default])
            .expect("get/2"),
        value
    );

    // The reset hook answers `true` and clears the store: the same lookup now answers the
    // default, and `get/1` — which raises for an absent key — is `badarg`.
    assert_eq!(
        ctx.call_bif("erts_internal", "erase_persistent_terms", 0, &[])
            .expect("erase_persistent_terms/0"),
        true_atom
    );
    assert_eq!(
        ctx.call_bif("persistent_term", "get", 2, &[key, default])
            .expect("get/2 after the erase"),
        default
    );
    let error = ctx
        .call_bif("persistent_term", "get", 1, &[key])
        .expect_err("an absent key is badarg");
    assert_eq!(error.as_atom(), Some("badarg"), "{error}");
}

/// Assert `error` is the typed capability gap whose reason is `expected` (the reason the
/// `unsupported_reason` classifier strips the leading token from).
fn shared_gap(error: &BeamError, expected: &str) {
    assert_eq!(
        crate::error::unsupported_reason(error).as_deref(),
        Some(expected),
        "{error}"
    );
}

// ---------------------------------------------------------------------------------------
// The GC root-set hand-off: every x register and the whole FR file
// ---------------------------------------------------------------------------------------

/// The integer-only fixture (`tests/fixtures/gc_ints.beam`), used here for one property
/// only: its code contains **no** float-family instruction, so the module guard
/// ([`super::gc::module_uses_floats`]) lets a collection run over it. The bytes are never
/// executed by these cases.
const GC_INTS_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/gc_ints.beam");

/// The call site's module name — a module the rig has loaded (so [`module_may_be_collected`]
/// answers `true` from the real code server, not from a stubbed cache).
///
/// [`module_may_be_collected`]: super::gc::module_may_be_collected
const GC_CALL_SITE: &str = "gc_ints";

/// The rig the two root-hand-off cases share: a runtime with the float-free fixture loaded
/// as the call site, plus a process living in a **caller-supplied** [`ProcessTable`] — the
/// shape a scheduler slice has ([`RuntimeContext::for_process`]), and the one where a
/// collection is a legitimate admission point.
fn gc_root_set_rig() -> (Runtime, ProcessTable, ProcessId) {
    let mut rt = runtime();
    rt.load_module(GC_INTS_BEAM)
        .expect("the float-free fixture loads into the code server");
    let mut table = ProcessTable::new(beam_process::SignalAtoms::from_table(
        &mut AtomTable::new(),
    ));
    let pid = table.spawn_default().expect("spawn a process");
    (rt, table, pid)
}

/// Builds the two terms the root-set hand-off cases need on `pid`'s **own young heap**:
///
/// * `beyond` — the term that will sit in `x[1]` while the engine declares only `x[0]`
///   live (`live = 1`); and
/// * `fr_only` — the term that will be reachable **only** through the machine's FR file.
///
/// Both are two-element tuples of small integers, so their relocation is checked by value
/// (`{7, 8}` / `{9, 10}`), never by address arithmetic.
fn gc_root_terms(table: &mut ProcessTable, pid: ProcessId) -> (Eterm, Eterm) {
    let process = table.get_mut(pid).expect("the spawned process");
    let heap = process.heap_mut();
    let beyond = heap
        .tuple(&[make_small(7), make_small(8)])
        .expect("the young heap builds {7,8}");
    let fr_only = heap
        .tuple(&[make_small(9), make_small(10)])
        .expect("the young heap builds {9,10}");
    (beyond, fr_only)
}

/// Assert `relocated` is `expected`'s tuple **at its new address** — the term really moved
/// (a copied term never keeps its from-space address) and the copy is intact.
fn assert_relocated_tuple(relocated: Eterm, stale: Eterm, expected: [i64; 2], what: &str) {
    assert_ne!(
        relocated, stale,
        "{what}: the term was left pointing into the from-space — a collection moved the \
         heap without relocating it"
    );
    assert_eq!(
        terms::tuple_arity(relocated).unwrap_or_else(|e| panic!("{what}: {e}")),
        2,
        "{what}: the relocated word is not the two-element tuple it was"
    );
    for (index, value) in expected.iter().enumerate() {
        assert_eq!(
            terms::tuple_element(relocated, index).expect("in range"),
            make_small(*value),
            "{what}: element {index} of the relocated tuple"
        );
    }
}

/// **The hole-1 regression guard: a collection must root the WHOLE root set.**
///
/// The engine's `live` count is only a *hint* — a slot it declares dead at one admission
/// point can still be read later in the same run — and the FR file holds `Eterm` term slots
/// at Milestone 1, so a float register is a root like any other. Both are invisible to the
/// narrow `ensure_heap` door (it receives only an x prefix and the stack), so a collection it
/// drove would move the heap out from under them.
///
/// The case therefore drives the **full-root** admission point
/// ([`ExecutionContext::ensure_heap_full`]) with `live = 1` while a live term sits in `x[1]`
/// and a second lives only in `fr[0]`. A forced collection runs; both must come back
/// relocated (a different word, still the same tuple). A runtime that publishes only
/// `x[0..live)` and discards the FR file leaves both words stale, which is exactly the
/// failure this asserts against.
#[test]
fn a_admission_point_roots_every_x_register_and_the_whole_fr_file() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    // `x[1]` is a live term the engine declares DEAD (`live = 1`); `fr[0]` is reachable
    // through the FR file alone. Neither is in the narrow door's published root set.
    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }

    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "the forced collection really ran; {after:?}"
    );
    assert_eq!(
        after.halted, before.halted,
        "the collection and its scatter completed; {after:?}"
    );

    assert_relocated_tuple(x[1], beyond, [7, 8], "x[1] (beyond the `live` hint)");
    assert_relocated_tuple(fr[0], fr_only, [9, 10], "fr[0] (reachable only through the FR file)");
}

/// **The same guard for the collecting allocation seam.**
///
/// [`ExecutionContext::alloc_words_rooted`] is the engine's term-building admission point:
/// when the running process's young heap cannot serve `n` raw words it collects over the
/// handed-in root set and retries. The root set it receives is the machine's whole
/// [`RootSet`], so the same two roots must survive: an x register beyond the engine's `live`
/// hint, and a term reachable only through the FR file.
///
/// The nursery is filled with garbage first, so the first bump genuinely fails and the
/// collection is the only way the retry can be served.
#[test]
fn a_collecting_allocation_roots_every_x_register_and_the_whole_fr_file() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    // Fill the nursery with unreachable tuples so a 16-word bump cannot be served.
    {
        let process = table.get_mut(pid).expect("the spawned process");
        let filler = [make_small(1)];
        while process.heap().available() >= 16 {
            process
                .heap_mut()
                .tuple(&filler)
                .expect("the nursery has room for a two-word tuple");
        }
    }

    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    let before = rt.gc_stats();
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.alloc_words_rooted(16, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("a rooted allocation");
    }

    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "the refused bump collected over the handed-in roots; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "{after:?}");
    assert_eq!(
        after.fallbacks, before.fallbacks,
        "the collection emptied the nursery, so the retry was served from the young heap \
         (no arena fallback); {after:?}"
    );

    assert_relocated_tuple(x[1], beyond, [7, 8], "x[1] (beyond the `live` hint)");
    assert_relocated_tuple(fr[0], fr_only, [9, 10], "fr[0] (reachable only through the FR file)");
}

// ---------------------------------------------------------------------------------------
// The GC root-set hand-off, hole 2: a root in a region the collector does not scan
// ---------------------------------------------------------------------------------------

/// Builds the pooled-BIF-heap shape the `erl_error:format_exception` repro hit: a **cons
/// cell built on the runtime's pooled BIF heap** (a region the copying collector classifies
/// as `Target::Foreign` — it leaves the pointer in place and does not follow it) whose `car`
/// aliases a term on `pid`'s **own young heap**.
///
/// Returns `(foreign, child)`:
///
/// * `child` — a two-element tuple `{7,8}` on the process's young heap (a term the collector
///   *would* relocate); and
/// * `foreign` — the one-element list `[child]` whose cons cell lives in `rt.bif_heaps[0]`.
///
/// The list's own address is in neither the young nor the mature space of `pid`, so it is
/// exactly the root [`super::gc::root_is_unscanned`] must flag. The pool heap lives in the
/// runtime (index `0` is never popped in these tests), so the cons cell stays valid.
fn foreign_root_aliasing_a_young_child(
    rt: &mut Runtime,
    table: &mut ProcessTable,
    pid: ProcessId,
) -> (Eterm, Eterm) {
    let child = table
        .get_mut(pid)
        .expect("the spawned process")
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("the young heap builds {7,8}");
    let foreign = rt
        .bif_heaps
        .first_mut()
        .expect("the runtime always owns its pooled BIF heap")
        .cons(child, NIL)
        .expect("the pooled BIF heap builds the cons cell");
    (foreign, child)
}

/// **The hole-2 regression guard: a collection is DECLINED while a live machine root is a
/// container in a region the collector does not scan.**
///
/// A copying collector relocates exactly the words in its *from* set (here the process's
/// young space) and classifies every other address as `Target::Foreign`
/// (`../../beam_gc/src/copy.rs`): it leaves such a pointer where it is **and does not follow
/// it**, so a young word the foreign container points *at* is neither kept alive by it nor
/// updated when it moves — the term goes stale.
///
/// The `erl_error:format_exception` repro hit exactly this: a registered BIF built its result
/// on the runtime's pooled BIF heap (`../context/mod.rs`'s `call_registered` heart) and left
/// it in `x[0]` when the process's minor collection ran. This case reproduces the shape
/// directly at the admission point: `x[0]` holds a cons cell built on `rt.bif_heaps[0]` whose
/// `car` is a young `{7,8}`, and a **forced** collection asks the runtime to move that heap.
///
/// The fix declines the move: `gc_stats().collections` must **not** advance, the forced
/// request must stay armed (it is consumed only by a collection that is committed to), and
/// the root must remain the valid `[child]` list — its `car` still reads back the intact
/// `{7,8}` from the young heap it aliases. Without the valve the collection runs and the
/// `collections` assertion is the one that fires.
#[test]
fn a_collection_is_declined_when_a_live_root_lives_in_an_unscanned_region() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (foreign, _child) = foreign_root_aliasing_a_young_child(&mut rt, &mut table, pid);

    // The predicate on its own: the cons cell's address is in no region this process's
    // collector scans (neither the young space nor the mature space).
    {
        let process = table.get(pid).expect("the spawned process");
        assert!(
            super::gc::root_is_unscanned(process, foreign),
            "a cons cell built on the pooled BIF heap is not a young/mature-heap pointer, so \
             the root is unscanned"
        );
        assert!(
            super::gc::any_root_is_unscanned(process, [foreign].iter()),
            "`any_root_is_unscanned` must agree with the single-term predicate"
        );
    }

    let mut x = [foreign, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();

    // The decline: the forced collection was NOT run.
    assert_eq!(
        after.collections, before.collections,
        "the collection must be declined while a live root is a container in an unscanned \
         region — it would relocate this heap's young words out from under that root; {after:?}"
    );
    assert_eq!(after.halted, before.halted, "nothing failed; {after:?}");
    assert!(
        rt.gc_request_pending(pid),
        "a declined collection must leave the forced request armed for the runner's park arm"
    );

    // The heap kept its terms, so the root is still the valid `[child]` list and its `car`
    // still reads the intact young `{7,8}` it aliases.
    assert_eq!(
        x[0], foreign,
        "the declined collection moved nothing, so the root word is unchanged"
    );
    assert!(
        beam_term::compare::eq(x[0], foreign, rt.atoms()),
        "the root is structurally the same list after the decline"
    );
    let head = terms::cons_head(x[0]).expect("the root is a cons cell");
    assert!(
        beam_term::compare::eq(terms::cons_tail(x[0]).expect("a cons cell"), NIL, rt.atoms()),
        "the root is exactly the one-element list `[child]`"
    );
    assert_eq!(
        terms::tuple_arity(head).expect("the `car` is a tuple"),
        2,
        "the aliased young child is still the two-element tuple"
    );
    assert_eq!(
        terms::tuple_element(head, 0).expect("in range"),
        make_small(7),
        "element 0 of the aliased child survived the declined collection"
    );
    assert_eq!(
        terms::tuple_element(head, 1).expect("in range"),
        make_small(8),
        "element 1 of the aliased child survived the declined collection"
    );
}

/// **The valve's non-firing guard: an ordinary young-only root set is never declined.**
///
/// A root set made only of immediates and pointers into the process's **own young heap** is
/// fully scanned, so the valve must not fire: `any_root_is_unscanned` is `false` and the
/// forced admission-point collection really runs and relocates both terms (the x register
/// beyond the engine's `live` hint *and* the FR-only term). This is the guard against the
/// valve over-firing and quietly disabling collection for every ordinary workload.
#[test]
fn an_ordinary_young_only_root_set_is_never_declined() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (beyond, fr_only) = gc_root_terms(&mut table, pid);

    let mut x = [make_small(0), beyond, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [fr_only];

    {
        let process = table.get(pid).expect("the spawned process");
        assert!(
            !super::gc::any_root_is_unscanned(process, x.iter().chain(stack.iter()).chain(fr.iter())),
            "immediates and own-young-heap pointers are all in scanned regions, so no root \
             may be reported as unscanned"
        );
    }

    let before = rt.gc_stats();
    rt.request_gc(pid);
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        ctx.ensure_heap_full(0, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the heap-admission point never fails the program");
    }
    let after = rt.gc_stats();
    assert_eq!(
        after.collections,
        before.collections + 1,
        "the valve must not fire for a young-only root set: the forced collection really ran; \
         {after:?}"
    );
    assert_eq!(after.halted, before.halted, "{after:?}");

    assert_relocated_tuple(x[1], beyond, [7, 8], "x[1] (beyond the `live` hint)");
    assert_relocated_tuple(fr[0], fr_only, [9, 10], "fr[0] (reachable only through the FR file)");
}

/// **The same guard on the collecting allocation seam.**
///
/// [`ExecutionContext::alloc_words_rooted`] is the second site the fix reconciles: when the
/// running process's young bump fails it collects over the handed-in root set — but a root in
/// an unscanned region makes that move unsound, so the valve must decline the collection and
/// take the pointer-stable arena fallback instead (`Runtime::gc_stats().fallbacks` advances,
/// `collections` does not). The nursery is filled with garbage first, so the bump genuinely
/// fails and the collection is the only way the retry could be served.
#[test]
fn a_rooted_allocation_is_declined_when_a_live_root_lives_in_an_unscanned_region() {
    let (mut rt, mut table, pid) = gc_root_set_rig();
    let (foreign, _child) = foreign_root_aliasing_a_young_child(&mut rt, &mut table, pid);

    // Fill the nursery with unreachable tuples so a 16-word bump cannot be served.
    {
        let process = table.get_mut(pid).expect("the spawned process");
        let filler = [make_small(1)];
        while process.heap().available() >= 16 {
            process
                .heap_mut()
                .tuple(&filler)
                .expect("the nursery has room for a two-word tuple");
        }
    }

    let mut x = [foreign, NIL, NIL, NIL];
    let mut stack = [NIL];
    let mut fr = [NIL];

    let before = rt.gc_stats();
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.current_module = Some(GC_CALL_SITE.to_string());
        ctx.arm_gc();
        let block = ctx
            .alloc_words_rooted(16, RootSet::new(&mut x, &mut stack, &mut fr, 1))
            .expect("the declined collection falls back to the arena, which never errors");
        assert!(!block.is_null(), "a block was really handed out");
    }
    let after = rt.gc_stats();

    assert_eq!(
        after.collections, before.collections,
        "the refused bump must NOT collect over a root set holding a container in an unscanned \
         region; {after:?}"
    );
    assert_eq!(
        after.fallbacks,
        before.fallbacks + 1,
        "the declined collection takes the pointer-stable arena fallback; {after:?}"
    );
    assert_eq!(
        x[0], foreign,
        "the arena fallback moved nothing, so the unscanned root is unchanged"
    );
    let head = terms::cons_head(x[0]).expect("the root is still the cons cell");
    assert_eq!(
        terms::tuple_element(head, 0).expect("in range"),
        make_small(7),
        "the aliased young child is still intact"
    );
}

/// The nested blocking pump ([`super::pump_until_returned`], the engine of
/// [`RuntimeContext::run_export_blocking`]/[`run_label_blocking`]) must not spin without end
/// when a process stays **runnable forever**: its `ran == 0` exit never triggers, so only the
/// wall-clock cap can end the drive.
///
/// `step` here always reports progress (`Ok(Some(Wait))`) — exactly the shape of a
/// perpetually runnable queue. Without the cap the loop below would never return; with it the
/// drive gives up with the typed diagnostic, so this is a genuine regression guard. The cap
/// is injected (production uses the generous [`super::BLOCKING_PUMP_DEADLINE`]) so the test
/// spins only for `limit`, not for the ten seconds the production bound allows.
#[test]
fn a_perpetually_runnable_blocking_pump_gives_up_at_the_wall_clock_cap() {
    let limit = std::time::Duration::from_millis(20);
    let mut steps = 0u64;
    let error = super::pump_until_returned(limit, RunOutcome::Wait { func: 0, pc: 0 }, |_f, _p| {
        steps += 1;
        Ok(Some(RunOutcome::Wait { func: 0, pc: 0 }))
    })
    .expect_err("an always-runnable pump must not run forever");
    assert!(steps > 0, "the drive pumped at least once before giving up");
    assert_eq!(
        error.to_string(),
        crate::error::message(format!(
            "blocking pump: the scheduler was still runnable after {} ms",
            limit.as_millis()
        ))
        .to_string(),
        "the drive reports the typed deadline diagnostic, never a hang and never a value"
    );
    // The production cap is a safety net, not an ordinary path: it must stay generous
    // (comfortably above the boot drive's own 10 s pass budget), so a silent shrink below the
    // workloads it protects is itself a regression.
    assert!(
        super::BLOCKING_PUMP_DEADLINE >= std::time::Duration::from_secs(10),
        "the blocking-pump cap must not be tightened below the workloads it protects"
    );
}

/// The **normal** path: a machine that returns ends the drive immediately, however short the
/// cap is — the deadline is a safety net, never the ordinary exit.
#[test]
fn a_returned_blocking_machine_ends_the_pump_without_the_cap() {
    let value = super::pump_until_returned(
        std::time::Duration::ZERO,
        RunOutcome::Returned(make_small(7)),
        |_f, _p| panic!("a returned machine never pumps"),
    )
    .expect("a returned machine yields its value");
    assert_eq!(signed_value(value), 7);
}

/// A `step` that reports **nothing ran** (`Ok(None)` — a context that may not pump, or a
/// drained queue) keeps the engine's honest typed `blocked` gap, and does so before the cap
/// (checked first) could fire.
#[test]
fn a_step_that_reports_nothing_ran_keeps_the_typed_blocked_gap() {
    let error = super::pump_until_returned(
        super::BLOCKING_PUMP_DEADLINE,
        RunOutcome::Wait { func: 0, pc: 0 },
        |_f, _p| Ok(None),
    )
    .expect_err("nothing ran is the blocked gap");
    assert_eq!(error.to_string(), beam_engine::blocked().to_string());
}

// -----------------------------------------------------------------------
// the nested run's own finite `receive … after T` deadline
// -----------------------------------------------------------------------

/// The committed `receive … after T` fixture (regeneration recipe in
/// `tests/fixtures/README.md`): every export pins one literal-timeout opcode shape.
const RECV_AFTER_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/recv_after.beam"
);

/// A **nested run** — the [`RuntimeContext::run_export_blocking`] route the `-eval` driver
/// and the fun bridge use — really delivers the timeout value of a finite
/// `receive … after T` that only its own deadline can satisfy.
///
/// The engine arms **no** timer for a `wait_timeout`, and the machine a nested run drives is
/// not a scheduled process, so no scheduler pass ever fires this deadline: with an empty run
/// queue the pump reports `0` on every round. [`super::RuntimeContext::pump_once`] therefore
/// recovers the deadline through
/// [`Machine::wait_deadline`](beam_engine::Machine::wait_deadline) and waits it out itself —
/// without that, this very call is the typed `blocked()` gap. Oracle:
/// `recv_after:after20()` ⇒ `timeout`.
#[test]
fn a_nested_finite_receive_delivers_its_timeout() {
    let image = beam_loader::load_file(RECV_AFTER_BEAM).expect("load recv_after.beam");
    // The fixture's own `timeout` atom is the one its code stream carries, so the term the
    // machine returns is exactly this atom.
    let expected = image
        .atom_table
        .lookup("timeout")
        .expect("the fixture names `timeout`");
    let mut rt = runtime();

    let started = Instant::now();
    let value = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.run_export(&image, "after20", 0, &[])
            .expect("the nested run waits its own deadline out")
    };
    let elapsed = started.elapsed();

    assert_eq!(
        value,
        make_atom(expected),
        "the `after` body ran: the timeout value, never the typed `blocked()` gap"
    );
    assert!(
        elapsed >= Duration::from_millis(20),
        "the timeout is never delivered before its 20 ms delay has really elapsed \
         (took {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the wait ends at the deadline rather than spinning out the safety cap \
         (took {elapsed:?})"
    );
}

/// The negative control for the case above: a parked `receive … after infinity` compiles to a
/// bare `wait`, so [`Machine::wait_deadline`](beam_engine::Machine::wait_deadline) reports
/// `delay_ms: None` — no deadline to wait for. The unchanged pump rule therefore stands: one
/// pump that reports nothing ran is the honest typed `blocked()` gap, never a wait for a
/// deadline that does not exist (and never a hang).
#[test]
fn a_nested_infinite_receive_keeps_the_typed_blocked_gap() {
    let image = beam_loader::load_file(RECV_AFTER_BEAM).expect("load recv_after.beam");
    let mut rt = runtime();
    let error = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.run_export(&image, "wait_forever", 0, &[])
            .expect_err("`after infinity` has no deadline to wait for")
    };
    assert_eq!(error.to_string(), beam_engine::blocked().to_string());
}

/// The **other** half of the finite-`after` branch: a message that arrives before the deadline
/// wins the race, and the abandoned deadline is not also fired.
///
/// Oracle: a receiver parked in `receive X -> X after 2000 -> timeout end` that is sent a
/// message returns **that message** — the send cancels the deadline, so the answer is neither
/// `timeout` nor a wait for the full 2000 ms. The message reaches the very mailbox the nested
/// run scans through the pump itself (`erlang:send_after/3`'s due deadline is fired by the
/// scheduler pass), so this pins the "a pump that reported progress resumes the machine and a
/// non-`Wait` outcome is returned at once" half of
/// [`super::RuntimeContext::pump_once`] on the finite path.
#[test]
fn a_message_beats_a_nested_finite_receive_deadline() {
    let image = beam_loader::load_file(RECV_AFTER_BEAM).expect("load recv_after.beam");
    let mut rt = runtime();
    let me = self_term(&rt);
    let message = make_small(42);

    let mut ctx = RuntimeContext::new(&mut rt);
    // 5 ms — far below `after2000/0`'s 2000 ms deadline, so the message can only win it.
    let _reference = ctx
        .call_bif("erlang", "send_after", 3, &[make_small(5), me, message])
        .expect("arm a 5 ms timer to the runtime's own mailbox");

    let started = Instant::now();
    let value = ctx
        .run_export(&image, "after2000", 0, &[])
        .expect("the pump delivers the timed message and the receive takes it");
    let elapsed = started.elapsed();

    assert_eq!(value, message, "the message cancelled the deadline");
    assert!(
        elapsed < Duration::from_millis(1000),
        "the 2000 ms deadline was abandoned, not waited out (took {elapsed:?})"
    );
    let mailbox = ctx
        .runtime
        .processes
        .get(ctx.runtime.self_pid)
        .expect("the runtime process")
        .mailbox();
    assert!(mailbox.is_empty(), "the receive removed the message");
}
