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

//! The `select_val` pair ladder — the **one** definition of "does this alternative
//! match the scrutinee", shared by the interpreter's `Instr::SelectVal` arm and both
//! native tiers' `select_val` plans (`crate::jit::baseline::emit::select` and
//! `crate::jit::cranelift::shape` accept a candidate exactly when
//! [`constant_word`] answers it, and compare the words it returns).
//!
//! # Why a ladder, and what it mirrors
//!
//! The C interpreter does **not** compare terms in a `select_val`: the loader sorts the
//! alternatives and bakes them into the instruction stream as raw `BeamInstr` words, and
//! the body compares those words directly — a binary search over the sorted values
//! (`select_val_bins.select`, `../../erts/emulator/beam/emu/select_instrs.tab:34-78`) or a
//! linear scan (`select_val_lin.execute`, `:130-153`), both of which are plain integer
//! compares against `select_val`, with a jump table on a hit. A small-int alternative is
//! therefore an *unboxed* `BeamInstr` constant; no term is materialized, and no
//! general term ordering is entered.
//!
//! The typed IR keeps the alternatives as [`Operand`]s, so the same shape has to be
//! recovered at run time — this ladder, in cost order, with [`constant_word`] as the
//! static half:
//!
//! 1. **Unboxed candidate operand.** A candidate that is an immediate constant (a small
//!    int, an atom, `[]`) is answered by one raw-word compare against the scrutinee's own
//!    word — no `Eterm` is built and no `read_operand` allocation happens (a small-int
//!    `Operand` used to be materialized through
//!    [`terms::make_integer_unrooted`](crate::terms::make_integer_unrooted) on *every*
//!    pair). An out-of-range `Integer`/`Character` is **not** an immediate constant: it
//!    materializes as a bignum, which a bignum scrutinee can be `=:=` to, so it falls
//!    through to the general path.
//! 2. **Word identity.** For a candidate that is not an immediate constant (a register, a
//!    literal, a bignum constant), the operand is read and its **word** compared: word
//!    identity implies `=:=` for every term kind (two names of one boxed object, two
//!    immediates of one value), so a hit is a match with no ordering.
//! 3. **Either word immediate.** An immediate carries the primary tag `11`, which no cons,
//!    boxed or header word can carry, so a word that is not the very same word as an
//!    immediate's is never `=:=` to it — a *definite* non-match with no `cmp`. (Both sides
//!    immediate is the special case the tag table below pins; one side immediate is the same
//!    argument with the primary tag alone.)
//! 4. **Anything else** — the general `beam_term::cmp` `=:=` fallback, unchanged.
//!
//! # The tag facts steps 1-3 rest on
//!
//! `beam_term` freezes the C layout (`beam_term::tag`, `erts/emulator/beam/erl_term.h`):
//! a term's low two bits are its primary tag (header `00`, list `01`, boxed `10`, immediate
//! `11`), a cons/boxed word additionally carries at most `TAG_LITERAL_PTR` (`0x4`) above an
//! 8-byte-aligned pointer, and an immediate's own sub-tags occupy the bits above that:
//!
//! | class | encoding | low bits |
//! |---|---|---|
//! | header | `00` + arity/subtag | low nibble even, `< 0x4` with the tag |
//! | list | `01` + literal bit | low 3 bits `001`/`101` -> low nibble `1`/`9` |
//! | boxed | `10` + literal bit | low 3 bits `010`/`110` -> low nibble `2`/`A` |
//! | pid / port | `(payload << 4) \| 0x3` / `\| 0x7` | low nibble `3` / `7` |
//! | small int | `(value << 4) \| 0xF` | low nibble **`F`** |
//! | atom | `(idx << 6) \| 0xB` | low 6 bits **`0xB`** |
//! | catch | `(payload << 6) \| 0x1B` | low 6 bits `0x1B` |
//! | `[]` (`NIL`) | `0x3B` | low 6 bits **`0x3B`** |
//!
//! Two consequences, and both are pinned by this module's tests:
//!
//! * a word with low nibble `0xF` **is** a small integer, so comparing it with a small-int
//!   candidate's word is exactly `=:=` (equal value -> match, any other scrutinee ->
//!   non-match, floats/bignums/atoms/binaries included);
//! * a word whose low 6 bits are `0xB` is an atom, `0x3B` is `[]` and `0x1B` is a catch
//!   term — no other class can produce those bits (pid/port/small payloads land on
//!   `0x03`/`0x07`/`0x0F` patterns, boxed and list words are even or `…01`), so an
//!   atom/`[]` candidate's word identifies it uniquely and `[]` never matches the *atom*
//!   spelled `'[]'`.
//!
//! So for an immediate candidate the answer is **definite in both directions**, exactly as
//! the JIT's emitted `cmp`/`jcc` is: a hit means `=:=`, a miss means "not `=:=`".
//!
//! The same table gives rung 3 its reach: the primary tag separates an immediate (`11`) from
//! a cons (`01`), a boxed term (`10`) and a header word (`00`), so a *non-identical* word
//! where **either** side is an immediate can never be `=:=` — `1 =:= 1.0` and `a =:= {}` are
//! both decided without entering `beam_term::compare`.

