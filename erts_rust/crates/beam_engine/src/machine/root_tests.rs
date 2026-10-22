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
//!   three *loose* heap terms, which `with_roots` parks at the stack tail.
//! * **Selection** — only `LIST`/`BOXED` words are reported; small integers, atoms and
//!   `NIL` are skipped, because relocating an immediate is meaningless.
//! * **Agreement** — the word set the walk reports equals the pointer-bearing subset of
//!   the frozen [`RootSet`](crate::context::RootSet) `with_roots` hands over for the very
//!   same machine state, so the two doors can never disagree about *what* is rooted.
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
/// * the two loose latch slots and the pending exception's reason all armed (a boxed
///   tuple each way), which is the state `with_roots` parks at the stack tail.
///
/// Returns the machine and the six pointer-bearing words it roots, in the order
/// [`Machine::for_each_root`] documents (`x` prefix, stack, `fr` prefix, loose terms).
fn roots_fixture() -> (Machine, Vec<Eterm>) {
    let mut ctx = MinimalContext::new();
    let x0 = terms::make_tuple(&mut ctx, &[make_small(1)]).expect("a boxed tuple");
    let x2 = terms::make_list(&mut ctx, &[make_small(2)]).expect("a cons cell");
    let y0 = terms::make_tuple(&mut ctx, &[make_small(3)]).expect("a boxed tuple");
    let fr0 = terms::make_tuple(&mut ctx, &[make_small(4)]).expect("a boxed tuple");
    let loose_raise = terms::make_tuple(&mut ctx, &[make_small(5)]).expect("a boxed tuple");
    let missing_args = terms::make_tuple(&mut ctx, &[make_small(6)]).expect("a boxed tuple");
    let reason = terms::make_list(&mut ctx, &[make_small(7)]).expect("a cons cell");

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

    (
        machine,
        vec![x0, x2, y0, fr0, loose_raise, missing_args, reason],
    )
}

#[test]
fn for_each_root_enumerates_registers_stack_frames_and_the_loose_terms() {
    let (machine, expected) = roots_fixture();
    let mut seen = Vec::new();
    machine.for_each_root(|term| seen.push(term));
    assert_eq!(
        seen, expected,
        "every rooted file and all three loose terms, in the documented order"
    );
    assert_eq!(seen.len(), 7, "two x registers, one Y slot, one FR slot, three loose");
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
