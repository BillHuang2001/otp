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

//! Pins for the two root-enumeration doors of [`Machine`]:
//!
//! * [`Machine::for_each_root`] — the read-only **pointer-bearing root enumeration** a
//!   census or reachability walk consumes.
//! * [`Machine::for_each_root_mut`] — its writable twin, the door a **relocating** pass
//!   uses: the same root words, handed over as three `&mut [Eterm]` slices one at a time.
//!
//! The properties, each its own test:
//!
//! * **Coverage** — every file [`Machine::with_roots`] hands over is walked (the used
//!   X-register prefix, the whole Erlang stack, the used FR prefix) plus the machine's
//!   five *loose* heap terms, which `with_roots` parks at the stack tail.
//! * **Selection** — only `LIST`/`BOXED` words are reported; small integers, atoms and
//!   `NIL` are skipped, because relocating an immediate is meaningless.
//! * **Agreement** — the word set the walk reports equals the pointer-bearing subset of
//!   the frozen [`RootSet`](crate::context::RootSet) `with_roots` hands over for the very
//!   same machine state, so the two doors can never disagree about *what* is rooted.
//! * **Parity of the two doors** — the mutable door enumerates the same root words, and a
//!   write through it lands in the very slots the census reports (and is observed by the
//!   census afterwards), while both doors keep the same `args_scratch` exclusion.
//!
//! Plus the re-seat pin: a `with_roots` hand-off whose visitor **moves** the parked words
//! leaves both supplied-trace latches holding the *moved* terms.
//!
//! Every term here is built in a [`MinimalContext`]'s region heap (a real boxed/cons
//! pointer) or is a real immediate, so the tag classification under test is the one a
//! live machine produces. The walk never dereferences: it hands back the words verbatim.

use beam_common::{BeamError, ExceptionClass};
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

// ---------------------------------------------------------------------------
// `Machine::with_roots`'s fast-path predicate: one loose field at a time
// ---------------------------------------------------------------------------

/// Read loose heap-term slot `slot` straight from its **field**, in
/// [`Machine::loose_heap_terms`] / [`Machine::seat_loose_heap_terms`] order.
///
/// Written out field by field — deliberately not through `loose_heap_terms` — so the pins
/// below compare the parked-and-re-seated *fields* with an independent reading of them, not
/// with the engine's own array.
fn loose_slot(machine: &Machine, slot: usize) -> Option<Eterm> {
    match slot {
        0 => machine.raise_function_clause_args,
        1 => machine.pending_function_clause_args,
        2 => machine.pending_exception.map(|(_class, reason)| reason),
        3 => machine.pending_supplied_term,
        4 => machine.raise_supplied_term,
        _ => unreachable!("the machine has exactly {} loose fields", Machine::LOOSE_HEAP_TERMS),
    }
}

/// Arm loose heap-term slot `slot` with `term`, leaving every other loose field unarmed.
fn arm_loose_slot(machine: &mut Machine, slot: usize, term: Eterm) {
    match slot {
        0 => machine.raise_function_clause_args = Some(term),
        1 => machine.pending_function_clause_args = Some(term),
        2 => machine.pending_exception = Some((ExceptionClass::Error, term)),
        3 => machine.pending_supplied_term = Some(term),
        4 => machine.raise_supplied_term = Some(term),
        _ => unreachable!("the machine has exactly {} loose fields", Machine::LOOSE_HEAP_TERMS),
    }
}

/// A machine with a 1-slot frame holding `NIL`, **no** loose term armed and nothing else
/// rooted, so a test that arms one loose field can attribute every parked word to it.
fn empty_roots_machine() -> Machine {
    let mut machine = Machine::new();
    machine.push_frame(1).expect("a 1-slot frame fits the stack");
    machine.set_y(0, NIL).expect("y(0) is inside the frame");
    assert_eq!(
        machine.loose_heap_terms(),
        [None; Machine::LOOSE_HEAP_TERMS],
        "the machine must start with no loose heap term armed"
    );
    machine
}