#[cfg(test)]
use std::cell::Cell;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};
use beam_term::{cmp, is_immediate, make_atom, make_small, Eterm, MAX_SMALL, MIN_SMALL, NIL};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::read_operand;

/// The `select_val` scan: the first alternative whose candidate is exactly (`=:=`)
/// equal to `scrutinee`, in source order — C's ordered scan
/// (`../../erts/emulator/beam/emu/select_instrs.tab:130-153`), minus C's loader-time
/// sort (the IR keeps source order, and the scan is linear either way).
///
/// `None` means no alternative matched; the caller branches to `fail`, or raises
/// `function_clause` for a `NO_FAIL` switch.
pub(crate) fn first_match(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    scrutinee: Eterm,
    pairs: &[(Operand, Label)],
) -> Result<Option<Label>> {
    for (candidate, label) in pairs {
        if pair_eq(machine, ctx, module, scrutinee, candidate)? {
            return Ok(Some(*label));
        }
    }
    Ok(None)
}

/// One alternative's `=:=` test — the ladder documented at the module level.
///
/// `#[inline]`-marked: it is called once per alternative of a switch that a real
/// workload (`cowboy_bstr:char_to_lower/1`) enters per byte, and the first two rungs are
/// two loads and a compare.
#[inline]
fn pair_eq(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    scrutinee: Eterm,
    candidate: &Operand,
) -> Result<bool> {
    // Rung 1: an immediate candidate constant is decided by word identity alone, with no
    // term materialized (this is the arm the compiler emits for every `case` over small
    // ints / atoms).
    let Some(word) = constant_word(candidate) else {
        // Rungs 2-4 need the candidate's value.
        let value = read_operand(machine, ctx, candidate)?;
        if value.raw() == scrutinee.raw() {
            count_word_answer();
            return Ok(true);
        }
        if is_immediate(value) || is_immediate(scrutinee) {
            // Rung 3: the words differ (rung 2), and an immediate's primary tag (`11`) can
            // never match a cons/boxed/header word, so this is a definite non-match.
            count_word_answer();
            return Ok(false);
        }
        count_cmp_answer();
        return Ok(cmp(scrutinee, value, ctx.atoms(module)).is_eq());
    };
    count_word_answer();
    Ok(scrutinee.raw() as i64 == word)
}

