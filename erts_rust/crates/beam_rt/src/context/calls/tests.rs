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

//! Units for the copy-out's **retry policy** (see
//! [`RuntimeContext::move_result_into_process_heap`]): the growth hint recovered from the
//! copier's own refusal, and the gate that keeps a *permanent* refusal from growing the
//! destination at all. The copy itself is exercised end to end by `tests/perf_call.rs`
//! (path B copies a container result on every iteration) and the `src/context/tests.rs`
//! registered-BIF suites.

use beam_common::{AllocFailure, AllocKind};
use beam_process::SignalAtoms;
use beam_term::make_small;
use beam_term::prelude::make_header;
use super::*;

/// The number of `Eterm`s a byte figure converts back to.
const WORD: usize = core::mem::size_of::<Eterm>();

/// A `runtime` + a caller-owned `ProcessTable` holding one default process, plus the pid —
/// the `for_process` shape, i.e. the one where the copy-out (and its retry) is ON.
fn process_table(rt: &mut crate::runtime::Runtime) -> (ProcessTable, beam_process::ProcessId) {
    let signals = SignalAtoms::from_table(rt.code_server_atoms_mut());
    let mut table = ProcessTable::new(signals);
    let pid = table.spawn_default().expect("a default process");
    (table, pid)
}

#[test]
fn the_growth_hint_is_the_copiers_own_shortfall_in_words() {
    // `beam_message`'s exhaustion error carries `words * size_of::<Eterm>()` in
    // `AllocFailure::requested` (its documented contract), so dividing recovers exactly the
    // flat word count the refused copy wanted — with no second traversal of the graph.
    let exhaustion = BeamError::Alloc(AllocFailure::new(AllocKind::Message, 37 * WORD));
    assert_eq!(
        copy_shortfall_words(&exhaustion),
        37,
        "the copier's own word count is recovered verbatim"
    );
    // A refusal of any other class reports no room request: the conservative `0` (the
    // caller's `max(256)` floor turns it into a legal growth hint).
    assert_eq!(copy_shortfall_words(&BeamError::BadArg), 0);
    assert_eq!(copy_shortfall_words(&BeamError::BadArity), 0);
    assert_eq!(
        copy_shortfall_words(&BeamError::Alloc(AllocFailure::new(AllocKind::Heap, 9 * WORD))),
        9,
        "any `Alloc` refusal carries its own requested byte count"
    );
}

#[test]
fn a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let used_before = table.get(pid).expect("the spawned process").heap().used();
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        // A bare header word is not a standing term at all (`make_header(0, 0)` is the C
        // ABI's `THE_NON_VALUE`): `copy_struct` answers the copier's typed malformed-graph
        // `badarg` — `BeamError::BadArg`, **not** a message-heap exhaustion. Growing the
        // destination cannot make it succeed, so the retry must not run: no space is
        // retired, no word is written, and the caller gets `None` (so `call_registered`
        // leaves both the result and the pooled BIF heap exactly as they were).
        ctx.move_result_into_process_heap(make_header(0, 0))
    };
    assert!(
        copy.is_none(),
        "a malformed graph cannot be re-homed; the caller must leave the result untouched"
    );
    let process = table.get(pid).expect("the spawned process");
    assert_eq!(
        process.collector().young_space_count(),
        spaces_before,
        "a permanent refusal must not retire a nursery space (the item-4 growth is per-call)"
    );
    assert_eq!(
        process.heap().used(),
        used_before,
        "a refused copy writes no destination word"
    );
}

#[test]
fn an_exhaustion_refusal_grows_the_nursery_and_the_retry_copies() {
    // The positive control for the gate above: a **valid** container the nursery cannot hold
    // still grows the destination by the copier's own word count and is copied on the retry.
    // `lists:seq/2`'s 4000-odd-word result does not fit the default 233-word nursery.
    use beam_engine::ExecutionContext;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let copied = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(2000)])
            .expect("lists:seq(1, 2000)")
    };
    assert!(
        beam_term::prelude::is_list(copied) && !beam_term::literal::is_literal(copied),
        "the result is a real list built on the process's own heap"
    );
    assert!(
        table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young_space_count()
            > spaces_before,
        "an exhaustion refusal grows the destination (the retry then fits)"
    );
    // The successful retry's destination was **empty** as handed to the copier (fast path
    // eligible) — `lists:seq/2` carries no off-heap reference, and the retry builds its own
    // fresh table, so nothing from the refused attempt can have poisoned it.
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        (attempts, grows),
        (2, 1),
        "one refused attempt, one growth, one retry"
    );
    assert_eq!(
        empties, attempts,
        "every attempt — including the retry — was offered a fully empty destination"
    );
}