/// Play a moving collector over the hand-off: assert `held` rides the handed root set,
/// rewrite **every** occurrence of it in the stack slice with `moved`, and report how many
/// slots it occupied.
fn park_and_move(machine: &mut Machine, held: Eterm, moved: Eterm) -> usize {
    let mut parked = 0;
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        assert!(
            roots
                .x
                .iter()
                .chain(roots.stack.iter())
                .chain(roots.fr.iter())
                .any(|word| *word == held),
            "the armed loose term must ride the mutable root set with_roots hands over — a \
             fast path that took its cheap branch with a loose term armed would leave the \
             term unrooted, and a collecting host would strand it"
        );
        for slot in roots.stack.iter_mut() {
            if *slot == held {
                *slot = moved;
                parked += 1;
            }
        }
    });
    parked
}

/// **The soundness pin for [`Machine::with_roots`]'s fast-path predicate.**
///
/// `with_roots` parks the machine's loose heap terms — and re-seats each field from the slot
/// the host wrote back through — **iff** at least one of the five is armed. A predicate that
/// reads the wrong field, or that forgets one, silently takes the cheap branch while a heap
/// term is held outside the frozen [`RootSet`]: the visitor's relocation never reaches it and
/// the field keeps the pre-hand-off word, i.e. a **dangling pointer** after a real
/// collection. Nothing about that is visible without a visitor that actually moves things,
/// which is what this test is.
///
/// Each of the five fields is armed **on its own** (every other loose field stays unarmed),
/// so an enum-erated predicate that is right for "all armed" and wrong for "exactly one armed"
/// fails here. The parked word must appear exactly once, the field must hold the *moved* word
/// afterwards, the stack length must be restored, and both root-enumeration doors must report
/// the moved word and nothing else.
#[test]
fn with_roots_parks_and_relocates_each_loose_field_armed_on_its_own() {
    let mut ctx = MinimalContext::new();
    for slot in 0..Machine::LOOSE_HEAP_TERMS {
        // Distinct pointer-bearing words per slot, so a stale word from the previous
        // iteration (or the wrong field) can never pass the assertions below.
        let held = terms::make_list(&mut ctx, &[make_small(100 + slot as i64)])
            .expect("a cons cell");
        let moved = terms::make_tuple(&mut ctx, &[make_small(200 + slot as i64)])
            .expect("a boxed tuple");
        assert!(
            pointer_bearing(held) && pointer_bearing(moved),
            "both words must be relocatable for the move to be observable"
        );

        let mut machine = empty_roots_machine();
        arm_loose_slot(&mut machine, slot, held);
        assert_eq!(
            machine
                .loose_heap_terms()
                .iter()
                .filter(|term| term.is_some())
                .count(),
            1,
            "slot {slot} is the only armed loose field"
        );
        let stack_before = machine.stack_mut().len();

        let parked = park_and_move(&mut machine, held, moved);
        assert_eq!(
            parked, 1,
            "slot {slot}: the armed loose term is parked exactly once"
        );
        assert_eq!(
            loose_slot(&machine, slot),
            Some(moved),
            "slot {slot}: the field must be re-seated from the slot the host wrote back \
             through — a fast path that skipped the parking leaves the stale word here"
        );
        assert_eq!(
            machine.stack_mut().len(),
            stack_before,
            "slot {slot}: the parked word is truncated off the stack afterwards"
        );

        let mut walked = Vec::new();
        machine.for_each_root(|term| walked.push(term));
        assert_eq!(
            walked,
            vec![moved],
            "slot {slot}: for_each_root reports the re-seated word, never the abandoned one"
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
            "slot {slot}: the two doors still agree after the relocation"
        );
    }
}

