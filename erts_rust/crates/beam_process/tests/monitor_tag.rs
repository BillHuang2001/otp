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

//! Integration evidence for the **`erlang:monitor/3` `{tag, Tag}` user tag**: the notification a
//! tagged monitor delivers is the oracle's `{Tag, Ref, Kind, Target, Reason}` in place of the
//! ordinary `{'DOWN', Ref, Kind, Target, Reason}`, for a **live** target, a target that
//! terminates with a reason, a target that is already dead (`noproc`), a **registered name**
//! target and a **port** target.
//!
//! # Where the tag lives (and why every shape is accepted)
//!
//! Not on the monitor records: `Monitor`/`MonitoredBy`/`PortWatch` are built by struct literal
//! from other crates, so their shape is fixed (adding a field would break them). The tag is kept
//! in a **side table on `ProcessTable`**, keyed by the monitor reference — exactly like the
//! `{Name, node}` descriptor of a named monitor (see `monitor.rs` and `ptab.rs`, Design
//! Decisions).
//!
//! A plain-value side table is not a collector root set, so a tag word resident in a per-process
//! heap could not be relocated and a later DOWN would carry a dangling tag. The C keeps an
//! **immediate** tag inline in the monitor allocation and copies every other shape into its own
//! `ErlHeapFragment` (`mk_heap_frag_eterm`, `erts/emulator/beam/erl_monitor_link.c:833-900`);
//! `beam_process::tag`'s `TagSlot` is that record in Rust form, so **every** term shape the
//! oracle accepts is accepted here:
//!
//! * an **immediate** (atom, small integer, pid/port, `nil`) or a term resident in a **literal
//!   area** (shared, frozen, never relocated) is stored verbatim and allocates nothing — the
//!   `{tag, mytag}`-shaped monitor pays nothing for this model (pinned by `tag.rs`'s unit tests
//!   `an_immediate_tag_is_stored_verbatim_and_allocates_nothing` /
//!   `a_literal_area_tag_is_stored_verbatim_and_allocates_nothing`, which assert the slot owns
//!   **no** fragment for them, and end to end by
//!   [`a_literal_area_tag_is_accepted_end_to_end`]);
//! * a **heap-resident** term of any size or nesting — a list, a tuple, a map, a big binary, the
//!   `[a, make_ref(), #{k => <<"v">>}]` shape `gen_server`'s `multi_call` and Khepri produce — is
//!   copied at monitor-creation time into the slot's own `beam_message::HeapFragment`, released
//!   with the monitor (`erlang:demonitor/1`) and never scanned by the collector. The copy is
//!   therefore **independent** of the watcher's heap: it survives the watcher's own
//!   `erlang:garbage_collect()`, which is what
//!   [`a_heap_resident_tag_survives_the_watchers_own_garbage_collection`] and
//!   [`a_heap_resident_tag_reaches_the_notification_in_every_shape`] pin.
//!
//! # Two delivery routes for a tagged DOWN
//!
//! A fragment-resident word is valid only against the slot's own off-heap table, so it is never
//! handed to the mailbox or the signal queue directly:
//!
//! * the **queued-signal route** materialises the tag on the receiver's **own** young heap
//!   (`ptab`'s `materialise_tag_onto`) before the signal is pushed, because the queued signal's
//!   tag slot is an in-heap sub-rootset the collector gathers and scatters — pinned by
//!   [`a_queued_down_tag_word_lives_on_the_receivers_own_heap`];
//! * the **message-fragment route** builds the whole DOWN — tag, reason and envelope — on one
//!   scratch arena whose off-heap table *merges* the slot's and the reason's, then copies it into
//!   the message's own `HeapFragment` (`ptab`'s `deliver_down_fragment`), for a receiver whose
//!   heap must not move (relocation-exempt / `Running`, or the host's `CollectionGate`'s refusal)
//!   — pinned by [`an_exempt_watcher_gets_a_tagged_down_through_a_message_fragment`].
//!
//! Order matters on the queued-signal route: the **reason** is copied first (its collect-grow
//! retry is safe there, since nothing of ours is on the receiver's heap yet), then the tag, which
//! is only ever **grown** into place and never triggers a collection — a collection at that point
//! would dangle the reason word just materialised — pinned by
//! [`materialising_a_tag_grows_the_nursery_without_collecting`].
//!
//! # Oracle protocol (the same text protocol as `tests/registry_monitors.rs`)
//!
//! Every `oracle_*`-shaped check runs an equivalent Erlang expression under the system `erl`
//! (`-noshell -eval '<expr>, halt(0).'`) and asserts its printed answer. The expressions are
//! written to print only **stable** parts of the `{'DOWN', …}` tuple — the head's `=:=` identity
//! with the original tag, whether element 1 is a reference, the kind atom, whether the target is a
//! pid/port, the reason and a byte size — because the reference, the pid and the node name are not
//! reproducible from a Rust test. Where the node name is involved the expression compares against
//! `node()` instead of spelling a literal.
//!
//! # Absence of the oracle is not a failure
//!
//! If `erl` is not on `PATH` (or does not answer within [`common::ERL_TIMEOUT`]) every oracle check
//! **skips loudly** (a clear `eprintln!`) and returns, never failing the suite.

mod common;

use beam_alloc::{Heap, RefcBinary};
use beam_gc::HeapPolicy;
use beam_message::Message;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::list::ptr_val;
use beam_term::prelude::{
    heap_bits_size_words, is_immediate, is_list, is_literal, make_arityval, make_atom, make_list,
    make_small, make_tuple, map_words_capacity, set_literal_tag, tuple_element, tuple_size,
    write_binary, write_map, write_refc_binary, Atom, AtomTable, Eterm, LiteralArea, NIL,
    ERL_BIN_REF_SIZE, ERL_REFC_BITS_SIZE, HEADER_BIN_REF,
};

use common::expect_oracle;

// The oracle driver (`ERL_TIMEOUT`, `erl_stdout`, `expect_oracle` and the stdout normalisation) is
// shared with the crate's other `oracle_*` files; it lives in `tests/common/mod.rs` so the
// logger-suppression fix exists once. `erl` runs with `-kernel logger_level none` and its stdout is
// normalised, so a crashed probe helper's run-time error report can never make a comparison flaky.

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A process table plus the atom table its [`SignalAtoms`] were interned from, so a test can
/// resolve the well-known message atoms (`'DOWN'`, `process`, `port`, `noproc`, ...) back to
/// terms.
struct Fixture {
    table: ProcessTable,
    names: AtomTable,
}