/// The copy-out's destination off-heap table is **fresh and empty per attempt**, so
/// `beam_message`'s copier takes its single-traversal fast path even when the runtime's
/// escaped-payload store ([`crate::runtime::Runtime::eval_off_heap`]) is already populated by an
/// unrelated nested run or transport copy.
///
/// **This test fails on the pre-fix code**: with the shared `eval_off_heap` as the copier's
/// destination, the node entry pushed below makes the table non-empty for *both* attempts, so
/// `empty_destination_attempts` reads `0` instead of the `2` asserted here — exactly the
/// permanent poisoning the fresh local table removes.
#[test]
fn the_copy_out_always_hands_the_copier_an_empty_destination_table() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // Model a runtime whose store a prior nested run / transport copy has already populated:
    // one node entry, exactly what a copy that passed an external pid would have registered.
    let node = {
        let atoms = rt.code_server_atoms_mut();
        beam_term::prelude::make_atom(atoms.intern("nonode@nohost"))
    };
    rt.eval_off_heap
        .push_node(beam_message::NodeEntry::new(node, 0));
    // A **foreign** container — a term built on a heap the process's collector does not scan —
    // that fits the default nursery by a wide margin (a 2-tuple is 3 words), held in a local so
    // it outlives both copies.
    let mut foreign = beam_alloc::Heap::new(16).expect("a local heap");
    let term = foreign
        .tuple(&[make_small(1), make_small(2)])
        .expect("the tuple fits");
    let used_before = table.get(pid).expect("the spawned process").heap().used();
    notes::reset();
    let (first, second) = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        (
            ctx.move_result_into_process_heap(term),
            ctx.move_result_into_process_heap(term),
        )
    };
    assert!(
        first.is_some() && second.is_some(),
        "both copies fit the nursery"
    );
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        empties, 2,
        "each copy was handed a fully empty destination table (the fast path was eligible)"
    );
    assert_eq!(attempts, 2, "one `copy_struct` call per copy-out");
    assert_eq!(grows, 0, "a 3-word tuple never has to grow the nursery");
    let copied_words = table.get(pid).expect("the spawned process").heap().used() - used_before;
    assert_eq!(
        copied_words, 6,
        "two 3-word tuples land on the process's own heap"
    );
}

/// A **refused** copy adopts nothing into the runtime-lifetime store: a malformed-graph refusal
/// leaves [`crate::runtime::Runtime::eval_off_heap`] exactly as it was — its pre-existing entry
/// intact, no entry added — because the destination table is local and is merely dropped.
#[test]
fn a_refused_copy_adopts_nothing_into_the_runtime_store() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // A pre-existing entry models a store a prior nested run populated: the refusal must not
    // disturb it, and must add nothing of its own.
    let node = {
        let atoms = rt.code_server_atoms_mut();
        beam_term::prelude::make_atom(atoms.intern("nonode@nohost"))
    };
    rt.eval_off_heap
        .push_node(beam_message::NodeEntry::new(node, 0));
    let nodes_before = rt.eval_off_heap.nodes().len();
    let binaries_before = rt.eval_off_heap.binaries().len();
    notes::reset();
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        // The `make_header(0, 0)` malformed graph is refused **permanently** (see
        // `a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing`).
        ctx.move_result_into_process_heap(make_header(0, 0))
    };
    assert!(copy.is_none(), "the malformed graph is not re-homed");
    assert_eq!(
        rt.eval_off_heap.nodes().len(),
        nodes_before,
        "a refused copy adopts nothing (it also drops its own node references)"
    );
    assert_eq!(
        rt.eval_off_heap.binaries().len(),
        binaries_before,
        "a refused copy adopts no binary"
    );
    let (attempts, grows, _) = notes::snapshot();
    assert_eq!(attempts, 1, "the malformed graph is refused on the first attempt");
    assert_eq!(grows, 0, "a permanent refusal never grows the nursery");
}
