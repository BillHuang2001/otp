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

//! Pins for [`Machine::for_each_root`] — the read-only **pointer-bearing root
//! enumeration** a census or reachability walk consumes.
//!
//! The three properties, each its own test:
//!
//! * **Coverage** — every file [`Machine::with_roots`] hands over is walked (the used
//!   X-register prefix, the whole Erlang stack, the used FR prefix) plus the machine's
//!   five *loose* heap terms, which `with_roots` parks at the stack tail.
//! * **Selection** — only `LIST`/`BOXED` words are reported; small integers, atoms and
//!   `NIL` are skipped, because relocating an immediate is meaningless.
//! * **Agreement** — the word set the walk reports equals the pointer-bearing subset of
//!   the frozen [`RootSet`](crate::context::RootSet) `with_roots` hands over for the very
//!   same machine state, so the two doors can never disagree about *what* is rooted.
//!
//! Plus the re-seat pin: a `with_roots` hand-off whose visitor **moves** the parked words
//! leaves both supplied-trace latches holding the *moved* terms.
//!
//! Every term here is built in a [`MinimalContext`]'s region heap (a real boxed/cons
//! pointer) or is a real immediate, so the tag classification under test is the one a
//! live machine produces. The walk never dereferences: it hands back the words verbatim.

use beam_common::ExceptionClass;
use beam_term::{make_atom, make_small, primary_tag, Eterm, PrimaryTag, NIL};

use super::test_support::Fixture;
use super::Machine;
use crate::context::{MinimalContext, RootSet};
// The production term builders take the machine plus a reader closure (so a moving
// collector can relocate operands between the reserve and the read); these tests build
// from already-materialized terms against a non-collecting arena, so alias `terms` to the
// test-only wrappers that keep the older, ergonomic arity.
use crate::terms::test_support as terms;

/// Every word that can hold a heap pointer, in a term-independent expression of the rule
/// [`Machine::for_each_root`] applies: a `LIST` (cons) or `BOXED` primary tag, and nothing
/// else.
///
/// Deliberately written out here rather than reusing the machine's own predicate, so the
/// agreement test compares two independent expressions of the rule instead of comparing a
/// function with itself.
fn pointer_bearing(word: Eterm) -> bool {
    matches!(primary_tag(word), PrimaryTag::List | PrimaryTag::Boxed)
}

/// The sorted raw words of `terms` — the comparable form of a *set* of root words
/// (duplicates included, since a term may legitimately appear in two roots).
fn raws(terms: impl IntoIterator<Item = Eterm>) -> Vec<usize> {
    let mut raws: Vec<usize> = terms.into_iter().map(|term| term.raw()).collect();
    raws.sort_unstable();
    raws
}

/// The machine state the coverage and agreement tests share.
///
/// * `x(0)` a boxed tuple, `x(1)` a small integer, `x(2)` a cons cell;
/// * a 3-Y-slot frame holding `y(0)` a boxed tuple, `y(1)` `NIL`, `y(2)` an atom;
/// * `fr(0)` a boxed tuple;
/// * the two failed-clause latch slots, the pending exception's reason and the two
///   supplied-trace **term** latches all armed (a boxed/cons term each way), which is
///   the state `with_roots` parks at the stack tail.
///
/// Returns the machine and the nine pointer-bearing words it roots, in the order
/// [`Machine::for_each_root`] documents (`x` prefix, stack, `fr` prefix, loose terms —
/// [`Machine::loose_heap_terms`] order).
fn roots_fixture() -> (Machine, Vec<Eterm>) {
    let mut ctx = MinimalContext::new();
    let x0 = terms::make_tuple(&mut ctx, &[make_small(1)]).expect("a boxed tuple");
    let x2 = terms::make_list(&mut ctx, &[make_small(2)]).expect("a cons cell");
    let y0 = terms::make_tuple(&mut ctx, &[make_small(3)]).expect("a boxed tuple");
    let fr0 = terms::make_tuple(&mut ctx, &[make_small(4)]).expect("a boxed tuple");
    let loose_raise = terms::make_tuple(&mut ctx, &[make_small(5)]).expect("a boxed tuple");
    let missing_args = terms::make_tuple(&mut ctx, &[make_small(6)]).expect("a boxed tuple");
    let reason = terms::make_list(&mut ctx, &[make_small(7)]).expect("a cons cell");
    let pending_trace = terms::make_list(&mut ctx, &[make_small(8)]).expect("a cons cell");
    let live_trace = terms::make_tuple(&mut ctx, &[make_small(9)]).expect("a boxed tuple");

    let mut fixture = Fixture::new("m");
    let an_atom = make_atom(fixture.atom("an_atom"));

    let mut machine = Machine::new();
    machine.set_x(0, x0).expect("x(0) is in range");
    machine.set_x(1, make_small(7)).expect("x(1) is in range");
    machine.set_x(2, x2).expect("x(2) is in range");
    machine.push_frame(3).expect("a 3-slot frame fits the stack");
    machine.set_y(0, y0).expect("y(0) is inside the frame");
    machine.set_y(1, NIL).expect("y(1) is inside the frame");
    machine.set_y(2, an_atom).expect("y(2) is inside the frame");
    machine.set_fr(0, fr0).expect("fr(0) is in range");
    machine.raise_function_clause_args = Some(loose_raise);
    machine.pending_function_clause_args = Some(missing_args);
    machine.pending_exception = Some((ExceptionClass::Error, reason));
    machine.pending_supplied_term = Some(pending_trace);
    machine.raise_supplied_term = Some(live_trace);

    (
        machine,
        vec![
            x0,
            x2,
            y0,
            fr0,
            loose_raise,
            missing_args,
            reason,
            pending_trace,
            live_trace,
        ],
    )
}