impl Fixture {
    /// An empty table whose atoms come from a fresh table the fixture keeps.
    fn new() -> Fixture {
        let mut names = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut names);
        Fixture {
            table: ProcessTable::new(atoms),
            names,
        }
    }

    /// The interned atom `name` (the same index space the table's signal atoms use).
    fn atom(&mut self, name: &str) -> Atom {
        self.names.intern(name)
    }

    /// The atom `name` as a term.
    fn atom_term(&mut self, name: &str) -> Eterm {
        make_atom(self.atom(name))
    }

    /// Builds a tuple on `pid`'s own heap (a *structured*, non-immediate term).
    fn tuple_in(&mut self, pid: ProcessId, elems: &[Eterm]) -> Eterm {
        self.table
            .get_mut(pid)
            .expect("the holder is live")
            .heap_mut()
            .tuple(elems)
            .expect("the tuple fits the holder's young heap")
    }

    /// The number of messages queued for `pid`.
    fn mailbox_len(&self, pid: ProcessId) -> usize {
        self.table.get(pid).expect("the recipient is live").mailbox().len()
    }

    /// The payload of `pid`'s **only** queued message (panics unless exactly one is queued).
    fn sole_payload(&self, pid: ProcessId) -> Eterm {
        let mailbox = self.table.get(pid).expect("the recipient is live").mailbox();
        assert_eq!(mailbox.len(), 1, "exactly one message is queued");
        mailbox.peek().expect("one message is queued").payload()
    }

    /// The message at `index` in `pid`'s mailbox.
    fn message_at(&self, pid: ProcessId, index: usize) -> &Message {
        let mailbox = self.table.get(pid).expect("the recipient is live").mailbox();
        mailbox
            .message_at(mailbox.cursor_at(index))
            .expect("the message is queued")
    }

    /// Removes and discards the head of `pid`'s mailbox, so a multi-phase test can again assert
    /// "exactly one message is queued".
    fn drain_one(&mut self, pid: ProcessId) {
        let drained = self
            .table
            .get_mut(pid)
            .expect("the recipient is live")
            .mailbox_mut()
            .dequeue();
        assert!(drained.is_some(), "a message was queued");
    }

    /// Fills `pid`'s young heap until exactly `free_words` words remain.
    ///
    /// The filler is an **unrooted** tuple of distinct small integers: it is only ever read as an
    /// occupancy figure, and every caller of this helper exercises a route that is forbidden to
    /// collect (so it cannot be reclaimed underneath the assertions).
    fn drain_nursery_to(&mut self, pid: ProcessId, free_words: usize) {
        let available = self
            .table
            .get(pid)
            .expect("the process is live")
            .heap()
            .available();
        assert!(
            available > free_words,
            "the fixture needs more than {free_words} free words to drain, has {available}"
        );
        let filler_words = available - free_words;
        let elems: Vec<Eterm> = (0..filler_words - 1)
            .map(|i| make_small(i as i64))
            .collect();
        self.table
            .get_mut(pid)
            .expect("the process is live")
            .heap_mut()
            .tuple(&elems)
            .expect("the filler fits the nursery it drains");
        assert_eq!(
            self.table
                .get(pid)
                .expect("the process is live")
                .heap()
                .available(),
            free_words,
            "the fixture's arithmetic holds: the filler drained the nursery exactly"
        );
    }

    /// Whether `term` refers into `pid`'s **current** young heap.
    fn on_young_heap(&self, pid: ProcessId, term: Eterm) -> bool {
        self.table
            .get(pid)
            .expect("the process is live")
            .collector()
            .young_contains(ptr_val(term))
    }
}

// ---------------------------------------------------------------------------
// Heap-term builders (the shapes a tag may take)
// ---------------------------------------------------------------------------

/// A byte-aligned on-heap binary `<<bytes…>>` built on `heap`.
fn small_binary(heap: &mut Heap, bytes: &[u8]) -> Eterm {
    let words = heap_bits_size_words(bytes.len() * 8);
    // SAFETY: the heap hands out exactly the words an `ErlHeapBits` of this size needs, and
    // `write_binary` writes the header, the bit size and the payload into them.
    let hp = unsafe { heap.alloc_words(words) }.expect("the binary fits the heap");
    // SAFETY: `hp` addresses `words` exclusively-owned, writable words just reserved.
    unsafe { write_binary(hp, bytes) }.expect("a byte-aligned binary is buildable")
}

/// The one-pair flat map `#{key => value}` built on `heap`.
fn one_pair_map(heap: &mut Heap, names: &AtomTable, key: Eterm, value: Eterm) -> Eterm {
    // SAFETY: `map_words_capacity(1)` is exactly the words a one-pair flat map needs.
    let hp = unsafe { heap.alloc_words(map_words_capacity(1)) }.expect("the map fits the heap");
    // SAFETY: `hp` addresses `map_words_capacity(1)` exclusively-owned, writable words.
    unsafe { write_map(hp, &[(key, value)], names) }.expect("a one-pair flat map is buildable")
}

/// The `gen_server`/Khepri `multi_call` tag shape `[a, Ref, #{k => <<"v">>}]`, built on `heap`.
///
/// The reference is spelled from `reference`'s **id**, so the very same shape can be rebuilt on a
/// different heap after a collection and compared with `=:=` (a reference's identity is its id).
fn multi_call_tag(
    heap: &mut Heap,
    names: &AtomTable,
    reference: MonitorRef,
    a: Atom,
    k: Atom,
) -> Eterm {
    let payload = small_binary(heap, b"v");
    let map = one_pair_map(heap, names, make_atom(k), payload);
    let reference_term = reference.to_eterm(heap).expect("the reference fits the heap");
    let tail = heap.cons(map, NIL).expect("the map cell fits");
    let middle = heap.cons(reference_term, tail).expect("the reference cell fits");
    heap.cons(make_atom(a), middle).expect("the head cell fits")
}

/// The on-heap `[BinRef][ErlSubBits]` pair naming `binary`'s payload, built on `heap`.
///
/// # Safety
///
/// `heap` must have [`ERL_REFC_BITS_SIZE`] spare words, and the caller must keep `binary` (or
/// another handle to its payload) alive as long as the returned term is used.
unsafe fn refc_pair(heap: &mut Heap, binary: &RefcBinary) -> Eterm {
    // SAFETY: the caller reserves the words and keeps the payload alive; the payload pointer is
    // word-aligned (`beam_alloc` hands out `base + HEADER_SIZE`, a whole unit).
    unsafe {
        let hp = heap
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("the refc pair fits the heap");
        write_refc_binary(
            hp,
            binary.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            binary.data_ptr(),
            0,
            binary.as_bytes().len() * 8,
        )
    }
}

/// The `(bytes, bit size)` view of the bitstring term `term`, accepting both a refc binary's
/// `ErlSubBits` and a term that still points at its `BinRef` (the discrimination
/// `beam_term`'s hashing walks make).
///
/// # Safety
///
/// `term` must be a live bitstring whose storage outlives the returned slice.
unsafe fn binary_view(term: Eterm) -> (&'static [u8], usize) {
    // SAFETY: `term` is a live boxed bitstring, so its header word is initialised; a `BinRef`
    // header fixes the `ErlSubBits` `ERL_BIN_REF_SIZE` words above it.
    let bits_term = if unsafe { *beam_term::prelude::boxed_val(term) } == HEADER_BIN_REF {
        // SAFETY: the `BinRef` header means the `ErlSubBits` sits directly above it.
        unsafe { beam_term::prelude::boxed_val(term).add(ERL_BIN_REF_SIZE) }
    } else {
        // SAFETY: a live boxed term.
        beam_term::prelude::boxed_val(term)
    };
    // SAFETY: `bits_term` is the live `ErlSubBits`/`ErlHeapBits` of `term`.
    let (base, offset, bits) = unsafe { beam_term::prelude::bitstring_parts(make_tuple(bits_term)) };
    assert_eq!(offset % 8, 0, "the tag binary is byte-aligned");
    // SAFETY: `base` addresses the bitstring's storage, which outlives the returned slice; the
    // offset is whole bytes, so `bits / 8` bytes from it are the payload.
    let bytes = unsafe { core::slice::from_raw_parts(base.add(offset / 8), bits / 8) };
    (bytes, bits)
}

// ---------------------------------------------------------------------------
// DOWN assertions
// ---------------------------------------------------------------------------