/// The other half of the pin: with **no** loose term armed, `with_roots` parks nothing — the
/// handed stack is exactly the Erlang stack.
///
/// This is the shape the no-parking fast path must keep (its cheap branch is taken *iff* all
/// five fields are unarmed), and it is what makes the fast path byte-identical to the
/// pre-parking behavior.
#[test]
fn with_roots_parks_nothing_when_no_loose_field_is_armed() {
    let mut machine = empty_roots_machine();
    let stack_before = machine.stack_mut().len();
    let handed = machine.with_roots(RootSet::ALL_LIVE, |roots| {
        assert_eq!(
            roots.stack.len(),
            stack_before,
            "an unarmed machine hands over the Erlang stack and nothing else"
        );
        roots.stack.len()
    });
    assert_eq!(handed, stack_before);
    assert_eq!(machine.stack_mut().len(), stack_before);
}

/// The all-armed twin of the per-slot pin: every loose field armed at once must be parked and
/// re-seated, in one hand-off, and none may be dropped or cross-seated.
#[test]
fn with_roots_relocates_all_five_loose_fields_in_one_hand_off() {
    let mut ctx = MinimalContext::new();
    let mut held = Vec::new();
    let mut moved = Vec::new();
    for slot in 0..Machine::LOOSE_HEAP_TERMS {
        held.push(terms::make_list(&mut ctx, &[make_small(300 + slot as i64)]).expect("a cons"));
        moved.push(
            terms::make_tuple(&mut ctx, &[make_small(400 + slot as i64)]).expect("a tuple"),
        );
    }
    let mut machine = empty_roots_machine();
    for (slot, term) in held.iter().enumerate() {
        arm_loose_slot(&mut machine, slot, *term);
    }
    let stack_before = machine.stack_mut().len();

    let mut parked = 0;
    machine.with_roots(RootSet::ALL_LIVE, |roots| {
        for slot in roots.stack.iter_mut() {
            if let Some(index) = held.iter().position(|word| *word == *slot) {
                *slot = moved[index];
                parked += 1;
            }
        }
    });
    assert_eq!(
        parked,
        Machine::LOOSE_HEAP_TERMS,
        "every armed loose term is parked, in one hand-off"
    );
    for slot in 0..Machine::LOOSE_HEAP_TERMS {
        assert_eq!(
            loose_slot(&machine, slot),
            Some(moved[slot]),
            "slot {slot} must hold its own moved word (not another slot's, and not the stale one)"
        );
    }
    assert_eq!(machine.stack_mut().len(), stack_before);
}