/// The raw `Eterm` word of a candidate operand that is an **immediate constant** — a
/// small int (`Integer`/`Character` inside [`MIN_SMALL`]`..=`[`MAX_SMALL`]), an atom, or
/// `[]` — i.e. a candidate the ladder (and both native tiers) may decide by a raw-word
/// compare; `None` for every operand that needs the value read and possibly a general
/// comparison (a register, a literal, an out-of-range `Integer`/`Character` that
/// materializes as a bignum).
///
/// This is the **single** definition of "decidable by word identity": the interpreter's
/// ladder reads it, and `crate::jit::baseline::emit`'s and `crate::jit::cranelift`'s
/// `select_val` plans accept an alternative exactly when it answers, emitting the same
/// word compare this module performs — so the tiers cannot drift.
#[inline]
pub(crate) fn constant_word(operand: &Operand) -> Option<i64> {
    match operand {
        Operand::Integer(value) => small_word(*value),
        Operand::Character(c) => small_word(i64::from(*c)),
        Operand::Atom(atom) => Some(make_atom(*atom).raw() as i64),
        Operand::Nil => Some(NIL.raw() as i64),
        _ => None,
    }
}

/// The raw word of a small-int constant, or `None` when `value` is outside the 60-bit
/// immediate range — the point where a constant becomes a **bignum** and the ladder
/// must not short-circuit (a bignum scrutinee can be `=:=` to it).
///
/// Same range test as [`crate::terms::make_integer_unrooted`], so the two agree on which
/// values are small (the negative side included: `MIN_SMALL` is in range,
/// `MIN_SMALL - 1` is not).
#[inline]
fn small_word(value: i64) -> Option<i64> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        Some(make_small(value).raw() as i64)
    } else {
        None
    }
}

// A test-only tally of how every `select_val` pair was answered — the structural counter
// this ladder is gated on (the same thread-local idiom as [`super::label_map_builds`]):
// `WORD_ANSWERS` counts pairs the ladder decided **without** `cmp` (rungs 1-3),
// `CMP_ANSWERS` counts pairs that reached the general term comparison (rung 4).
//
// Compiled only under `cfg(test)`, so it is absent from production builds and from the
// `tests/` integration suites (which link the non-test library) and cannot perturb their
// timing. A pair is counted exactly once, on the rung that answered it.
#[cfg(test)]
thread_local! {
    static WORD_ANSWERS: Cell<usize> = const { Cell::new(0) };
    static CMP_ANSWERS: Cell<usize> = const { Cell::new(0) };
}

/// Test-only: reset the current thread's `select_val` pair tallies.
#[cfg(test)]
pub(crate) fn reset_select_answers() {
    WORD_ANSWERS.with(|c| c.set(0));
    CMP_ANSWERS.with(|c| c.set(0));
}

/// Test-only: `(decided_without_cmp, decided_by_cmp)` — the current thread's tally.
#[cfg(test)]
pub(crate) fn select_answers() -> (usize, usize) {
    (WORD_ANSWERS.with(|c| c.get()), CMP_ANSWERS.with(|c| c.get()))
}

/// Count a pair answered by rungs 1-3 (no general comparison).
#[inline]
fn count_word_answer() {
    #[cfg(test)]
    WORD_ANSWERS.with(|c| c.set(c.get() + 1));
}