/// Asserts `payload` is the 5-tuple `{head, <a ref>, kind, target, reason}` and returns its
/// **target** element (element 3).
///
/// This is the one shape every DOWN builder produces — `{'DOWN', …}` when `head` is the interned
/// `'DOWN'` atom, `{Tag, …}` when it is the monitor's user tag. `head` is checked only when the
/// caller can state it: a *structured* tag is a structurally equal but distinct boxed term, and
/// the caller compares it with `beam_term::compare::eq` (against a term rebuilt on its own arena)
/// instead, passing `None` here. The target is returned rather than compared for the same reason
/// (a caller's `{Name, Node}` target is its own boxed term).
fn assert_down(payload: Eterm, head: Option<Eterm>, kind: Atom, reason: Eterm) -> Eterm {
    // SAFETY: `head`/`kind` come from the same table that interned the message's atoms, and
    // `payload` is the live 5-tuple the table built on the recipient's heap (or in its message
    // fragment); the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5, "a DOWN message is a 5-tuple");
        if let Some(expected) = head {
            assert_eq!(
                tuple_element(payload, 0),
                expected,
                "the head slot is the monitor's user tag (immediate forms)"
            );
        }
        assert!(is_reference(tuple_element(payload, 1)), "element 1 is a reference");
        assert_eq!(tuple_element(payload, 2), make_atom(kind));
        assert_eq!(tuple_element(payload, 4), reason);
        tuple_element(payload, 3)
    }
}

/// The **head** element (element 0) of a live DOWN payload — the monitor's tag, or the default
/// `'DOWN'` atom.
///
/// [`assert_down`] returns the *target* (element 3), so a caller that wants to compare the head
/// must read it here rather than reusing that return value.
fn head_of(payload: Eterm) -> Eterm {
    // SAFETY: `payload` is the live 5-tuple a DOWN builder produced on the recipient's heap (or in
    // its message fragment); element 0 is in bounds.
    unsafe { tuple_element(payload, 0) }
}

/// Asserts the target element of a DOWN is the pid/port immediate `expected`.
fn assert_immediate_target(target: Eterm, expected: Eterm) {
    assert_eq!(target, expected, "the DOWN target is the expected immediate");
}

/// Whether `term` is a boxed reference term (`REF_SUBTAG`).
fn is_reference(term: Eterm) -> bool {
    MonitorRef::from_eterm(term).is_ok()
}

// ---------------------------------------------------------------------------
// Process targets
// ---------------------------------------------------------------------------

#[test]
fn a_tagged_live_monitor_delivers_the_tagged_down_and_the_untagged_one_is_unchanged() {
    // The two baseline shapes, cross-checked against the oracle in one run: the tagged monitor's
    // DOWN is `{mytag, Ref, process, Pid, Reason}` and the untagged monitor's is byte-identical to
    // the ordinary `{'DOWN', Ref, process, Pid, Reason}`.
    let expr = r#"
        F = fun(Opts) ->
              P = spawn(fun() -> receive go -> ok end end),
              _ = erlang:monitor(process, P, Opts),
              P ! go,
              receive {H, R, K, T, A} -> {H, is_reference(R), K, is_pid(T), A}
              after 2000 -> timeout end
            end,
        io:format("~p|~p~n", [F([{tag, mytag}]), F([])])
    "#;
    if !expect_oracle(
        expr,
        "{mytag,true,process,true,normal}|{'DOWN',true,process,true,normal}",
    ) {
        return;
    }

    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");

    // 1. Tagged through the pid entry point: the head is the tag, the rest is the ordinary DOWN.
    let target = f.table.spawn_default().expect("a target process");
    let tagged_ref = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("both pids are live");
    f.table.terminate(target, normal).expect("the target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(mytag)), process, normal),
        target.to_eterm(),
    );
    // SAFETY: the live DOWN tuple on the watcher's heap.
    unsafe {
        assert_ne!(
            tuple_element(payload, 0),
            make_atom(down),
            "a tagged DOWN never carries the default head atom"
        );
    }
    f.drain_one(watcher);

    // 2. The same tag through the arbitrary-term entry point, which routes a local pid identically
    //    (and allocates a fresh reference — the two monitors are really separate).
    let target = f.table.spawn_default().expect("a second target");
    let term_ref = f
        .table
        .monitor_term_with_tag(
            watcher,
            target.to_eterm(),
            MonitorKind::Process,
            Some(make_atom(mytag)),
        )
        .expect("a local pid term dispatches to the pid path");
    assert_ne!(term_ref, tagged_ref, "each monitor gets its own reference");
    f.table.terminate(target, normal).expect("the target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(mytag)), process, normal),
        target.to_eterm(),
    );
    f.drain_one(watcher);

    // 3. Untagged: `monitor/2` draws the default `'DOWN'` head — unchanged by this feature.
    let untagged = f.table.spawn_default().expect("an untagged target");
    f.table
        .monitor(watcher, untagged, MonitorKind::Process)
        .expect("both pids are live");
    f.table
        .terminate(untagged, normal)
        .expect("the untagged target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(down)), process, normal),
        untagged.to_eterm(),
    );
}

#[test]
fn a_tagged_monitor_of_an_exiting_target_carries_the_death_reason() {
    let expr = r#"
        P = spawn(fun() -> receive go -> exit(boom) end end),
        _ = erlang:monitor(process, P, [{tag, mytag}]),
        P ! go,
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, is_pid(T), A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,process,true,boom}") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let boom = f.atom_term("boom");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let target = f.table.spawn_default().expect("a target process");

    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("both pids are live");
    f.table.terminate(target, boom).expect("the target is live");
    assert!(!f.table.contains(target));

    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(mytag)), process, boom),
        target.to_eterm(),
    );
}

#[test]
fn a_tagged_monitor_of_a_dead_pid_is_born_down_tagged() {
    let expr = r#"
        {P, R0} = spawn_monitor(fun() -> ok end),
        receive {DOWN, R0, _, _, _} when is_atom(DOWN) -> ok end,
        _ = erlang:monitor(process, P, [{tag, mytag}]),
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, is_pid(T), A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,process,true,noproc}") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let dead = {
        let dying = f.table.spawn_default().expect("a process about to die");
        f.table.remove(dying);
        dying
    };

    let reference = f
        .table
        .monitor_with_tag(watcher, dead, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("a non-live target is not an error");
    // Nothing is left to watch, so no monitor half is recorded — only the immediate DOWN.
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .monitors()
        .find(reference)
        .is_none());
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(mytag)), process, noproc),
        dead.to_eterm(),
    );
}

// ---------------------------------------------------------------------------
// Heap-resident tags: every shape, and the independent copy
// ---------------------------------------------------------------------------