// ---------------------------------------------------------------------------
// The writable twin: `Machine::for_each_root_mut`
// ---------------------------------------------------------------------------
/// **The parity gate between the two root doors.**
///
/// [`Machine::for_each_root_mut`] must hand over exactly the root words
/// [`Machine::for_each_root`] reports. The two deliver them in different *shapes* and in
/// different *orders*: the census yields single pointer-bearing words and reports the five
/// loose terms **last**, from their fields, while the writable door yields three contiguous
/// slices (`x` prefix, whole stack, `fr` prefix, immediates included) and the loose terms
/// arrive **inside the stack slice** because `with_roots` parks them at the stack tail
/// before handing the stack over. So the comparison is over the sorted **multiset** — a
/// term may legitimately be rooted from two slots.
#[test]
fn for_each_root_mut_enumerates_the_same_roots_as_for_each_root() {
    let (mut machine, expected) = roots_fixture();

    let mut census = Vec::new();
    machine.for_each_root(|term| census.push(term));

    // One visit per root file, each slice borrowed only for the duration of its own call.
    let mut slices: Vec<Vec<Eterm>> = Vec::new();
    machine
        .for_each_root_mut(&mut |slice: &mut [Eterm]| {
            slices.push(slice.to_vec());
            Ok(())
        })
        .expect("a non-failing visitor");

    assert_eq!(
        slices.len(),
        3,
        "the used X prefix, the whole stack (loose terms parked inside it) and the used FR \
         prefix — handed over one at a time"
    );

    let writable: Vec<Eterm> = slices
        .iter()
        .flatten()
        .copied()
        .filter(|&word| pointer_bearing(word))
        .collect();

    assert_eq!(
        writable.len(),
        9,
        "the fixture roots nine pointer-bearing words"
    );
    assert_eq!(
        raws(writable.clone()),
        raws(census.clone()),
        "the writable door must cover exactly the words the census reports"
    );
    assert_eq!(
        raws(census),
        raws(expected.clone()),
        "and that same set is the fixture's own"
    );

    // The three pre-existing immediates of the fixture sit *inside* the handed slices: the
    // door cannot and does not filter them out (a relocating visitor leaves them untouched),
    // which is the documented difference from the census's selection.
    assert!(
        slices[0].contains(&make_small(7)),
        "x(1) is an immediate and still part of the handed X prefix"
    );
    assert!(
        slices[1].contains(&NIL),
        "y(1) is `NIL` and still part of the handed stack"
    );

    // The five loose terms arrive through the **stack** slice (they were parked there),
    // never as their own slice and never in the register files.
    for expected_loose in &expected[4..] {
        assert!(
            slices[1].contains(expected_loose),
            "the loose term {expected_loose:?} must be parked in the handed stack"
        );
        assert!(!slices[0].contains(expected_loose) && !slices[2].contains(expected_loose));
    }
}

/// A rewrite through the writable door lands in the **very slots** the census reports: play
/// a moving collector, giving every pointer-bearing root word a distinct relocation target
/// (a boxed tuple, so the move is observable and the words stay distinguishable).
///
/// Afterwards the census must enumerate exactly the relocated words — the same *set* of
/// slots, not merely the same count — and none of the abandoned ones, because no slot holds
/// them any more. Counting alone would pass even if the two doors disagreed about *which*
/// slots are roots.
#[test]
fn for_each_root_mut_relocates_the_very_slots_for_each_root_reports() {
    /// The number of pointer-bearing words `roots_fixture` arms (asserted below).
    const FIXTURE_ROOTS: usize = 9;

    // The relocation targets are built **before** the fixture, in a `MinimalContext` that
    // stays alive for the whole test. Ordering matters: `roots_fixture` builds its terms in a
    // context it drops on return, so a context created *after* it could be handed the very
    // same arena addresses — and then a "moved" word would be indistinguishable from an
    // "abandoned" one at the word level, which is what this test compares.
    let mut ctx = MinimalContext::new();
    let moved: Vec<Eterm> = (0..FIXTURE_ROOTS)
        .map(|n| terms::make_tuple(&mut ctx, &[make_small(100 + n as i64)]).expect("a boxed tuple"))
        .collect();

    let (mut machine, original) = roots_fixture();
    assert_eq!(
        original.len(),
        FIXTURE_ROOTS,
        "one relocation target per rooted word"
    );
    let mut next = 0usize;
    machine
        .for_each_root_mut(&mut |slice| {
            for slot in slice.iter_mut() {
                if pointer_bearing(*slot) {
                    let target = moved
                        .get(next)
                        .copied()
                        .expect("at most one relocation target per rooted word");
                    *slot = target;
                    next += 1;
                }
            }
            Ok(())
        })
        .expect("a non-failing visitor");
    assert_eq!(
        next,
        original.len(),
        "every pointer-bearing root word is handed over exactly once"
    );

    let mut after = Vec::new();
    machine.for_each_root(|term| after.push(term));
    assert_eq!(
        raws(after.clone()),
        raws(moved),
        "the census must now report exactly the relocated words"
    );
    for abandoned in &original {
        assert!(
            !after.contains(abandoned),
            "the abandoned word {abandoned:?} is held by no root slot any more"
        );
    }
}