#[test]
fn for_each_root_enumerates_registers_stack_frames_and_the_loose_terms() {
    let (machine, expected) = roots_fixture();
    let mut seen = Vec::new();
    machine.for_each_root(|term| seen.push(term));
    assert_eq!(
        seen, expected,
        "every rooted file and all five loose terms, in the documented order"
    );
    assert_eq!(
        seen.len(),
        9,
        "two x registers, one Y slot, one FR slot, five loose"
    );
}

#[test]
fn for_each_root_skips_immediates() {
    // A machine whose every rooted slot holds an immediate: nothing may be reported, since
    // none of those words can name a heap object (and a collector relocating one would
    // corrupt it).
    let mut fixture = Fixture::new("m");
    let an_atom = make_atom(fixture.atom("an_atom"));
    let mut machine = Machine::new();
    for n in 0..4 {
        machine
            .set_x(n, make_small(i64::from(n)))
            .expect("the x registers are in range");
    }
    machine.push_frame(2).expect("a 2-slot frame fits the stack");
    machine
        .set_y(0, an_atom)
        .expect("y(0) is inside the frame");
    machine.set_y(1, NIL).expect("y(1) is inside the frame");
    machine.set_fr(0, NIL).expect("fr(0) is in range");

    let mut seen = Vec::new();
    machine.for_each_root(|term| seen.push(term));
    assert!(seen.is_empty(), "immediates are not roots, got {seen:?}");

    // One boxed word in an otherwise immediate-only machine is the *only* thing reported,
    // and it arrives verbatim (the walk copies the word, it does not rebuild the term).
    let mut ctx = MinimalContext::new();
    let tuple = terms::make_tuple(&mut ctx, &[make_small(9)]).expect("a boxed tuple");
    machine.set_x(4, tuple).expect("x(4) is in range");
    let mut seen = Vec::new();
    machine.for_each_root(|term| seen.push(term));
    assert_eq!(seen, vec![tuple]);
    assert!(pointer_bearing(tuple));
}

#[test]
fn for_each_root_agrees_with_with_roots() {
    let (mut machine, _) = roots_fixture();

    let mut walked = Vec::new();
    machine.for_each_root(|term| walked.push(term));

    // The frozen door's own hand-off: the same three files, `with_roots` having parked the
    // loose terms at the stack tail so the `RootSet` carries them. Collecting the
    // pointer-bearing words from the handed slices is what a collecting host walks.
    let mut rooted = Vec::new();
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        for &word in roots
            .x
            .iter()
            .chain(roots.stack.iter())
            .chain(roots.fr.iter())
        {
            if pointer_bearing(word) {
                rooted.push(word);
            }
        }
    });

    assert!(!walked.is_empty(), "the fixture roots something");
    assert_eq!(
        raws(walked),
        raws(rooted),
        "for_each_root must report exactly the pointer-bearing words with_roots hands over"
    );
}