#[test]
fn a_heap_resident_tag_reaches_the_notification_in_every_shape() {
    // The oracle accepts ANY term as a tag and echoes it `=:=` — a nested list carrying a
    // reference and a map (the `gen_server`/Khepri `multi_call` shape), a tuple, a map, and a
    // binary far above `ERL_ONHEAP_BINARY_LIMIT` (so its bytes are a real refcounted payload).
    let expr = r#"
        Shapes = [[a, make_ref(), #{k => <<"v">>}], {a, 1}, #{k => <<"v">>},
                  binary:copy(<<"x">>, 1000)],
        R = lists:map(fun(T) ->
              P = spawn(fun() -> receive go -> exit(boom) end end),
              _ = erlang:monitor(process, P, [{tag, T}]),
              P ! go,
              M = receive X -> X after 2000 -> timeout end,
              {element(1, M) =:= T, element(5, M)}
            end, Shapes),
        io:format("~p~n", [R])
    "#;
    if !expect_oracle(
        expr,
        "[{true,boom},{true,boom},{true,boom},{true,boom}]",
    ) {
        return;
    }

    // The oracle's own report of the binary shape's size, so the "arrives intact" figure is not
    // this test's invention.
    let size_expr = r#"
        B = binary:copy(<<"x">>, 1000),
        P = spawn(fun() -> receive go -> exit(boom) end end),
        _ = erlang:monitor(process, P, [{tag, B}]),
        P ! go,
        M = receive X -> X after 2000 -> timeout end,
        io:format("~p~n", [byte_size(element(1, M))])
    "#;
    if !expect_oracle(size_expr, "1000") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let boom = f.atom_term("boom");
    let a = f.atom("a");
    let k = f.atom("k");
    let watcher = f.table.spawn_default().expect("a watcher process");
    // A reference to embed in the tag. It is not a monitor of anything here — it only has to be a
    // *reference term*, which is what makes the shape non-immediate and non-trivial.
    let embedded = MonitorRef::from_id(4242);

    // 1. `[a, Ref, #{k => <<"v">>}]` — the nested `multi_call` shape.
    let tag = {
        let heap = f
            .table
            .get_mut(watcher)
            .expect("the watcher is live")
            .heap_mut();
        multi_call_tag(heap, &f.names, embedded, a, k)
    };
    assert!(is_list(tag), "the fixture really is a heap-resident list");
    assert!(
        f.on_young_heap(watcher, tag),
        "the tag starts life on the watcher's own young heap"
    );
    let target = f.table.spawn_default().expect("a target process");
    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("a heap-resident tag of any shape is accepted");
    f.table.terminate(target, boom).expect("the target dies");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, None, process, boom), target.to_eterm());
    let head = head_of(payload);
    // The delivered head is a *copy*, structurally `=:=` the original (compared against a term
    // rebuilt on a fresh arena, so nothing here reads the watcher's heap twice).
    let mut arena = Heap::new(64).expect("the expectation arena allocates");
    let expected = multi_call_tag(&mut arena, &f.names, embedded, a, k);
    assert!(
        compare::eq(head, expected, &f.names),
        "the nested list tag is echoed `=:=` to the original"
    );
    assert_ne!(
        head, tag,
        "the delivered tag is the slot's own copy, not the caller's heap word"
    );
    f.drain_one(watcher);

    // 2. A heap tuple.
    let tag = f.tuple_in(watcher, &[make_small(1), make_small(2), make_small(3)]);
    let target = f.table.spawn_default().expect("a target process");
    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("a heap tuple tag is accepted");
    f.table.terminate(target, boom).expect("the target dies");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, None, process, boom), target.to_eterm());
    let head = head_of(payload);
    // The expectation lives on a bound arena: the delivered head points into the payload, so the
    // arena it is compared against must outlive the comparison.
    let mut arena = Heap::new(8).expect("the expectation arena allocates");
    let expected = arena
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the expected tuple fits");
    assert!(compare::eq(head, expected, &f.names), "the tuple tag is echoed");
    // The delivered tag really is a receiver-heap word: it was materialised onto the watcher's
    // own young heap before the signal was queued (see `materialise_tag_onto`).
    assert!(
        f.on_young_heap(watcher, head),
        "the delivered tag lives on the watcher's own young heap"
    );
    f.drain_one(watcher);

    // 3. A heap map.
    let tag = {
        let heap = f
            .table
            .get_mut(watcher)
            .expect("the watcher is live")
            .heap_mut();
        one_pair_map(heap, &f.names, make_atom(k), make_small(7))
    };
    let target = f.table.spawn_default().expect("a target process");
    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("a heap map tag is accepted");
    f.table.terminate(target, boom).expect("the target dies");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, None, process, boom), target.to_eterm());
    let head = head_of(payload);
    let mut arena = Heap::new(map_words_capacity(1) + 1).expect("the arena allocates");
    let expected = one_pair_map(&mut arena, &f.names, make_atom(k), make_small(7));
    assert!(compare::eq(head, expected, &f.names), "the map tag is echoed");
    f.drain_one(watcher);

    // 4. A 1000-byte binary — far above the on-heap limit, so its payload is a refcounted
    //    off-heap object the slot's copy has to re-register.
    let binary = RefcBinary::new(&[0x41u8; 1000]).expect("the payload allocates");
    f.table
        .get_mut(watcher)
        .expect("the watcher is live")
        .off_heap_mut()
        .push_binary(binary.clone());
    let tag = {
        let heap = f
            .table
            .get_mut(watcher)
            .expect("the watcher is live")
            .heap_mut();
        // SAFETY: the watcher's heap has the pair's words and `binary` (plus the watcher's own
        // off-heap table, seeded above) keeps the payload alive.
        unsafe { refc_pair(heap, &binary) }
    };
    let target = f.table.spawn_default().expect("a target process");
    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("a big-binary tag is accepted");
    f.table.terminate(target, boom).expect("the target dies");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, None, process, boom), target.to_eterm());
    let head = head_of(payload);
    // SAFETY: `head` is the live binary the delivered DOWN carries, owned by the watcher's heap.
    let (bytes, bits) = unsafe { binary_view(head) };
    assert_eq!(bits, 8000, "the 1000-byte tag arrives with all 8000 bits");
    assert_eq!(bytes, &[0x41u8; 1000][..], "and no byte of it changed");
    assert!(
        f.on_young_heap(watcher, head),
        "the big binary's `[BinRef][ErlSubBits]` pair is a word on the watcher's young heap"
    );
}


#[test]
fn a_heap_resident_tag_survives_the_watchers_own_garbage_collection() {
    // The whole point of the slot's own copy: the tag outlives a collection of the watcher's heap
    // while the monitor is still outstanding. The oracle's answer (its own `garbage_collect/0`
    // between the monitor and the target's death) is the same `=:=` identity.
    let expr = r#"
        T = [a, make_ref(), #{k => <<"v">>}],
        P = spawn(fun() -> receive go -> exit(boom) end end),
        _ = erlang:monitor(process, P, [{tag, T}]),
        Junk = [lists:seq(1, 50) || _ <- lists:seq(1, 100)],
        erlang:garbage_collect(),
        erlang:garbage_collect(),
        P ! go,
        M = receive X -> X after 2000 -> timeout end,
        io:format("~p/~p/~p~n", [element(1, M) =:= T, element(5, M), length(Junk)])
    "#;
    if !expect_oracle(expr, "true/boom/100") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let boom = f.atom_term("boom");
    let a = f.atom("a");
    let k = f.atom("k");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let embedded = MonitorRef::from_id(99);

    let tag = {
        let heap = f
            .table
            .get_mut(watcher)
            .expect("the watcher is live")
            .heap_mut();
        multi_call_tag(heap, &f.names, embedded, a, k)
    };
    let target = f.table.spawn_default().expect("a target process");
    let reference = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("the tag is accepted");
    // The monitor really is recorded with its tag pending: the slot is the tag's only owner while
    // the target is alive.
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .monitors()
        .find(reference)
        .is_some());

    // A rooted collection of the watcher's own heap. The original tag word is retired here, so a
    // tag stored as a raw heap word would dangle from now on; the slot's copy is not part of any
    // heap and is untouched.
    let minors = f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .collector()
        .minor_gcs();
    f.table
        .get_mut(watcher)
        .expect("the watcher is live")
        .collect()
        .expect("the watcher's own garbage collection runs");
    assert_eq!(
        f.table
            .get(watcher)
            .expect("the watcher is live")
            .collector()
            .minor_gcs(),
        minors + 1,
        "the fixture really collected the watcher's heap"
    );

    // Only now does the target die: the DOWN must still carry the tag, `=:=` the original.
    f.table.terminate(target, boom).expect("the target dies");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, None, process, boom), target.to_eterm());
    let head = head_of(payload);
    let mut arena = Heap::new(64).expect("the expectation arena allocates");
    let expected = multi_call_tag(&mut arena, &f.names, embedded, a, k);
    assert!(
        compare::eq(head, expected, &f.names),
        "the tag survives the watcher's collection `=:=` the original"
    );
    assert!(
        f.on_young_heap(watcher, head),
        "the delivered tag is a live word on the watcher's (post-collection) young heap"
    );
}