/// Count a pair answered by rung 4 (the general comparison).
#[inline]
fn count_cmp_answer() {
    #[cfg(test)]
    CMP_ANSWERS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;
    use crate::terms;
    use beam_term::{
        is_atom, is_catch, is_nil, is_small, Atom,
    };

    /// The **pre-ladder** answer for one pair — `cmp(scrutinee, read_operand(candidate),
    /// atoms).is_eq()`, exactly what the old arm computed. Every case below asserts the
    /// ladder reproduces it.
    fn cmp_oracle(
        module: &Module,
        machine: &Machine,
        ctx: &mut MinimalContext,
        scrutinee: Eterm,
        candidate: &Operand,
    ) -> bool {
        let value = read_operand(machine, ctx, candidate).expect("the candidate reads");
        cmp(scrutinee, value, ctx.atoms(module)).is_eq()
    }

    /// Run the ladder on one pair: `(answer, answered_by_cmp)`.
    fn ladder(
        module: &Module,
        machine: &Machine,
        ctx: &mut MinimalContext,
        scrutinee: Eterm,
        candidate: &Operand,
    ) -> (bool, bool) {
        reset_select_answers();
        let answer = pair_eq(machine, ctx, module, scrutinee, candidate).expect("a decidable pair");
        let (word, by_cmp) = select_answers();
        assert_eq!(
            word + by_cmp,
            1,
            "every pair is tallied exactly once (word {word}, cmp {by_cmp})"
        );
        (answer, by_cmp == 1)
    }

    /// Assert the ladder agrees with [`cmp_oracle`] on one pair, and report whether the
    /// general comparison was taken.
    #[track_caller]
    fn agree(
        module: &Module,
        machine: &Machine,
        ctx: &mut MinimalContext,
        scrutinee: Eterm,
        candidate: &Operand,
    ) -> (bool, bool) {
        let want = cmp_oracle(module, machine, ctx, scrutinee, candidate);
        let (got, by_cmp) = ladder(module, machine, ctx, scrutinee, candidate);
        assert_eq!(
            got, want,
            "the ladder answered {got} where `cmp` answers {want} (candidate {candidate:?})"
        );
        (got, by_cmp)
    }

    /// The scalar shapes: equal/unequal small ints, the `Character` operand form, both
    /// sides of the small-int range boundary, and atoms — every one of them decided by a
    /// raw-word compare, with the negative small ints handled exactly as
    /// `make_integer_unrooted` does (`MIN_SMALL` is small, `MIN_SMALL - 1` is a bignum).
    #[test]
    fn small_ints_characters_and_atoms_are_decided_by_word_identity() {
        let mut module = Module::default();
        let alpha = module.atom_table.intern("alpha");
        let beta = module.atom_table.intern("beta");
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();

        // Equal and unequal small ints, through `Operand::Integer`.
        assert_eq!(agree(&module, &machine, &mut ctx, make_small(1), &Operand::Integer(1)), (true, false));
        assert_eq!(agree(&module, &machine, &mut ctx, make_small(1), &Operand::Integer(2)), (false, false));
        // Negative values: the encoding is `(value << 4) | 0xF`, so `-1` is all ones.
        assert_eq!(agree(&module, &machine, &mut ctx, make_small(-1), &Operand::Integer(-1)), (true, false));
        assert_eq!(agree(&module, &machine, &mut ctx, make_small(0), &Operand::Integer(-1)), (false, false));
        // The small-int range boundary: `MAX_SMALL`/`MIN_SMALL` are small, one step out is
        // a bignum constant and must NOT be short-circuited.
        assert_eq!(
            agree(&module, &machine, &mut ctx, make_small(MAX_SMALL), &Operand::Integer(MAX_SMALL)),
            (true, false)
        );
        assert_eq!(
            agree(&module, &machine, &mut ctx, make_small(MIN_SMALL), &Operand::Integer(MIN_SMALL)),
            (true, false)
        );
        // ... and a bignum constant never equals a small int. The constant is not an
        // immediate (it materializes as a bignum), so it is read; rung 3 then answers
        // definitively because the scrutinee is an immediate.
        assert_eq!(
            agree(&module, &machine, &mut ctx, make_small(MAX_SMALL), &Operand::Integer(MAX_SMALL + 1)),
            (false, false)
        );
        // `Operand::Character` is the same unboxed path.
        assert_eq!(
            agree(&module, &machine, &mut ctx, make_small(i64::from(b'A')), &Operand::Character(u32::from(b'A'))),
            (true, false)
        );
        assert_eq!(
            agree(&module, &machine, &mut ctx, make_small(i64::from(b'a')), &Operand::Character(u32::from(b'A'))),
            (false, false)
        );
        // Atoms, same and different — and an atom never matches a small int.
        assert_eq!(agree(&module, &machine, &mut ctx, make_atom(alpha), &Operand::Atom(alpha)), (true, false));
        assert_eq!(agree(&module, &machine, &mut ctx, make_atom(alpha), &Operand::Atom(beta)), (false, false));
        assert_eq!(agree(&module, &machine, &mut ctx, make_atom(alpha), &Operand::Integer(1)), (false, false));
        // `[]` is `NIL`, an immediate — and never the *atom* spelled `'[]'`.
        let bracket = module.atom_table.intern("[]");
        assert_eq!(agree(&module, &machine, &mut ctx, NIL, &Operand::Nil), (true, false));
        assert_eq!(agree(&module, &machine, &mut ctx, NIL, &Operand::Atom(bracket)), (false, false));
        assert_eq!(agree(&module, &machine, &mut ctx, make_atom(bracket), &Operand::Nil), (false, false));
        assert_eq!(agree(&module, &machine, &mut ctx, make_atom(bracket), &Operand::Atom(bracket)), (true, false));
    }

    /// The boxed shapes, where `=:=` is **not** word equality: a float equal to a small int
    /// is `=:=`-unequal (`1 =:= 1.0` is false), two separately allocated floats with the
    /// same value ARE `=:=`, two equal bignums in different allocations ARE `=:=`, and two
    /// heap binaries with equal bytes in different allocations ARE `=:=` — each of those
    /// must therefore come back from the general fallback, as an `Ok(true)` where
    /// `=:=` holds.
    #[test]
    fn boxed_values_that_are_exactly_equal_go_through_the_general_compare() {
        let module = Module::default();
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();

        // A float scrutinee against an immediate candidate held in a register: `1 =:= 1.0`
        // is **false**, and rung 3 answers it definitively (a float is never an immediate).
        let float = terms::make_float(&mut ctx, 1.0).expect("a float");
        let mut immediate_reg = Machine::new();
        immediate_reg.set_x(0, make_small(1)).expect("x0");
        assert_eq!(
            agree(&module, &immediate_reg, &mut ctx, float, &Operand::X(0)),
            (false, false),
            "a float is not `=:=` a small int, and it is not `cmp` that says so"
        );

        // Two `1.0` floats, allocated separately: `=:=` TRUE, word identity false, so the
        // only correct answer comes from the fallback.
        let other = terms::make_float(&mut ctx, 1.0).expect("a second float");
        assert_ne!(other.raw(), float.raw(), "two independent allocations");
        let mut machine2 = Machine::new();
        machine2.set_x(0, other).expect("x0");
        assert_eq!(
            agree(&module, &machine2, &mut ctx, float, &Operand::X(0)),
            (true, true),
            "two `1.0` floats are `=:=` through the general compare"
        );

        // Two equal bignums in different allocations: `=:=` TRUE, so again the fallback.
        let big_a = terms::make_integer_unrooted(&mut ctx, MAX_SMALL + 1).expect("a bignum");
        let big_b = terms::make_integer_unrooted(&mut ctx, MAX_SMALL + 1).expect("a second bignum");
        assert_ne!(big_a.raw(), big_b.raw(), "two independent allocations");
        let mut machine3 = Machine::new();
        machine3.set_x(0, big_b).expect("x0");
        assert_eq!(
            agree(&module, &machine3, &mut ctx, big_a, &Operand::X(0)),
            (true, true),
            "two equal bignums are `=:=` through the general compare"
        );
        // ... and a bignum scrutinee against a bignum *constant* operand (the out-of-range
        // `Integer`) is the same case: the ladder must not short-circuit it.
        assert_eq!(
            agree(&module, &machine, &mut ctx, big_a, &Operand::Integer(MAX_SMALL + 1)),
            (true, true)
        );

        // Two heap binaries with equal bytes in different allocations: `=:=` TRUE.
        let bin_a = terms::make_heap_binary(&mut ctx, b"hello").expect("a heap binary");
        let bin_b = terms::make_heap_binary(&mut ctx, b"hello").expect("a second heap binary");
        assert_ne!(bin_a.raw(), bin_b.raw(), "two independent allocations");
        let mut machine4 = Machine::new();
        machine4.set_x(0, bin_b).expect("x0");
        assert_eq!(
            agree(&module, &machine4, &mut ctx, bin_a, &Operand::X(0)),
            (true, true),
            "two equal heap binaries are `=:=` through the general compare"
        );

        // A tuple scrutinee against an atom candidate: two distinct non-immediates never
        // reach `cmp` as a *definite* answer unless both are immediate, so this one does.
        let tuple = terms::make_tuple(&mut ctx, &[make_small(1)]).expect("a tuple");
        assert_eq!(agree(&module, &machine, &mut ctx, tuple, &Operand::Integer(1)), (false, false));
    }

    /// Word identity is a match for **every** term kind, including "two names of the same
    /// boxed object" — the rung-2 shortcut, taken without any ordering.
    #[test]
    fn a_shared_boxed_word_is_a_match_without_a_compare() {
        let module = Module::default();
        let mut ctx = MinimalContext::new();
        let float = terms::make_float(&mut ctx, 2.5).expect("a float");
        let mut machine = Machine::new();
        machine.set_x(0, float).expect("x0");
        assert_eq!(
            agree(&module, &machine, &mut ctx, float, &Operand::X(0)),
            (true, false),
            "the very same word must be answered by identity, not by cmp"
        );
    }

    /// The tag facts the ladder rests on, taken from `beam_term`'s frozen layout: a word
    /// with low nibble `0xF` is a small int, a word whose low six bits are `0xB` is an
    /// atom, `0x3B` is `[]`, `0x1B` is a catch term — and no boxed/list/header/pid/port
    /// word can collide with any of those, so an immediate candidate's word identifies it
    /// uniquely (`[]` can never be the atom `'[]'`).
    #[test]
    fn immediate_classes_cannot_share_a_word() {
        // Small ints: the low nibble is the small-int sub-tag, whatever the payload.
        for value in [
            0i64,
            1,
            2,
            15,
            -1,
            -2,
            0x0F,
            0x100,
            MAX_SMALL,
            MAX_SMALL - 1,
            MIN_SMALL,
            MIN_SMALL + 1,
        ] {
            let word = make_small(value).raw();
            assert_eq!(word & 0xF, 0xF, "small int {value} must carry the 0xF nibble");
            assert!(is_small(Eterm::from_raw(word)));
            assert!(is_immediate(Eterm::from_raw(word)));
            assert!(!is_atom(Eterm::from_raw(word)));
            assert!(!is_nil(Eterm::from_raw(word)));
            assert!(!is_catch(Eterm::from_raw(word)));
        }

        // Atoms: the low six bits are the atom sub-tag for every payload, so no atom word
        // can ever look like a small int, `[]` or a catch term.
        for index in 0..64u32 {
            let word = make_atom(Atom::from_raw(index)).raw();
            assert_eq!(word & 0x3F, 0xB, "atom {index} must carry the 0xB sub-tag");
            assert!(is_atom(Eterm::from_raw(word)));
            assert_eq!(word & 0xF, 0xB, "never the small-int nibble");
            assert_ne!(word, NIL.raw());
        }
        // `[]` and catch terms sit on the same nibble as atoms, so they are separated by
        // the low six bits only — and they are exactly those two words.
        assert_eq!(NIL.raw() & 0x3F, 0x3B);
        assert_eq!(beam_term::make_catch(0).raw() & 0x3F, 0x1B);
        assert!(is_nil(NIL));
        assert!(!is_atom(NIL));

        // pid / port / small payloads land on 0x3 / 0x7 nibbles: never 0xB, never 0xF.
        for payload in [0u64, 1, 0x12, 0x1234_5678] {
            let pid = beam_term::make_internal_pid(payload);
            let port = beam_term::make_internal_port(payload);
            assert_eq!(pid.raw() & 0xF, 0x3);
            assert_eq!(port.raw() & 0xF, 0x7);
            assert_ne!(pid.raw() & 0xF, 0xF);
            assert_ne!(pid.raw() & 0x3F, 0xB);
            assert_ne!(port.raw() & 0x3F, 0xB);
        }

        // Real boxed and list words: low 3 bits are the primary tag plus the literal bit,
        // so their low nibble is `1`/`9` (cons) or `2`/`A` (boxed) — never `F`, and their
        // low six bits are never `B`.
        let mut ctx = MinimalContext::new();
        let boxed = [
            terms::make_float(&mut ctx, 1.0).expect("a float"),
            terms::make_heap_binary(&mut ctx, b"hi").expect("a binary"),
            terms::make_tuple(&mut ctx, &[make_small(1)]).expect("a tuple"),
            terms::make_integer_unrooted(&mut ctx, MAX_SMALL + 1).expect("a bignum"),
        ];
        for term in boxed {
            assert!(!is_immediate(term), "{term:?} must not be an immediate");
            assert_ne!(term.raw() & 0xF, 0xF, "{term:?} must not look like a small int");
            assert_ne!(term.raw() & 0x3F, 0xB, "{term:?} must not look like an atom");
            assert_ne!(term.raw() & 0x3F, 0x3B, "{term:?} must not look like `[]`");
        }
        let cons = terms::make_cons(&mut ctx, make_small(1), NIL).expect("a cons cell");
        assert!(!is_immediate(cons));
        assert_eq!(cons.raw() & 0x3, beam_term::TAG_PRIMARY_LIST);
        assert_ne!(cons.raw() & 0xF, 0xF);
        assert_ne!(cons.raw() & 0x3F, 0xB);
    }

    /// `constant_word` is the static half of the ladder, and the JIT plans' only criterion:
    /// the immediate constant kinds answer their exact `read_operand` word, and everything
    /// whose value needs a term (a register, a literal, an out-of-range integer) answers
    /// `None`.
    #[test]
    fn constant_word_is_the_operand_seams_word_for_immediates_only() {
        let mut ctx = MinimalContext::new();
        let machine = Machine::new();
        let mut module = Module::default();
        let atom = module.atom_table.intern("k");
        for (operand, term) in [
            (Operand::Integer(7), make_small(7)),
            (Operand::Integer(-7), make_small(-7)),
            (Operand::Integer(MAX_SMALL), make_small(MAX_SMALL)),
            (Operand::Integer(MIN_SMALL), make_small(MIN_SMALL)),
            (Operand::Character(u32::from(b'z')), make_small(i64::from(b'z'))),
            (Operand::Atom(atom), make_atom(atom)),
            (Operand::Nil, NIL),
        ] {
            assert_eq!(
                constant_word(&operand),
                Some(read_operand(&machine, &mut ctx, &operand).unwrap().raw() as i64),
                "{operand:?} must answer the word the seam reads"
            );
            assert_eq!(constant_word(&operand), Some(term.raw() as i64));
        }
        for operand in [
            Operand::Integer(MAX_SMALL + 1),
            Operand::Integer(MIN_SMALL - 1),
            Operand::X(0),
            Operand::Y(0),
            Operand::Literal(0),
            Operand::FloatReg(0),
        ] {
            assert_eq!(constant_word(&operand), None, "{operand:?} is not an immediate");
        }
    }

    /// The scan takes the **first** matching alternative in source order (C's ordered
    /// scan), and reports `None` when nothing matches.
    #[test]
    fn the_scan_takes_the_first_match_in_source_order() {
        let module = Module::default();
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let pairs = [
            (Operand::Integer(1), Label(11)),
            (Operand::Integer(2), Label(12)),
            (Operand::Integer(1), Label(13)),
        ];
        assert_eq!(
            first_match(&machine, &mut ctx, &module, make_small(1), &pairs).unwrap(),
            Some(Label(11))
        );
        assert_eq!(
            first_match(&machine, &mut ctx, &module, make_small(2), &pairs).unwrap(),
            Some(Label(12))
        );
        assert_eq!(
            first_match(&machine, &mut ctx, &module, make_small(3), &pairs).unwrap(),
            None
        );
        assert_eq!(
            first_match(&machine, &mut ctx, &module, make_small(1), &[]).unwrap(),
            None
        );
        // The tally covers exactly the pairs scanned before the hit.
        reset_select_answers();
        let _ = first_match(&machine, &mut ctx, &module, make_small(2), &pairs).unwrap();
        assert_eq!(select_answers(), (2, 0));
    }

    /// A bad register operand keeps the seam's typed error (`bad_register`), never a
    /// silent non-match — the ladder only adds fast rungs, it weakens no error path.
    #[test]
    fn an_out_of_range_register_is_still_a_typed_error() {
        let module = Module::default();
        let mut machine = Machine::with_max_register(2);
        let mut ctx = MinimalContext::new();
        let pairs = [(Operand::X(9), Label(11))];
        let error = first_match(&machine, &mut ctx, &module, make_small(1), &pairs).unwrap_err();
        assert!(matches!(error, beam_common::BeamError::BadArg));
        // The same error `read_operand` reports, so nothing was swallowed.
        assert!(matches!(
            read_operand(&machine, &mut ctx, &Operand::X(9)),
            Err(beam_common::BeamError::BadArg)
        ));
        let _ = machine.set_x(0, make_small(1));
    }

    /// The committed **real `erlc` fixture** end to end (`tests/fixtures/selval.beam`):
    /// every pair of an immediate-only switch (`ints/1` over small ints, `atoms/1` over
    /// atoms) is answered by rungs 1-3 on the interpreter, so the general comparison runs
    /// **zero** times, and the native tier never enters the ladder at all (its emitted
    /// block compares the same words inline) — the deterministic per-pair tally, not a
    /// timing.
    ///
    /// `tests/select_val.rs` is the observable half of the same fixture (both tiers vs
    /// the live `erl`, plus the zero-on-heap-allocation claim); this is the tally the
    /// ladder itself is gated on.
    #[test]
    fn the_committed_fixture_is_decided_without_a_general_compare() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/selval.beam"
        );
        let module = beam_loader::load_file(path)
            .unwrap_or_else(|error| panic!("load {path}: {error:?}"));
        let alpha = module.atom_table.lookup("alpha").expect("`selval` interns `alpha`");

        for (function, argument) in [("ints", make_small(3)), ("atoms", make_atom(alpha))] {
            let entry = module
                .atom_table
                .lookup(function)
                .unwrap_or_else(|| panic!("`selval` does not intern `{function}`"));
            for tier in [crate::Tier::Interpreter, crate::Tier::BaselineJit] {
                let mut ctx = MinimalContext::new();
                let mut machine = Machine::new();
                reset_select_answers();
                machine
                    .run_with_tier(&module, entry, 1, &[argument], &mut ctx, tier)
                    .unwrap_or_else(|error| panic!("selval:{function}/1 on {}: {error:?}", tier.name()));
                let (word, by_cmp) = select_answers();
                assert_eq!(
                    by_cmp, 0,
                    "{}: {by_cmp} pair(s) of `selval:{function}/1` paid a general term \
                     comparison over immediate alternatives",
                    tier.name()
                );
                match tier {
                    // The interpreter scans the switch through this ladder.
                    crate::Tier::Interpreter => assert!(
                        word > 0,
                        "the ladder answered no pair of `selval:{function}/1`: it did not engage"
                    ),
                    // The baseline tier inlines the switch into its own word compares, so
                    // the ladder is legitimately never reached (tier-equivalence is
                    // `tests/select_val.rs`'s claim; this pins the structural difference).
                    _ => assert_eq!(
                        word, 0,
                        "{}: the inlined `select_val` block must not reach the ladder",
                        tier.name()
                    ),
                }
            }
        }
    }
}