/// The two supplied-trace **term** latches ([`Machine::pending_supplied_term`] /
/// [`Machine::raise_supplied_term`]) hold a cooked `[{M,F,A,Loc}]` cons list while a
/// re-raise is in flight, and that field is **outside** the frozen
/// [`RootSet`](crate::context::RootSet). [`Machine::with_roots`] must therefore park
/// both at the stack tail and **re-seat each field from the word the host writes back**,
/// so a collection relocates the list instead of stranding it.
///
/// The visitor here plays a moving collector: it rewrites every parked occurrence of a
/// term with a freshly built, distinguishable one. Re-seating is the only way the fields
/// can end up holding the *moved* words, so this fails on any build where the pair is
/// left out of [`Machine::loose_heap_terms`].
#[test]
fn with_roots_re_seats_the_two_supplied_trace_term_latches_from_their_parked_slots() {
    let mut ctx = MinimalContext::new();
    // What the latches hold: cons cells, so a "relocation" is observable (an immediate
    // could not be moved, and a wrong re-seat would be invisible).
    let pending = terms::make_list(&mut ctx, &[make_small(11)]).expect("a cons cell");
    let live = terms::make_list(&mut ctx, &[make_small(12)]).expect("a cons cell");
    assert!(
        pointer_bearing(pending) && pointer_bearing(live),
        "the latches must hold pointer-bearing terms for the move to be observable"
    );
    // Where they "move" to.
    let moved_pending = terms::make_tuple(&mut ctx, &[make_small(13)]).expect("a boxed tuple");
    let moved_live = terms::make_tuple(&mut ctx, &[make_small(14)]).expect("a boxed tuple");

    let mut machine = Machine::new();
    machine.push_frame(1).expect("a 1-slot frame fits the stack");
    machine.set_y(0, NIL).expect("y(0) is inside the frame");
    machine.pending_supplied_term = Some(pending);
    machine.raise_supplied_term = Some(live);
    let stack_before = machine.stack_mut().len();

    let mut parked_loose = 0;
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        // Both latches ride the frozen root set: they are in the stack slice the host is
        // handed, which is what lets a conforming collector relocate them.
        assert!(
            roots.stack.iter().any(|word| *word == pending),
            "the pending supplied-trace term must be parked in the handed-over stack"
        );
        assert!(
            roots.stack.iter().any(|word| *word == live),
            "the live supplied-trace term must be parked in the handed-over stack"
        );
        for slot in roots.stack.iter_mut() {
            if *slot == pending {
                *slot = moved_pending;
                parked_loose += 1;
            } else if *slot == live {
                *slot = moved_live;
                parked_loose += 1;
            }
        }
    });

    assert_eq!(parked_loose, 2, "both supplied-trace terms were parked");
    assert_eq!(
        machine.pending_supplied_term,
        Some(moved_pending),
        "the one-shot latch must be re-seated from its parking slot, not left on the \
         pre-hand-off word"
    );
    assert_eq!(
        machine.raise_supplied_term,
        Some(moved_live),
        "the live latch must be re-seated from its parking slot, not left on the \
         pre-hand-off word"
    );
    assert_eq!(
        machine.stack_mut().len(),
        stack_before,
        "the parked words must be truncated after the hand-off"
    );

    // The read-only walk enumerates the *moved* words (the re-seated fields), never the
    // abandoned ones, and it agrees with a second hand-off of the same machine state.
    let mut walked = Vec::new();
    machine.for_each_root(|term| walked.push(term));
    assert_eq!(
        walked,
        vec![moved_pending, moved_live],
        "for_each_root must report exactly the two re-seated latches (y(0) is `NIL`, and \
         the abandoned words are no longer held by any field)"
    );

    let mut rooted = Vec::new();
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        for &word in roots
            .x
            .iter()
            .chain(roots.stack.iter())
            .chain(roots.fr.iter())
        {
            if pointer_bearing(word) {
                rooted.push(word);
            }
        }
    });
    assert_eq!(
        raws(walked),
        raws(rooted),
        "with_roots and for_each_root must still agree after the relocation"
    );
}