#[test]
fn a_born_down_noproc_down_carries_a_heap_resident_tag() {
    // The `noproc` arm of the same feature, for a pid target and for a registered name.
    let expr = r#"
        T = [a, make_ref(), #{k => <<"v">>}],
        {P, R0} = spawn_monitor(fun() -> ok end),
        receive {DOWN, R0, _, _, _} when is_atom(DOWN) -> ok end,
        _ = erlang:monitor(process, P, [{tag, T}]),
        M = receive X -> X after 2000 -> timeout end,
        io:format("~p/~p~n", [element(1, M) =:= T, element(5, M)])
    "#;
    if !expect_oracle(expr, "true/noproc") {
        return;
    }
    let named_expr = r#"
        T = {name_tag, make_ref()},
        _ = erlang:monitor(process, nobody_here, [{tag, T}]),
        M = receive X -> X after 2000 -> timeout end,
        io:format("~p/~p/~p~n",
                  [element(1, M) =:= T, element(4, M) =:= {nobody_here, node()},
                   element(5, M)])
    "#;
    if !expect_oracle(named_expr, "true/true/noproc") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let noproc = f.atom_term("noproc");
    let a = f.atom("a");
    let k = f.atom("k");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let embedded = MonitorRef::from_id(4711);

    // 1. A dead pid target.
    let tag = {
        let heap = f
            .table
            .get_mut(watcher)
            .expect("the watcher is live")
            .heap_mut();
        multi_call_tag(heap, &f.names, embedded, a, k)
    };
    let dead = {
        let dying = f.table.spawn_default().expect("a process about to die");
        f.table.remove(dying);
        dying
    };
    let reference = f
        .table
        .monitor_with_tag(watcher, dead, MonitorKind::Process, Some(tag))
        .expect("a non-live target is not an error");
    let mut arena = Heap::new(64).expect("the expectation arena allocates");
    let expected = multi_call_tag(&mut arena, &f.names, embedded, a, k);
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, None, process, noproc), dead.to_eterm());
    let head = head_of(payload);
    assert!(compare::eq(head, expected, &f.names), "the born-down tag is the tag");
    // The monitor was never recorded (nothing to watch), so the tag's slot was released with the
    // DOWN it produced.
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .monitors()
        .find(reference)
        .is_none());
    f.drain_one(watcher);

    // 2. A registered-name target that does not resolve.
    let name_tag = f.tuple_in(watcher, &[make_small(5), make_small(6)]);
    let nobody = f.atom("nobody_here");
    let node = f.atom("nonode@nohost");
    f.table
        .monitor_named_with_tag(watcher, nobody, node, MonitorKind::Process, Some(name_tag))
        .expect("an unregistered name is not an error");
    let expected_target = f.tuple_in(watcher, &[make_atom(nobody), make_atom(node)]);
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, None, process, noproc);
    let mut arena = Heap::new(8).expect("the expectation arena allocates");
    let expected_tag = arena
        .tuple(&[make_small(5), make_small(6)])
        .expect("the expected tuple fits");
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
    let head = head_of(payload);
    assert!(
        compare::eq(head, expected_tag, &f.names),
        "the named born-down DOWN carries the heap-resident tag"
    );
}

// ---------------------------------------------------------------------------
// The fragment-fallback route
// ---------------------------------------------------------------------------

#[test]
fn an_exempt_watcher_gets_a_tagged_down_through_a_message_fragment() {
    // A watcher whose heap must not move — **relocation-exempt** here, the host's own declaration
    // that it holds live terms outside the crate root set — cannot take the tag onto its heap, so
    // `deliver_down_fragment` builds the whole DOWN into the message's own heap fragment: the tag
    // still arrives, and the watcher's own collection cannot disturb it.
    let mut f = Fixture::new();
    let process = f.atom("process");
    let boom = f.atom_term("boom");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let tag = f.tuple_in(watcher, &[make_small(1), make_small(2), make_small(3)]);
    // One free word: neither the reason nor the tag can be materialised there.
    f.drain_nursery_to(watcher, 1);
    f.table
        .get_mut(watcher)
        .expect("the watcher is live")
        .set_relocation_exempt(true);
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .has_unpublished_roots());

    let used_before = f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .heap_used_words();
    let target = f.table.spawn_default().expect("a target process");
    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("the tag is accepted");
    f.table
        .terminate(target, boom)
        .expect("the DOWN is delivered fragment-resident");

    // No signal was queued: the DOWN could not live on the receiver's heap.
    assert_eq!(
        f.table
            .get(watcher)
            .expect("the watcher is live")
            .pending_signals(),
        0,
        "the tag could not be materialised, so no queued signal carries it"
    );
    // ... and not one word was written on the watcher's heap either.
    assert_eq!(
        f.table
            .get(watcher)
            .expect("the watcher is live")
            .heap_used_words(),
        used_before,
        "the fragment path writes nothing on the receiver's heap"
    );

    let payload = f.sole_payload(watcher);
    let message = f.message_at(watcher, 0);
    let fragments = message
        .fragments()
        .expect("the exempt fallback enqueued a fragment-resident message");
    assert_eq!(fragments.len(), 1, "one fragment carries the whole payload");
    let fragment = fragments.iter().next().expect("one fragment");
    assert!(
        fragment.contains(payload),
        "the DOWN root lives inside the message's fragment"
    );
    assert!(
        !f.on_young_heap(watcher, payload),
        "the DOWN is not a word on the watcher's heap"
    );

    // The tag rides the fragment, `=:=` the original, and inside the fragment rather than the heap.
    let head = head_of(payload);
    assert!(
        fragment.contains(head),
        "the tag lives inside the fragment, never on the watcher's heap"
    );
    assert!(
        !f.on_young_heap(watcher, head),
        "not one word of the tag was materialised on the heap"
    );
    let mut arena = Heap::new(8).expect("the expectation arena allocates");
    let expected_tag = arena
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the expected tuple fits");
    assert!(
        compare::eq(head, expected_tag, &f.names),
        "the fragment-resident tag is the caller's tag"
    );
    // SAFETY: the same live 5-tuple.
    unsafe {
        assert_eq!(tuple_element(payload, 2), make_atom(process));
        assert_eq!(tuple_element(payload, 4), boom);
        assert_eq!(tuple_element(payload, 3), target.to_eterm());
    }

    // The watcher's own collection (now permitted again) leaves the fragment — and therefore the
    // tag — exactly where it was: a fragment-rooted payload is foreign to the copier, which copies
    // it verbatim and never descends into it.
    f.table
        .get_mut(watcher)
        .expect("the watcher is live")
        .set_relocation_exempt(false);
    f.table
        .get_mut(watcher)
        .expect("the watcher is live")
        .collect()
        .expect("the watcher's own garbage collection runs");
    let after = f.sole_payload(watcher);
    assert_eq!(after, payload, "the fragment-resident DOWN did not move");
    let head_after = head_of(after);
    assert!(
        compare::eq(head_after, expected_tag, &f.names),
        "the watcher's garbage collection does not disturb a fragment-resident tag"
    );
}