/// The sharpest form of the parity claim: replace **every** pointer-bearing root word with a
/// distinguishable **immediate** marker and check the census's answer, then restore.
///
/// Two things are proved at once. (1) After the rewrite the census reports **nothing at
/// all** — no root slot in any of the three files or among the loose fields still holds a
/// heap pointer, so the writable door covered every slot the census reads. (2) A second visit
/// compares the slices **slot for slot**: exactly the words the first visit saw as
/// pointer-bearing now hold their markers while every pre-existing immediate is byte-identical
/// — the door hands over the same columns with the same widths, and an immediate in the middle
/// of a prefix is passed through rather than dropped. Restoring the originals through the same
/// door (the "then restore" step, so the machine is left as the fixture built it) brings the
/// census back to the original root multiset, which a slot-count-only check could not
/// distinguish.
#[test]
fn for_each_root_mut_writes_are_observed_by_for_each_root() {
    /// Well inside `MIN_SMALL..=MAX_SMALL`, so a marker is a legal small integer.
    const MARKER_BASE: i64 = 1_000_000;

    let (mut machine, original) = roots_fixture();

    // What each slice held before the rewrite, in visit order, so a later visit can compare
    // slot for slot and restore.
    let mut before: Vec<Vec<Eterm>> = Vec::new();
    let mut markers: Vec<Eterm> = Vec::new();
    let mut replaced = 0usize;
    machine
        .for_each_root_mut(&mut |slice| {
            let mut recorded = Vec::with_capacity(slice.len());
            for slot in slice.iter_mut() {
                recorded.push(*slot);
                if pointer_bearing(*slot) {
                    let marker = make_small(MARKER_BASE + replaced as i64);
                    markers.push(marker);
                    *slot = marker;
                    replaced += 1;
                }
            }
            before.push(recorded);
            Ok(())
        })
        .expect("a non-failing visitor");
    assert_eq!(
        replaced,
        original.len(),
        "one marker per pointer-bearing root word"
    );

    // (1) Nothing pointer-bearing is left anywhere the census looks.
    let mut census_after = Vec::new();
    machine.for_each_root(|term| census_after.push(term));
    assert!(
        census_after.is_empty(),
        "every root slot holds an immediate now, so the census reports nothing; got \
         {census_after:?}"
    );

    // (2) Slot for slot: the marker sits exactly where the census's word was; every other
    // word is untouched.
    let mut visited = 0usize;
    let mut visit = 0usize;
    machine
        .for_each_root_mut(&mut |slice| {
            let expected_slice = &before[visit];
            assert_eq!(
                slice.len(),
                expected_slice.len(),
                "the same slot columns must be handed over on a second visit"
            );
            for (slot, &was) in slice.iter().zip(expected_slice) {
                if pointer_bearing(was) {
                    assert_eq!(
                        *slot, markers[visited],
                        "the marker written through the writable door must sit in the slot \
                         the census used to report"
                    );
                    visited += 1;
                } else {
                    assert_eq!(
                        *slot, was,
                        "an immediate root word is passed through untouched"
                    );
                }
            }
            visit += 1;
            Ok(())
        })
        .expect("a non-failing visitor");
    assert_eq!(
        (visit, visited),
        (before.len(), markers.len()),
        "both visits covered the same three slices and the same rooted slots"
    );

    // Restore the originals through the same door.
    let mut restored = 0usize;
    machine
        .for_each_root_mut(&mut |slice| {
            assert_eq!(
                slice.len(),
                before[restored].len(),
                "the restore visit sees the same slice widths"
            );
            for (slot, &was) in slice.iter_mut().zip(&before[restored]) {
                *slot = was;
            }
            restored += 1;
            Ok(())
        })
        .expect("a non-failing visitor");
    assert_eq!(restored, before.len(), "every slice was restored");

    let mut census_restored = Vec::new();
    machine.for_each_root(|term| census_restored.push(term));
    assert_eq!(
        raws(census_restored),
        raws(original),
        "restoring every slot brings the census back to the original root multiset"
    );
}