// ---------------------------------------------------------------------------
// The two ordering invariants behind the two routes
// ---------------------------------------------------------------------------

#[test]
fn a_queued_down_tag_word_lives_on_the_receivers_own_heap() {
    // The queued `PendingSignal::Down`'s tag slot is an **in-heap sub-rootset** the collector
    // gathers and scatters, so the (fragment-backed) slot word must never reach it: the tag is
    // materialised onto the receiver's own heap first (`materialise_tag_onto`). This pins that
    // invariant on the only route that queues a tagged DOWN.
    let mut f = Fixture::new();
    let boom = f.atom_term("boom");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let tag = f.tuple_in(watcher, &[make_small(7), make_small(8)]);
    // A `Running` watcher's signals are not serviced inside the signalling operation, so the
    // DOWN stays queued and its words can be inspected directly.
    f.table
        .get_mut(watcher)
        .expect("the watcher is live")
        .set_state(ProcessState::Running);

    let target = f.table.spawn_default().expect("a target process");
    let reference = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("the tag is accepted");
    f.table.terminate(target, boom).expect("the target dies");

    assert_eq!(f.mailbox_len(watcher), 0, "a running watcher is not serviced");
    let process = f.table.get(watcher).expect("the watcher is live");
    let signal = process
        .signals()
        .peek()
        .expect("the DOWN is queued as a pending signal");
    let tag_word = signal
        .tag()
        .expect("the queued DOWN carries the monitor's user tag");
    assert!(
        !is_immediate(tag_word),
        "a heap-resident tag is never queued as its (fragment-resident) slot word"
    );
    assert_ne!(
        tag_word, tag,
        "the queued word is the receiver-heap copy, not the caller's word"
    );
    assert!(
        process.collector().young_contains(ptr_val(tag_word)),
        "the queued tag word is a word on the receiver's own young heap — the form its \
         gather/scatter roots require"
    );
    // The same for the reason the queue copied onto that heap.
    let reason = PendingSignal::reason(signal);
    assert!(
        !is_immediate(reason) || reason == boom,
        "the queued reason is the copied word"
    );
    assert_eq!(
        signal.kind(),
        PendingSignalKind::Down,
        "the queued signal is a monitor DOWN, not an exit"
    );
    let _ = reference;
}

#[test]
fn materialising_a_tag_grows_the_nursery_without_collecting() {
    // The queued-signal route copies the **reason** first and materialises the **tag** second. A
    // collection between the two would relocate the reason word the route is still holding, so the
    // tag's route may only **grow** the nursery (address-preserving) and re-copy — never collect.
    let mut f = Fixture::new();
    let process = f.atom("process");
    let watcher = f
        .table
        .spawn(HeapPolicy::default())
        .expect("a watcher process");
    // The reason is a boxed tuple small enough for the receiver's five free words — built on the
    // **dying target's** own heap, exactly where a real death's reason lives when the signal copies
    // it, so the fixture's reason can never dangle under the copy.
    let target = f.table.spawn_default().expect("a target process");
    let reason = f.tuple_in(target, &[make_small(11), make_small(12)]);
    // A tag far larger than the reason, so the nursery takes the reason and refuses the tag.
    let tag_elems: Vec<Eterm> = (0..39).map(|i| make_small(i as i64)).collect();
    let tag = f.tuple_in(watcher, &tag_elems);
    f.drain_nursery_to(watcher, 5);

    let capacity_before = f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .collector()
        .young()
        .capacity_words();
    let minors_before = f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .collector()
        .minor_gcs();

    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("the tag is accepted");
    f.table.terminate(target, reason).expect("the target dies");

    // The tag's materialisation grew the nursery in place ...
    assert!(
        f.table
            .get(watcher)
            .expect("the watcher is live")
            .collector()
            .young()
            .capacity_words()
            > capacity_before,
        "the tag's refusal grew the nursery instead of failing the DOWN"
    );
    // ... and ran **no** collection, so the reason word it had already copied is still valid.
    assert_eq!(
        f.table
            .get(watcher)
            .expect("the watcher is live")
            .collector()
            .minor_gcs(),
        minors_before,
        "no collection may run between the reason copy and the tag copy"
    );

    // Both halves of the DOWN are correct, `=:=` the inputs — the reason did not dangle. The
    // delivered reason is a *copy* of the dying target's boxed tuple, so it is compared
    // structurally against a rebuilt expectation (word equality would only work for an immediate).
    let payload = f.sole_payload(watcher);
    let mut reason_arena = Heap::new(8).expect("the reason expectation arena allocates");
    let expected_reason = reason_arena
        .tuple(&[make_small(11), make_small(12)])
        .expect("the expected reason tuple fits");
    // SAFETY: the live DOWN tuple; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5, "a DOWN message is a 5-tuple");
        assert!(
            is_reference(tuple_element(payload, 1)),
            "element 1 is a reference"
        );
        assert_eq!(tuple_element(payload, 2), make_atom(process));
        assert_eq!(tuple_element(payload, 3), target.to_eterm());
        assert!(
            compare::eq(tuple_element(payload, 4), expected_reason, &f.names),
            "the delivered reason is the dying target's own tuple, copied `=:=` without dangling"
        );
    }
    let head = head_of(payload);
    let mut arena = Heap::new(48).expect("the expectation arena allocates");
    let expected_tag = arena
        .tuple(&tag_elems)
        .expect("the expected tag tuple fits");
    assert!(compare::eq(head, expected_tag, &f.names), "the tag is the tag");
    assert!(
        f.on_young_heap(watcher, head),
        "the tag was materialised onto the watcher's own heap"
    );
}

// ---------------------------------------------------------------------------
// Named targets
// ---------------------------------------------------------------------------

#[test]
fn a_tagged_named_monitor_reports_the_name_node_target() {
    // A named monitor's DOWN target is `{Name, Node}` rather than the pid the name resolved to;
    // a tag replaces only the head atom.
    let expr = r#"
        P = spawn(fun() -> receive go -> ok end end),
        true = register(my_tag_server, P),
        _ = erlang:monitor(process, my_tag_server, [{tag, mytag}]),
        P ! go,
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, T =:= {my_tag_server, node()}, A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,process,true,normal}") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let name = f.atom("my_tag_server");
    let node = f.atom("nonode@nohost");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");

    // 1. The registered name, with an immediate tag.
    let target = f.table.spawn_default().expect("a target process");
    f.table.register(name, target).expect("the name is free");
    f.table
        .monitor_named_with_tag(
            watcher,
            name,
            node,
            MonitorKind::Process,
            Some(make_atom(mytag)),
        )
        .expect("the name resolves to a live process");
    f.table.terminate(target, normal).expect("the target is live");

    // The DOWN target is the `{Name, Node}` pair materialised on the watcher's heap.
    let expected_target = f.tuple_in(watcher, &[make_atom(name), make_atom(node)]);
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, Some(make_atom(mytag)), process, normal);
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
    f.drain_one(watcher);

    // 2. The unregistered-name form is likewise tagged.
    let nobody = f.atom("nobody_here");
    f.table
        .monitor_named_with_tag(
            watcher,
            nobody,
            node,
            MonitorKind::Process,
            Some(make_atom(mytag)),
        )
        .expect("an unregistered name is not an error");
    let expected_target = f.tuple_in(watcher, &[make_atom(nobody), make_atom(node)]);
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, Some(make_atom(mytag)), process, f.atom_term("noproc"));
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
    f.drain_one(watcher);

    // 3. The same named monitor with a **heap-resident** tag: the tag is the slot's own copy and
    //    the target is still the `{Name, Node}` pair (the two side tables do not interfere).
    let other = f.atom("another_tag_server");
    let target = f.table.spawn_default().expect("a target process");
    f.table.register(other, target).expect("the name is free");
    let heap_tag = f.tuple_in(watcher, &[make_small(31), make_small(32)]);
    f.table
        .monitor_named_with_tag(
            watcher,
            other,
            node,
            MonitorKind::Process,
            Some(heap_tag),
        )
        .expect("a heap-resident tag on a named monitor is accepted");
    f.table.terminate(target, normal).expect("the target is live");
    let expected_target = f.tuple_in(watcher, &[make_atom(other), make_atom(node)]);
    let mut arena = Heap::new(8).expect("the expectation arena allocates");
    let expected_tag = arena
        .tuple(&[make_small(31), make_small(32)])
        .expect("the expected tuple fits");
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, None, process, normal);
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
    assert!(
        compare::eq(head_of(payload), expected_tag, &f.names),
        "the named DOWN's head is the heap-resident tag"
    );
}

#[test]
fn the_oracle_tags_a_noproc_named_down() {
    let expr = r#"
        _ = erlang:monitor(process, nobody_here, [{tag, mytag}]),
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, T =:= {nobody_here, node()}, A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,process,true,noproc}") {
        return;
    }

    // The same observable through the table: the unregistered name is born down, tagged.
    let mut f = Fixture::new();
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let nobody = f.atom("nobody_here");
    let node = f.atom("nonode@nohost");
    let watcher = f.table.spawn_default().expect("a watcher process");

    f.table
        .monitor_named_with_tag(
            watcher,
            nobody,
            node,
            MonitorKind::Process,
            Some(make_atom(mytag)),
        )
        .expect("an unregistered name is not an error");
    let expected_target = f.tuple_in(watcher, &[make_atom(nobody), make_atom(node)]);
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, Some(make_atom(mytag)), process, f.atom_term("noproc"));
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
}

// ---------------------------------------------------------------------------
// Port targets
// ---------------------------------------------------------------------------

#[test]
fn a_tagged_port_monitor_delivers_the_tagged_port_down_and_the_untagged_one_is_unchanged() {
    // The port analogue: the kind slot carries the `port` atom and the target is the port
    // immediate. `port_close/1` from the owning process queues an `{'EXIT', Port, normal}` first
    // (the owner link), so the expressions pattern-match the 5-tuple DOWN specifically.
    let expr = r#"
        process_flag(trap_exit, true),
        F = fun(Opts) ->
              Port = open_port({spawn, "cat"}, []),
              _ = erlang:monitor(port, Port, Opts),
              port_close(Port),
              receive {H, R, K, T, A} -> {H, is_reference(R), K, is_port(T), A}
              after 2000 -> timeout end
            end,
        io:format("~p|~p~n", [F([{tag, mytag}]), F([])])
    "#;
    if !expect_oracle(
        expr,
        "{mytag,true,port,true,normal}|{'DOWN',true,port,true,normal}",
    ) {
        return;
    }

    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let port_atom = f.atom("port");
    let mytag = f.atom("mytag");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();

    // 1. Tagged, through the typed `PortId` entry point.
    let port = ports.alloc().expect("room in the number space");
    let reference = f
        .table
        .monitor_port_with_tag(watcher, port, &mut ports, Some(make_atom(mytag)))
        .expect("a live port monitors");
    assert_eq!(
        f.table.get(watcher).expect("watcher").port_monitors(),
        &[PortWatch { reference, port }],
        "the outgoing half is recorded on the watcher"
    );
    f.table
        .port_closed(&mut ports, port, normal)
        .expect("the port is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(mytag)), port_atom, normal),
        port.to_eterm(),
    );
    assert!(
        f.table
            .get(watcher)
            .expect("watcher")
            .port_monitors()
            .is_empty(),
        "the port's death resolved the outgoing half"
    );
    f.drain_one(watcher);

    // 2. Untagged, through the term entry point: the default `'DOWN'` head, unchanged.
    let port = ports.alloc().expect("room in the number space");
    f.table
        .monitor_port_term(watcher, port.to_eterm(), &mut ports)
        .expect("a live port monitors");
    f.table
        .port_closed(&mut ports, port, normal)
        .expect("the port is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(down)), port_atom, normal),
        port.to_eterm(),
    );
    f.drain_one(watcher);

    // 3. Tagged, through the term entry point.
    let port = ports.alloc().expect("room in the number space");
    f.table
        .monitor_port_term_with_tag(watcher, port.to_eterm(), &mut ports, Some(make_atom(mytag)))
        .expect("a live port monitors");
    f.table
        .port_closed(&mut ports, port, normal)
        .expect("the port is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(mytag)), port_atom, normal),
        port.to_eterm(),
    );
    f.drain_one(watcher);

    // 4. Tagged with a **heap-resident** tag: the port DOWN takes the same tagged route.
    let port = ports.alloc().expect("room in the number space");
    let heap_tag = f.tuple_in(watcher, &[make_small(41), make_small(42)]);
    f.table
        .monitor_port_with_tag(watcher, port, &mut ports, Some(heap_tag))
        .expect("a heap-resident tag on a port monitor is accepted");
    f.table
        .port_closed(&mut ports, port, normal)
        .expect("the port is live");
    let mut arena = Heap::new(8).expect("the expectation arena allocates");
    let expected_tag = arena
        .tuple(&[make_small(41), make_small(42)])
        .expect("the expected tuple fits");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, None, port_atom, normal),
        port.to_eterm(),
    );
    assert!(
        compare::eq(head_of(payload), expected_tag, &f.names),
        "the port DOWN's head is the heap-resident tag"
    );
    assert!(
        f.table
            .get(watcher)
            .expect("watcher")
            .port_monitors()
            .is_empty(),
        "the port's death resolved the outgoing half"
    );
}

#[test]
fn a_tagged_monitor_of_a_closed_port_is_born_down_tagged() {
    let expr = r#"
        process_flag(trap_exit, true),
        Port = open_port({spawn, "cat"}, []),
        true = port_close(Port),
        receive _ -> ok end,
        _ = erlang:monitor(port, Port, [{tag, mytag}]),
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, is_port(T), A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,port,true,noproc}") {
        return;
    }

    let mut f = Fixture::new();
    let port_atom = f.atom("port");
    let mytag = f.atom("mytag");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();
    let port = ports.alloc().expect("room in the number space");
    // The port is already gone: the number was handed back, so nothing is watching it.
    ports.release(port);

    let reference = f
        .table
        .monitor_port_with_tag(watcher, port, &mut ports, Some(make_atom(mytag)))
        .expect("a non-live port is not an error");
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .port_monitors()
        .is_empty());
    // Not a recorded monitor, so it is not removable as one (`false`, not an error).
    assert!(!f
        .table
        .demonitor_port(watcher, reference, &mut ports)
        .expect("the watcher is live"));

    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, Some(make_atom(mytag)), port_atom, noproc),
        port.to_eterm(),
    );
}

// ---------------------------------------------------------------------------
// Flushing a tagged DOWN (head-agnostic, whatever the tag's shape)
// ---------------------------------------------------------------------------