/// The `args_scratch` exclusion holds for the writable door, and an `Err`-returning visitor
/// still leaves the machine's parking state exactly as it was.
///
/// The argument window is `mem::take`n out of the machine for the whole wide `call_bif`
/// dispatch, so no root door can see it — least of all a `&mut self` one. The exclusion is
/// pinned by **width and by address range**, not by comparing term words: the handed slices
/// must cover exactly the X prefix plus the stack (with the armed loose words parked in it),
/// and none of their byte ranges may overlap the `args_scratch` buffer. A failure in the
/// middle of the hand-off must still truncate the parked loose words off the stack and
/// re-seat all five fields; only the visitor's own error travels on.
#[test]
fn for_each_root_mut_excludes_args_scratch_and_restores_the_parking_on_error() {
    let (mut machine, _) = roots_fixture();
    let mut ctx = MinimalContext::new();
    // A boxed term held **only** by `args_scratch`. (A word-level comparison against the
    // handed slices would be a coincidence trap here: the fixture's own `MinimalContext` is
    // dropped when it returns, so this context's arena can hand out the same addresses.)
    let window = terms::make_tuple(&mut ctx, &[make_small(21)]).expect("a boxed tuple");
    assert!(
        pointer_bearing(window),
        "the window's term must be relocatable"
    );
    machine.args_scratch.push(window);

    let x_live = machine.used_x_extent() as usize;
    let fr_live = machine.used_fr_extent() as usize;
    assert!(
        fr_live > 0,
        "the fixture has a live FR slot, so leaving the FR prefix out of the count matters"
    );
    let stack_before = machine.stack_mut().len();
    let loose_before = machine.loose_heap_terms();
    let armed = loose_before.iter().filter(|slot| slot.is_some()).count();
    assert_eq!(armed, Machine::LOOSE_HEAP_TERMS, "the fixture arms them all");
    let word_bytes = std::mem::size_of::<Eterm>();
    let window_start = machine.args_scratch.as_ptr() as usize;
    let window_end = window_start + machine.args_scratch.len() * word_bytes;

    let mut words: Vec<Eterm> = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut handed = 0usize;
    let outcome = machine.for_each_root_mut(&mut |slice| {
        handed += 1;
        ranges.push((slice.as_ptr() as usize, slice.len() * word_bytes));
        words.extend_from_slice(slice);
        if handed == 2 {
            // Fail on the stack slice — the one carrying the parked loose terms.
            return Err(BeamError::BadArg);
        }
        Ok(())
    });

    assert!(outcome.is_err(), "the visitor's error must be propagated");
    assert_eq!(
        handed, 2,
        "the slices are handed over one at a time: a failure stops the walk, so the FR \
         prefix is never reached"
    );
    assert_eq!(
        words.len(),
        x_live + stack_before + armed,
        "the walk covers the X prefix and the stack (loose words parked in it) and nothing \
         else — the failed walk never reached the FR prefix"
    );
    for &(start, len) in &ranges {
        assert!(
            start + len <= window_start || window_end <= start,
            "no handed slice may overlap the call_bif argument window ({start:#x}..{:#x})",
            start + len
        );
    }
    assert_eq!(machine.args_scratch.len(), 1);
    assert_eq!(
        machine.args_scratch[0], window,
        "the argument window is untouched by the walk"
    );
    let mut census = Vec::new();
    machine.for_each_root(|term| census.push(term));
    assert_eq!(
        census.len(),
        9,
        "the census agrees: `args_scratch` contributes no root beyond the fixture's nine"
    );
    assert_eq!(
        machine.stack_mut().len(),
        stack_before,
        "the parked loose words are truncated off the stack even on the error path"
    );
    assert_eq!(
        machine.loose_heap_terms(),
        loose_before,
        "every loose field is re-seated even on the error path"
    );
}