#[test]
fn a_tagged_down_with_a_heap_resident_tag_is_flushed_by_demonitor() {
    // `erlang:demonitor/2`'s `[flush]` is head-agnostic (the C's
    // `erts_internal:flush_monitor_messages/3` pattern `{_, Ref, _, _, _}`), so it removes a
    // tagged DOWN whatever the tag's shape — including the fragment-backed kind this change adds.
    // `tests/monitor_flush.rs` is the suite for the head rule itself; this pins the new tag kind.
    let expr = r#"
        Q = fun() -> element(2, erlang:process_info(self(), message_queue_len)) end,
        T = [a, make_ref(), #{k => <<"v">>}],
        P = spawn(fun() -> receive go -> ok end end),
        R = erlang:monitor(process, P, [{tag, T}]),
        P ! go, timer:sleep(150),
        B = Q(), D = erlang:demonitor(R, [flush]), A = Q(),
        io:format("~p/~p/~p~n", [B, D, A])
    "#;
    if !expect_oracle(expr, "1/true/0") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let a = f.atom("a");
    let k = f.atom("k");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let embedded = MonitorRef::from_id(77);
    let tag = {
        let heap = f
            .table
            .get_mut(watcher)
            .expect("the watcher is live")
            .heap_mut();
        multi_call_tag(heap, &f.names, embedded, a, k)
    };
    let target = f.table.spawn_default().expect("a target process");
    let reference = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("the tag is accepted");
    f.table.terminate(target, normal).expect("the target dies");
    assert_eq!(f.mailbox_len(watcher), 1, "the tagged DOWN is queued");
    let payload = f.sole_payload(watcher);
    // SAFETY: the live DOWN tuple; elements 2 and 4 are in bounds.
    unsafe {
        assert_eq!(tuple_element(payload, 2), make_atom(process));
        assert_eq!(tuple_element(payload, 4), normal);
    }
    assert!(
        f.table
            .demonitor_flush(watcher, reference)
            .expect("the watcher is live"),
        "the monitor still existed under that reference"
    );
    assert_eq!(
        f.mailbox_len(watcher),
        0,
        "the flushed head-agnostic match removed the tagged DOWN"
    );
}

// ---------------------------------------------------------------------------
// Literal-area tags are accepted (and allocate nothing)
// ---------------------------------------------------------------------------

/// Builds a genuine **literal-area** 1-tuple `{value}` inside `area` and returns its root word.
///
/// `area` is a real [`LiteralArea`] — a bump-allocated block outside every process heap, never
/// reallocated — and the root word is tagged with `TAG_LITERAL_PTR` by the C's own walk
/// ([`set_literal_tag`]), exactly as a loaded module's literal is. The caller must keep `area`
/// alive for as long as the word is used.
fn literal_tuple(area: &mut LiteralArea, value: Eterm) -> Eterm {
    // SAFETY: two uninitialised words are requested and both are written immediately below,
    // before anything is read.
    let words = unsafe { area.alloc_words(2) }.expect("the literal area has room");
    // SAFETY: `words[0..2]` were just handed out exclusively by `alloc_words` and both are
    // written here — an arityval header followed by the single element — so nothing is read
    // uninitialised, and the words live as long as `area`.
    unsafe {
        words.write(make_arityval(1));
        words.add(1).write(value);
    }
    let mut term = make_tuple(words);
    // SAFETY: `words` addresses the two live, initialised words just written inside `area`, and
    // `&mut term` is a live, aligned, writable root word; the walk only ORs `TAG_LITERAL_PTR`
    // into BOXED/LIST words, leaving the transparent arityval and the immediate element intact.
    unsafe { set_literal_tag(&mut term, words, 2) };
    term
}

/// Builds a genuine literal-area cons cell `[value | []]` inside `area` and returns its root
/// word (the `TAG_PRIMARY_LIST` arm of the same rule).
fn literal_cons(area: &mut LiteralArea, value: Eterm) -> Eterm {
    // SAFETY: two uninitialised words are requested and both are written immediately below,
    // before anything is read.
    let words = unsafe { area.alloc_words(2) }.expect("the literal area has room");
    // SAFETY: the two words were just handed out exclusively and are both written here (the CAR
    // and the NIL CDR), and they live as long as `area`.
    unsafe {
        words.write(value);
        words.add(1).write(NIL);
    }
    let mut term = make_list(words);
    // SAFETY: `words` addresses the two live, initialised cons-cell words inside `area` and
    // `&mut term` is a live, aligned, writable root word, so the walk may tag both.
    unsafe { set_literal_tag(&mut term, words, 2) };
    term
}

#[test]
fn a_literal_area_tag_is_accepted_end_to_end() {
    // No oracle expression exists for this case: a literal area is a *runtime-internal*
    // representation (there is no Erlang source form for "this term lives in the frozen literal
    // block"), and the oracle cannot distinguish it from the boxed `{a}` tag that
    // `a_heap_resident_tag_reaches_the_notification_in_every_shape` already cross-checks. What
    // matters here is the *storage* rule: a shared, never-relocated term needs no copy at all
    // (`tag.rs`'s `a_literal_area_tag_is_stored_verbatim_and_allocates_nothing` pins the
    // no-fragment half at the slot), so it must reach the notification unchanged.
    let mut f = Fixture::new();
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");

    let mut area = LiteralArea::new(8);
    let tag = literal_tuple(&mut area, make_small(1234));
    assert!(
        is_literal(tag),
        "the fixture really is a literal-area term (`TAG_LITERAL_PTR` set)"
    );
    assert!(
        !is_immediate(tag),
        "and it is genuinely a boxed term, not an immediate"
    );

    let target = f.table.spawn_default().expect("a target process");
    let reference = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
        .expect("a literal-area tag is heap-independent and accepted");
    f.table.terminate(target, normal).expect("the target is live");

    let payload = f.sole_payload(watcher);
    // SAFETY: the live DOWN tuple the table built on the watcher's heap; all five element slots
    // are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5, "a DOWN message is a 5-tuple");
        assert_eq!(
            tuple_element(payload, 0),
            tag,
            "the notification head is the literal-area tag, not the default `'DOWN'` atom"
        );
        assert!(
            is_literal(tuple_element(payload, 0)),
            "the head word carries the literal-area bit, so it is never relocated"
        );
        assert_eq!(
            MonitorRef::from_eterm(tuple_element(payload, 1)).expect("element 1 is a reference"),
            reference,
            "the reference slot is unchanged by the tagged form"
        );
        assert_eq!(tuple_element(payload, 2), make_atom(process), "the kind slot");
        assert_eq!(tuple_element(payload, 3), target.to_eterm(), "the target slot");
        assert_eq!(tuple_element(payload, 4), normal, "the reason slot");
    }
    f.drain_one(watcher);

    // A literal-area *list* tag takes the same route (the predicate's `LIST` arm: a cons cell is
    // a `TAG_PRIMARY_LIST` word, which `is_boxed_term` does not match).
    let list_tag = literal_cons(&mut area, make_small(7));
    assert!(
        is_list(list_tag) && is_literal(list_tag),
        "a literal-area cons cell"
    );

    let target = f.table.spawn_default().expect("a second target");
    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(list_tag))
        .expect("a literal-area list tag is accepted too");
    f.table.terminate(target, normal).expect("the target is live");
    let payload = f.sole_payload(watcher);
    // SAFETY: the live DOWN tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(
            tuple_element(payload, 0),
            list_tag,
            "the head is the literal-area cons cell"
        );
        assert_eq!(tuple_element(payload, 4), normal);
    }
}
