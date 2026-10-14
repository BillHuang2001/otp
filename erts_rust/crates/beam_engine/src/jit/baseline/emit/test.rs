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

//! The baseline JIT's `test` subset: the guard/plan shape test and its lowering
//! (the section comment below documents exactly which predicates the tier may lower
//! natively).

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand, TestKind, NO_FAIL};
use beam_term::{
    make_atom, MAX_ARITYVAL, NIL, TAG_IMMED2_ATOM, TAG_IMMED2_MASK, TAG_PRIMARY_BOXED,
    TAG_PRIMARY_LIST, TAG_PRIMARY_MASK,
};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::arith::{FAST_C, SMALL_MASK};
use super::link::Linker;
use super::operands::{small_word, x_offset, x_register};
use super::{emit_helper_call, emit_refresh_y, emit_step_and_continue, fill, touch, Allocation, FillSite};
use crate::jit::baseline::runtime::StepHelper;
use crate::jit::baseline::test_helper::{jit_test, TEST_FALSE, TEST_TRUE};

// ---------------------------------------------------------------------------
// The `test` subset
// ---------------------------------------------------------------------------
//
// `test` decides every clause head, guard and `case` arm, so leaving it to
// `jit_step_one` costs a full helper round-trip for the most frequently executed
// non-arithmetic instruction. The subset below is lowered natively — and only where
// the emitted check is **provably** the interpreter's result (`eval_test` in
// `crate::interpreter`, per-kind line numbers cited where they matter):
//
// * the pure word tests `is_nil`/`is_atom`/`is_list`/`is_nonempty_list`/`is_boolean`,
//   which never dereference a term and mirror `beam_term`'s const predicates:
//   `is_nil` is `word == NIL`; `is_atom` is `(word & 0x3F) == 0xB`; and the list
//   tests are the C bit trick `(word & (_TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) == 0`
//   (`is_list` additionally accepts `NIL`);
// * the **boxed-term header tests** (`is_integer`, `is_number`, `is_float`, `is_tuple`,
//   `is_map`, `is_bitstring`, `is_binary`, `is_function`, `is_function2`,
//   `is_tagged_tuple`, `test_arity`, `is_pid`, `is_port`, `is_reference`), each a
//   pure tag/header check over **one** `mov` of the boxed header — see
//   [`test_plan`]'s arms for the `beam_term` predicate every constant mirrors;
// * `is_eq_exact`/`is_ne_exact` with a constant-immediate operand — an immediate is
//   `=:=` only to the identical word, so a plain word compare is exact. Two
//   registers are **not** inlined: two distinct boxed terms (equal floats, bignums,
//   binaries, tuples, …) are `=:=` with different words;
// * `is_eq`/`is_ne` — `==` is the one comparison where immediates are not
//   self-sufficient (it equates `1` and `1.0`), so a **small-int** operand's register
//   counterpart is guarded as a small int first. An atom/`nil` operand is never
//   arithmetically equal to anything but itself, so it needs no guard;
// * `is_lt`/`is_ge` — only two small-int words are safe, because the tagged-word
//   *signed* order is the value order (`make_small(v) == v*16 + 15` is monotonic)
//   while `cmp_arith` otherwise falls back to the term classes; an atom/`nil`
//   constant is therefore never inlined for these two.
//
// A guard rejection branches to **one** `jit_step_one` call that then continues
// ([`emit_step_and_continue`]), so a float/bignum/atom operand costs exactly what it
// did before. A test failure — the word tests *and* every header test, whose
// predicate is total over live terms — branches to the resolved `fail` label's
// `(func, pc)` — a `(spill, return-word, exit)` — while a `NO_FAIL` test (which
// *raises*), an unresolved label (whose exact `unknown_label` must surface) and an
// arity operand that is not a plain `{integer,A}` (which the interpreter *raises* on)
// keep the whole instruction on the helper.
//
// Every header test dereferences **exactly the one word** `beam_term`'s predicate
// reads, and only after proving `(value & 3) == 2` — i.e. `beam_term::is_boxed_term`
// (`beam_term::boxed_val` masks [`PTR_MASK`] and `is_boxed_term` is `is_boxed(x) &&
// is_term_word(x)`, which is exactly `(raw & 3) == TAG_PRIMARY_BOXED`; see
// `boxed.rs::is_boxed_term`/`tag.rs::is_term_word`). A word that is not a boxed term
// is a definite `false` for every one of these predicates, so it takes the fail path
// without touching memory. Nothing outside the block's own `machine.x` /

/// The scratch register holding a `test`'s first operand while its predicate is
/// evaluated.
///
/// All four are caller-saved (System V AMD64), so clobbering them within one
/// instruction's lowering is free, and none is a cache register ([`super::CACHE_REGISTERS`])
/// or the block's reserved `rbx` (the `machine.x` base) / `rsp` (the `rt` slot).
pub(super) const TEST_A: Reg = Reg::Rax;
/// The second operand's scratch register.
const TEST_B: Reg = Reg::R8;
/// The predicate's staging temporary (`mov`/`and` operand of a word test, and the
/// loaded header word / tuple element of a header test).
pub(super) const TEST_T: Reg = Reg::R10;
/// The header test's boxed-pointer scratch ([`load_boxed_header`]).
const TEST_C: Reg = Reg::R9;
/// The header test's staging scratch for the **masked** header value ([`header_matches`]).
///
/// It must not be one of the operand/pointer scratch registers: every caller passes
/// the loaded header in [`TEST_T`] and several of them (the float branch of
/// [`emit_is_number`], the sub-bits branch of [`emit_is_binary`], the arity field of
/// [`emit_is_fun_arity`]) go on to read that header again, so the predicate has to
/// test it **without consuming it**. `R11` is the emitter's existing transient
/// scratch (short-lived uses in `emit_helper_call`/`emit_step_and_continue`), so it
/// is free for the length of one predicate.
pub(super) const TEST_H: Reg = Reg::R11;

/// `beam_term::TAG_IMMED2_MASK` as an `i64` — the low-six-bits mask `is_atom` tests.
const ATOM_MASK: i64 = TAG_IMMED2_MASK as i64;
/// `beam_term::TAG_IMMED2_ATOM` as an `i64` — the atom immediate tag.
const ATOM_TAG: i64 = TAG_IMMED2_ATOM as i64;
/// `TAG_PRIMARY_MASK - TAG_PRIMARY_LIST` as an `i64`: `(word & LIST_MASK) == 0` is
/// exactly `beam_term::is_not_nil_list` (the C bit trick; the literal-pointer bit is
/// handled by the mask, as in the C).
const LIST_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST) as i64;
/// The raw word of `NIL`.
const TEST_NIL: i64 = NIL.raw() as i64;

/// `beam_term::TAG_PRIMARY_MASK` as an `i64` — the two-bit primary tag.
const PRIMARY_MASK: i64 = TAG_PRIMARY_MASK as i64;
/// `beam_term::TAG_PRIMARY_BOXED` as an `i64` — the primary tag of a boxed term, so
/// `(word & PRIMARY_MASK) == BOXED_TAG` is exactly `beam_term::is_boxed_term`
/// (`is_boxed(word) && is_term_word(word)`).
const BOXED_TAG: i64 = TAG_PRIMARY_BOXED as i64;
/// `!beam_term::TAG_PTR_MASK` — the mask turning a boxed term word into the pointer
/// `beam_term::boxed_val` reads.
const PTR_MASK: i64 = !(beam_term::TAG_PTR_MASK as i64);
/// `beam_term::TAG_HEADER_MASK` — the header subtag mask (arity values, maps, refs,
/// external pids/ports/refs).
const HEADER_MASK: i64 = 0x3F;
/// `beam_term::HEADER_SUBTAG_MASK` — the fun header's four-bit subtag mask.
const HEADER_KIND_MASK: i64 = 0x3C;
/// `beam_term::BITSTRING_TAG_MASK` — the bitstring header mask (heap **and** sub bits).
const BITSTRING_MASK: i64 = 0x3B;
/// `beam_term::BIG_TAG_MASK` — the bignum header mask.
const BIG_MASK: i64 = 0x3B;
/// `beam_term::ARITYVAL_SUBTAG` — a tuple's header subtag.
const ARITYVAL_SUBTAG: i64 = 0x00;
/// `beam_term::MAP_SUBTAG`.
const MAP_SUBTAG: i64 = 0x2C;
/// `beam_term::HEAP_BITS_SUBTAG`.
const HEAP_BITS_SUBTAG: i64 = 0x20;
/// `beam_term::FUN_SUBTAG`.
const FUN_SUBTAG: i64 = 0x14;
/// `beam_term::POS_BIG_SUBTAG` — a bignum's header subtag.
const POS_BIG_SUBTAG: i64 = 0x08;
/// `beam_term::REF_SUBTAG` — an internal reference's header subtag.
const REF_SUBTAG: i64 = 0x10;
/// `beam_term::boxed::EXTERNAL_PID_SUBTAG` (an external pid's header subtag).
const EXTERNAL_PID_SUBTAG: i64 = 0x30;
/// `beam_term::boxed::EXTERNAL_PORT_SUBTAG` (an external port's header subtag).
const EXTERNAL_PORT_SUBTAG: i64 = 0x34;
/// `beam_term::boxed::EXTERNAL_REF_SUBTAG` (an external reference's header subtag).
const EXTERNAL_REF_SUBTAG: i64 = 0x38;
/// `beam_term::HEADER_FLONUM` — the `(1, FLOAT_SUBTAG)` header of a boxed float.
const HEADER_FLONUM: i64 = 0x58;
/// `beam_term::HEADER_SUB_BITS` — the `(4, SUB_BITS_SUBTAG)` header of an `ErlSubBits`.
const HEADER_SUB_BITS: i64 = 0x124;
/// `beam_term::TAG_IMMED1_MASK` — the four-bit immediate-1 tag mask.
const IMMED1_MASK: i64 = 0xF;
/// `beam_term::TAG_IMMED1_PID` — an internal pid is `(data << 4) | 0x3`.
const INTERNAL_PID_TAG: i64 = 0x3;
/// `beam_term::TAG_IMMED1_PORT` — an internal port is `(data << 4) | 0x7`.
const INTERNAL_PORT_MASK: i64 = 0x7;
/// `beam_term::TAG_IMMED1_PORT` as the tag half of the `(word & 0x7) == 0x7` test.
const INTERNAL_PORT_TAG: i64 = 0x7;
/// The `is_binary` byte-alignment mask (`(bitstring_size(bin) & 7) == 0`).
const BYTE_MASK: i64 = 7;
/// `beam_term::FUN_HEADER_ARITY_OFFS` in bits — a fun header's arity field is
/// `(hdr >> 8) & 0xFF` (`beam_term::fun::fun_arity`).
const FUN_ARITY_SHIFT: u32 = 8;
/// `beam_term::fun::FUN_MAX_FIELD`.
const FUN_ARITY_MASK: i64 = 0xFF;
/// `beam_term::HEADER_ARITY_OFFS` — a tuple's header is `arity << 6`.
const ARITYVAL_SHIFT: u32 = 6;

/// Which `test` operands need the small-int guard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    /// No operand is a runtime value (both are constants), or the comparison is
    /// exact over immediate words.
    None,
    /// Guard the first operand.
    Lhs,
    /// Guard the second operand.
    Rhs,
    /// Guard both operands.
    Both,
}

/// One `test` operand the tier can read without the helper.
#[derive(Clone, Copy)]
pub(super) enum TestSrc {
    /// An x-register inside the engine's [`MAX_REGISTER`] bound.
    Reg(u32),
    /// A small-int constant (`{integer,V}`/`{char,C}` in the 60-bit range) — the only
    /// numeric constant that is word-order exact.
    Small(i64),
    /// An atom or `nil` constant — a **non-numeric** immediate.
    Immediate(i64),
}

/// The predicate of an inlinable `test`; a jump to the success label is taken when it
/// holds.
enum TestPred {
    /// `is_nil` — the word is `NIL`.
    IsNil,
    /// `is_atom` — the word carries the atom immediate tag.
    IsAtom,
    /// `is_list` — `is_nil(v) || is_not_nil_list(v)`.
    IsList,
    /// `is_nonempty_list` — a cons cell.
    IsNonemptyList,
    /// `is_boolean` — the word is the module's `true` or `false` atom word; a name the
    /// module never interned contributes nothing.
    IsBoolean {
        /// The raw word of the module's `true` atom, when interned.
        truthy: Option<i64>,
        /// The raw word of the module's `false` atom, when interned.
        falsy: Option<i64>,
    },
    /// `is_integer` — a small word **or** a bignum (`boxed.rs::is_bignum_header`).
    /// Mirrors `beam_term::is_integer` (`ops.rs`: `is_term_word(x) && (is_small(x) ||
    /// is_big(x))`; the `is_term_word` shield is vacuous for both disjuncts, since a
    /// small is an `IMMED1` word and `is_big` itself requires a boxed term).
    IsInteger,
    /// `is_number` — a small, a bignum or a float (`beam_term::is_number`).
    IsNumber,
    /// `is_float` — a boxed `HEADER_FLONUM` (`boxed.rs::is_float`).
    IsFloat,
    /// A boxed term whose header matches `(hdr & mask) == tag`:
    /// `is_tuple` (`is_arity_value`), `is_map` (`is_map_header`),
    /// `is_bitstring` (`is_bitstring_header`), `is_function` (`is_fun_header`).
    IsHeader {
        /// The header mask to apply.
        mask: i64,
        /// The masked value a match expects.
        tag: i64,
    },
    /// `is_binary` — a bitstring whose `beam_term::bitstring_size` is byte-aligned.
    IsBinary,
    /// `is_tuple_arity` — the **full** arity-value header equality
    /// `*boxed_val(v) == make_arityval_unchecked(arity)` (i.e. `hdr == arity << 6`).
    IsArity {
        /// The arity the header must equal.
        arity: i64,
    },
    /// `is_function2` — a fun header (`is_fun_header`) whose arity field
    /// (`beam_term::fun::fun_arity`) equals `arity`.
    IsFunArity {
        /// The arity the fun header must carry.
        arity: i64,
    },
    /// `is_tagged_tuple` — an arity-value header of `arity` whose element 0 (in
    /// `TEST_B`, loaded from [`TestPlan::rhs`]) is the tag.
    IsTaggedTuple {
        /// The arity the header must equal.
        arity: i64,
    },
    /// `is_pid` — an internal-pid immediate (`(word & 0xF) == 0x3`) or a boxed
    /// external-pid header.
    IsPid,
    /// `is_port` — an internal-port immediate (`(word & 0x7) == 0x7`) or a boxed
    /// external-port header.
    IsPort,
    /// `is_reference` — a boxed internal (`REF_SUBTAG`) or external
    /// (`EXTERNAL_REF_SUBTAG`) reference header.
    IsReference,
    /// A word compare: success when `A <cond> B` holds.
    Cmp(Cond),
}

/// An inlinable `test`.
pub(super) struct TestPlan {
    /// The first operand's source.
    pub(super) lhs: TestSrc,
    /// The second operand's source (a comparison, or an `is_tagged_tuple`'s tag).
    pub(super) rhs: Option<TestSrc>,
    /// The guard the comparison needs.
    guard: Guard,
    /// The predicate to evaluate over the loaded operands.
    pred: TestPred,
    /// The resolved fail label's `(func, pc)`.
    fail: (u32, u32),
}

/// The inlinable shape of `instr`, or `None` when the whole instruction must stay on
/// the helper.
pub(super) fn test_plan(
    module: &Module,
    kind: TestKind,
    fail: Label,
    args: &[Operand],
    labels: &LabelMap,
) -> Option<TestPlan> {
    // A `NO_FAIL` test *raises* on failure; the helper reproduces that exactly.
    if fail == NO_FAIL {
        return None;
    }
    // An unresolved label keeps the interpreter's exact `unknown_label`.
    let target = labels.resolve(fail).ok()?;

    // The tagged-tuple tag is the plan's second source (the compiled element-0 tag
    // compare reuses the operand load + the guard-free second slot).
    let mut tagged_tag: Option<TestSrc> = None;

    let pred = match kind {
        TestKind::IsNil => TestPred::IsNil,
        TestKind::IsAtom => TestPred::IsAtom,
        TestKind::IsList => TestPred::IsList,
        TestKind::IsNonemptyList => TestPred::IsNonemptyList,
        TestKind::IsBoolean => {
            let truthy = module
                .atom_table
                .lookup("true")
                .map(|atom| make_atom(atom).raw() as i64);
            let falsy = module
                .atom_table
                .lookup("false")
                .map(|atom| make_atom(atom).raw() as i64);
            if truthy.is_none() && falsy.is_none() {
                // `is_boolean` can never succeed in this module; the helper is exact.
                return None;
            }
            TestPred::IsBoolean { truthy, falsy }
        }
        // --- the boxed-term header tests ------------------------------------
        // `is_integer`/`is_number`: `beam_term::is_integer`/`is_number` (`ops.rs`).
        TestKind::IsInteger => TestPred::IsInteger,
        TestKind::IsNumber => TestPred::IsNumber,
        // `beam_term::boxed::is_float`.
        TestKind::IsFloat => TestPred::IsFloat,
        // `beam_term::tuple::is_tuple` -> `boxed.rs::is_arity_value`.
        TestKind::IsTuple => TestPred::IsHeader {
            mask: HEADER_MASK,
            tag: ARITYVAL_SUBTAG,
        },
        // `beam_term::boxed::is_map_header`.
        TestKind::IsMap => TestPred::IsHeader {
            mask: HEADER_MASK,
            tag: MAP_SUBTAG,
        },
        // `beam_term::boxed::is_bitstring` -> `is_bitstring_header`.
        TestKind::IsBitstr => TestPred::IsHeader {
            mask: BITSTRING_MASK,
            tag: HEAP_BITS_SUBTAG,
        },
        // `crate::terms::is_fun` -> `beam_term::is_any_fun` -> `is_fun_header`.
        TestKind::IsFunction => TestPred::IsHeader {
            mask: HEADER_KIND_MASK,
            tag: FUN_SUBTAG,
        },
        // `beam_term::boxed::is_binary` (`is_bitstring` + `bitstring_size & 7 == 0`).
        TestKind::IsBinary => TestPred::IsBinary,
        // `is_function2` needs an 8-bit-slice-equal arity operand; anything else stays
        // on the helper (which reproduces the interpreter's typed error / range).
        TestKind::IsFunction2 => TestPred::IsFunArity {
            arity: arity_operand(args.get(1), FUN_ARITY_MASK)?,
        },
        // `test_arity`: `is_boxed_term(v) && hdr == make_arityval_unchecked(arity)`,
        // with the interpreter's `0..=MAX_ARITYVAL` operand range.
        TestKind::TestArity => TestPred::IsArity {
            arity: arity_operand(args.get(1), MAX_ARITYVAL as i64)?,
        },
        // `is_tagged_tuple`: the same arity-value equality, but the interpreter rejects
        // `arity == 0` up front, then compares element 0 with the tag operand.
        TestKind::IsTaggedTuple => {
            let arity = arity_operand(args.get(1), MAX_ARITYVAL as i64)?;
            if arity == 0 {
                // The interpreter returns `false` for every arity outside `1..=MAX`;
                // the helper reproduces that exactly.
                return None;
            }
            let tag = test_src(args.get(2)?)?;
            tagged_tag = Some(tag);
            TestPred::IsTaggedTuple { arity }
        }
        // `is_pid`/`is_port`: the internal immediate tag, or the boxed external header.
        TestKind::IsPid => TestPred::IsPid,
        TestKind::IsPort => TestPred::IsPort,
        // `is_reference`: always boxed (`REF_SUBTAG` or `EXTERNAL_REF_SUBTAG`).
        TestKind::IsReference => TestPred::IsReference,
        _ => {
            let lhs = test_src(args.first()?)?;
            let rhs = test_src(args.get(1)?)?;
            let (guard, cond) = compare_shape(kind, lhs, rhs)?;
            return Some(TestPlan {
                lhs,
                rhs: Some(rhs),
                guard,
                pred: TestPred::Cmp(cond),
                fail: target,
            });
        }
    };

    Some(TestPlan {
        lhs: test_src(args.first()?)?,
        rhs: tagged_tag,
        guard: Guard::None,
        pred,
        fail: target,
    })
}

/// The plain `{integer,A}` operand of an arity test, or `None` when the operand needs
/// the helper — either because it is not an integer operand at all (the interpreter
/// *raises* a typed error on it) or because `A` is outside `0..=max` (where the
/// interpreter's own range guard decides).
fn arity_operand(operand: Option<&Operand>, max: i64) -> Option<i64> {
    match operand? {
        Operand::Integer(arity) if (0..=max).contains(arity) => Some(*arity),
        _ => None,
    }
}

/// The native source of one operand, or `None` when it needs the helper: a `Y`
/// register, a float register, a literal, a bignum constant, or an x-register past
/// the engine's [`MAX_REGISTER`] bound (whose exact `bad_register` must surface).
pub(super) fn test_src(operand: &Operand) -> Option<TestSrc> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => Some(TestSrc::Reg(x)),
        Some(_) => None,
        None => match operand {
            Operand::Integer(value) => small_word(*value).map(TestSrc::Small),
            Operand::Character(c) => small_word(i64::from(*c)).map(TestSrc::Small),
            Operand::Atom(atom) => Some(TestSrc::Immediate(make_atom(*atom).raw() as i64)),
            Operand::Nil => Some(TestSrc::Immediate(TEST_NIL)),
            _ => None,
        },
    }
}

/// The `(guard, success condition)` of an inlinable comparison, or `None` when the
/// shape is not provably word-exact.
fn compare_shape(kind: TestKind, lhs: TestSrc, rhs: TestSrc) -> Option<(Guard, Cond)> {
    let is_const =
        |src: TestSrc| matches!(src, TestSrc::Small(_) | TestSrc::Immediate(_));
    match kind {
        TestKind::IsEqExact | TestKind::IsNeExact => {
            let cond = if kind == TestKind::IsEqExact {
                Cond::E
            } else {
                Cond::Ne
            };
            match (lhs, rhs) {
                // Two runtime values: guard **both** as small ints, after which a plain
                // word compare *is* `=:=` — `make_small(v) == v*16 + 15` is injective
                // and `cmp`'s raw-word fast path is exact. Two distinct boxed terms
                // (equal floats, bignums, tuples, …) share no word, but neither is a
                // small, so the guard sends them to the helper (which compares `cmp`'s
                // own way) rather than fabricating a word-compare answer.
                (TestSrc::Reg(_), TestSrc::Reg(_)) => Some((Guard::Both, cond)),
                // At least one operand is a constant immediate, which is `=:=` only to
                // the identical word (see the module comment): a plain word compare is
                // already exact.
                _ if is_const(lhs) || is_const(rhs) => Some((Guard::None, cond)),
                _ => None,
            }
        }
        TestKind::IsEq | TestKind::IsNe => {
            let cond = if kind == TestKind::IsEq { Cond::E } else { Cond::Ne };
            Some((arith_guard(lhs, rhs)?, cond))
        }
        TestKind::IsLt | TestKind::IsGe => {
            let cond = if kind == TestKind::IsLt { Cond::L } else { Cond::Ge };
            Some((order_guard(lhs, rhs)?, cond))
        }
        _ => None,
    }
}

/// The guard an **arithmetic** equality (`is_eq`/`is_ne`) needs for the word compare
/// to reproduce `cmp_arith` exactly.
///
/// `==` is the only comparison where an immediate is not self-sufficient: it treats
/// `1` and `1.0` as equal, so a small-int operand needs its register counterpart
/// guarded. An atom/`nil` immediate is `==` only to the identical word, so it needs
/// no guard; two small-int constants need none either.
fn arith_guard(lhs: TestSrc, rhs: TestSrc) -> Option<Guard> {
    match (lhs, rhs) {
        (TestSrc::Immediate(_), _) | (_, TestSrc::Immediate(_)) => Some(Guard::None),
        (TestSrc::Small(_), TestSrc::Small(_)) => Some(Guard::None),
        (TestSrc::Small(_), TestSrc::Reg(_)) => Some(Guard::Rhs),
        (TestSrc::Reg(_), TestSrc::Small(_)) => Some(Guard::Lhs),
        (TestSrc::Reg(_), TestSrc::Reg(_)) => Some(Guard::Both),
    }
}

/// The guard an **ordering** compare (`is_lt`/`is_ge`) needs: only two small-int
/// words are word-order exact, so an atom/`nil` (or bignum/literal) constant keeps
/// the instruction on the helper.
fn order_guard(lhs: TestSrc, rhs: TestSrc) -> Option<Guard> {
    match (lhs, rhs) {
        (TestSrc::Small(_), TestSrc::Small(_)) => Some(Guard::None),
        (TestSrc::Small(_), TestSrc::Reg(_)) => Some(Guard::Rhs),
        (TestSrc::Reg(_), TestSrc::Small(_)) => Some(Guard::Lhs),
        (TestSrc::Reg(_), TestSrc::Reg(_)) => Some(Guard::Both),
        _ => None,
    }
}

/// Load a `test` operand into scratch `reg`.
pub(super) fn load_test_src(
    buffer: &mut CodeBuffer,
    src: TestSrc,
    reg: Reg,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match src {
        TestSrc::Reg(x) => match allocation.slot(x) {
            Some(cached) => buffer.mov_rr(reg, cached),
            None => {
                touch(highest_touched, x);
                buffer.mov_rm(reg, Reg::Rbx, x_offset(x))
            }
        },
        TestSrc::Small(word) | TestSrc::Immediate(word) => buffer.mov_ri(reg, word),
    }
}

/// Branch to `deopt` unless `reg` holds a small integer (`(word & 0xF) == 0xF`).
///
/// `FAST_C` must already hold [`SMALL_MASK`] — the tag and the mask are the same
/// nibble (asserted below), exactly as in [`super::arith::try_inline_gc_bif`].
fn guard_small(buffer: &mut CodeBuffer, reg: Reg, deopt: CodeLabel) -> Result<()> {
    buffer.mov_rr(TEST_T, reg)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::Ne, deopt)
}

// ---------------------------------------------------------------------------
// Header-test helpers
// ---------------------------------------------------------------------------

/// Load the **header word** of the boxed term in `value` into `out`, leaving its
/// masked pointer in `ptr`, or branch to `failed` when `value` is not a boxed term.
///
/// `(value & 3) == TAG_PRIMARY_BOXED` is exactly `beam_term::is_boxed_term`
/// (`is_boxed(value) && is_term_word(value)`), so passing it makes the single
/// `[ptr]` read below — the one word `beam_term`'s own predicate reads — safe. A
/// non-boxed word is a definite `false` for **every** header predicate, so it takes
/// the fail path without any dereference.
fn load_boxed_header(
    buffer: &mut CodeBuffer,
    value: Reg,
    ptr: Reg,
    out: Reg,
    failed: CodeLabel,
) -> Result<()> {
    buffer.mov_ri(FAST_C, PRIMARY_MASK)?;
    buffer.mov_rr(ptr, value)?;
    buffer.and_rr(ptr, FAST_C)?;
    buffer.mov_ri(out, BOXED_TAG)?;
    buffer.cmp_rr(ptr, out)?;
    buffer.jcc(Cond::Ne, failed)?;
    // `ptr` = boxed_val(value): mask off TAG_PTR_MASK and read the header word.
    //
    // `ptr` currently holds the *primary tag* of `value` (the `2` the guard just
    // matched), **not** `value`, so reload the term word first: masking the tag
    // value with `PTR_MASK` would leave `2 & !0x7 == 0` and the header read below
    // would dereference a null pointer.
    buffer.mov_rr(ptr, value)?;
    buffer.mov_ri(FAST_C, PTR_MASK)?;
    buffer.and_rr(ptr, FAST_C)?;
    buffer.mov_rm(out, ptr, 0)
}

/// `jcc E, success` when `(reg & mask) == tag` — the header predicate itself.
///
/// `reg` is **preserved**: the masked value is staged in [`TEST_H`], not in `reg`, so a
/// caller that still needs the header word (the `HEADER_FLONUM` branch of
/// [`emit_is_number`], the `HEADER_SUB_BITS` branch of [`emit_is_binary`], the arity
/// field of [`emit_is_fun_arity`]) can keep reading it.
fn header_matches(buffer: &mut CodeBuffer, reg: Reg, mask: i64, tag: i64, success: CodeLabel) -> Result<()> {
    buffer.mov_ri(FAST_C, mask)?;
    buffer.mov_rr(TEST_H, reg)?;
    buffer.and_rr(TEST_H, FAST_C)?;
    buffer.mov_ri(FAST_C, tag)?;
    buffer.cmp_rr(TEST_H, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `jcc E, success` when `(word & 0xF) == 0xF` — the small-int immediate test
/// (`beam_term::is_small`).
fn jump_if_small(buffer: &mut CodeBuffer, reg: Reg, success: CodeLabel) -> Result<()> {
    buffer.mov_ri(FAST_C, SMALL_MASK)?;
    buffer.mov_rr(TEST_T, reg)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `jcc E, success` when the immediate tag test `(word & mask) == tag` holds (an
/// internal pid/port).
fn jump_if_immediate(
    buffer: &mut CodeBuffer,
    reg: Reg,
    mask: i64,
    tag: i64,
    success: CodeLabel,
) -> Result<()> {
    buffer.mov_ri(FAST_C, mask)?;
    buffer.mov_rr(TEST_T, reg)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.mov_ri(TEST_C, tag)?;
    buffer.cmp_rr(TEST_T, TEST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_integer`: a small word, else a boxed bignum header.
fn emit_is_integer(buffer: &mut CodeBuffer, success: CodeLabel, failed: CodeLabel) -> Result<()> {
    jump_if_small(buffer, TEST_A, success)?;
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, BIG_MASK, POS_BIG_SUBTAG, success)
}

/// `is_number`: a small word, else a boxed bignum or `HEADER_FLONUM` header.
fn emit_is_number(buffer: &mut CodeBuffer, success: CodeLabel, failed: CodeLabel) -> Result<()> {
    jump_if_small(buffer, TEST_A, success)?;
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, BIG_MASK, POS_BIG_SUBTAG, success)?;
    buffer.mov_ri(FAST_C, HEADER_FLONUM)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_float`: a boxed `HEADER_FLONUM`.
fn emit_is_float(buffer: &mut CodeBuffer, success: CodeLabel, failed: CodeLabel) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    buffer.mov_ri(FAST_C, HEADER_FLONUM)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_binary`: a bitstring header whose `bitstring_size` is byte-aligned. The size is
/// read exactly as `beam_term::boxed::bitstring_size` reads it — `p[3] - p[2]` for an
/// `ErlSubBits` (`HEADER_SUB_BITS`), otherwise the heap-bits size word `p[1]`.
fn emit_is_binary(buffer: &mut CodeBuffer, success: CodeLabel, failed: CodeLabel) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    let bits = buffer.label();
    let sub_bits = buffer.label();
    let size_ready = buffer.label();
    header_matches(buffer, TEST_T, BITSTRING_MASK, HEAP_BITS_SUBTAG, bits)?;
    buffer.jmp(failed)?;
    buffer.bind(bits)?;
    buffer.mov_ri(FAST_C, HEADER_SUB_BITS)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, sub_bits)?;
    // ErlHeapBits: [thing_word, size, data...]
    buffer.mov_rm(TEST_T, TEST_C, 8)?;
    buffer.jmp(size_ready)?;
    buffer.bind(sub_bits)?;
    // ErlSubBits: [thing_word, base_flags, start, end, orig]
    buffer.mov_rm(TEST_T, TEST_C, 16)?;
    buffer.mov_rm(TEST_C, TEST_C, 24)?;
    buffer.sub_rr(TEST_C, TEST_T)?;
    buffer.mov_rr(TEST_T, TEST_C)?;
    buffer.bind(size_ready)?;
    buffer.mov_ri(FAST_C, BYTE_MASK)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_tuple_arity`: the full arity-value header equality (`hdr == arity << 6`).
fn emit_is_arity(
    buffer: &mut CodeBuffer,
    arity: i64,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    buffer.mov_ri(FAST_C, arity << ARITYVAL_SHIFT)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_function2`: a fun header whose arity field equals `arity`.
fn emit_is_fun_arity(
    buffer: &mut CodeBuffer,
    arity: i64,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    let is_fun = buffer.label();
    header_matches(buffer, TEST_T, HEADER_KIND_MASK, FUN_SUBTAG, is_fun)?;
    buffer.jmp(failed)?;
    buffer.bind(is_fun)?;
    // `beam_term::fun::fun_arity` is `(hdr >> FUN_HEADER_ARITY_OFFS) & FUN_MAX_FIELD`
    // (an arithmetic shift is identical under the `& 0xFF` mask).
    buffer.sar_ri(TEST_T, FUN_ARITY_SHIFT)?;
    buffer.mov_ri(FAST_C, FUN_ARITY_MASK)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.mov_ri(TEST_C, arity)?;
    buffer.cmp_rr(TEST_T, TEST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_tagged_tuple`: an arity-value header of `arity` whose element 0 equals the tag
/// already loaded into `TEST_B` (`beam_term::tuple_element(v, 0)`).
fn emit_is_tagged_tuple(
    buffer: &mut CodeBuffer,
    arity: i64,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    buffer.mov_ri(FAST_C, arity << ARITYVAL_SHIFT)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::Ne, failed)?;
    buffer.mov_rm(TEST_T, TEST_C, 8)?;
    buffer.cmp_rr(TEST_T, TEST_B)?;
    buffer.jcc(Cond::E, success)
}

/// `is_pid`: an internal-pid immediate or a boxed external-pid header.
fn emit_is_pid(buffer: &mut CodeBuffer, success: CodeLabel, failed: CodeLabel) -> Result<()> {
    jump_if_immediate(buffer, TEST_A, IMMED1_MASK, INTERNAL_PID_TAG, success)?;
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, HEADER_MASK, EXTERNAL_PID_SUBTAG, success)
}

/// `is_port`: an internal-port immediate or a boxed external-port header.
fn emit_is_port(buffer: &mut CodeBuffer, success: CodeLabel, failed: CodeLabel) -> Result<()> {
    jump_if_immediate(
        buffer,
        TEST_A,
        INTERNAL_PORT_MASK,
        INTERNAL_PORT_TAG,
        success,
    )?;
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, HEADER_MASK, EXTERNAL_PORT_SUBTAG, success)
}

/// `is_reference`: a boxed internal or external reference header.
fn emit_is_reference(buffer: &mut CodeBuffer, success: CodeLabel, failed: CodeLabel) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, HEADER_MASK, REF_SUBTAG, success)?;
    header_matches(buffer, TEST_T, HEADER_MASK, EXTERNAL_REF_SUBTAG, success)
}

/// Lower an inlinable `test` natively, or report that the whole instruction must be
/// handed to the helper.
///
/// Returns `false` **without having emitted a byte** for any shape [`test_plan`]
/// refuses (a `NO_FAIL` test, an unresolved fail label, a non-integer/out-of-range
/// arity operand, an inexact comparison, a `Y`/float/literal operand), so the caller
/// falls back to exactly today's `jit_step_one`.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_test(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    linker: &Linker<'_>,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Instr::Test { kind, fail, args } = instr else {
        return Ok(false);
    };
    let Some(plan) = test_plan(module, *kind, *fail, args, labels) else {
        // Not one of the word/header shapes the inline subset can decide: a
        // comparison `test` (a `Y`/float/literal operand, a boxed-vs-boxed compare) is
        // lowered through the `jit_test` helper instead; anything else — a `NO_FAIL`
        // form, an unresolved fail label, a type test — keeps today's per-instruction
        // deopt.
        return emit_test_helper(buffer, labels, linker, instr, allocation, done, func, pc);
    };

    let after: CodeLabel = buffer.label();
    // The "predicate is definitely false" exit, bound immediately before the fail
    // exit below: the word tests (and every header test) reach it either by falling
    // through the predicate or by an explicit `jcc`.
    let failed: CodeLabel = buffer.label();
    let deopt: Option<CodeLabel> = (plan.guard != Guard::None).then(|| buffer.label());

    // --- operands -----------------------------------------------------------
    load_test_src(buffer, plan.lhs, TEST_A, allocation, highest_touched)?;
    if let Some(rhs) = plan.rhs {
        load_test_src(buffer, rhs, TEST_B, allocation, highest_touched)?;
    }

    // --- the small-int guard ------------------------------------------------
    if let Some(deopt) = deopt {
        buffer.mov_ri(FAST_C, SMALL_MASK)?;
        if matches!(plan.guard, Guard::Lhs | Guard::Both) {
            guard_small(buffer, TEST_A, deopt)?;
        }
        if matches!(plan.guard, Guard::Rhs | Guard::Both) {
            guard_small(buffer, TEST_B, deopt)?;
        }
    }

    // --- the predicate (jump to `after` when the test SUCCEEDS) -------------
    match &plan.pred {
        TestPred::IsNil => {
            buffer.mov_ri(TEST_T, TEST_NIL)?;
            buffer.cmp_rr(TEST_A, TEST_T)?;
            buffer.jcc(Cond::E, after)?;
        }
        TestPred::IsAtom => {
            buffer.mov_ri(FAST_C, ATOM_MASK)?;
            buffer.mov_rr(TEST_T, TEST_A)?;
            buffer.and_rr(TEST_T, FAST_C)?;
            buffer.mov_ri(FAST_C, ATOM_TAG)?;
            buffer.cmp_rr(TEST_T, FAST_C)?;
            buffer.jcc(Cond::E, after)?;
        }
        TestPred::IsNonemptyList => {
            buffer.mov_ri(FAST_C, LIST_MASK)?;
            buffer.mov_rr(TEST_T, TEST_A)?;
            buffer.and_rr(TEST_T, FAST_C)?;
            buffer.jcc(Cond::E, after)?;
        }
        TestPred::IsList => {
            buffer.mov_ri(TEST_T, TEST_NIL)?;
            buffer.cmp_rr(TEST_A, TEST_T)?;
            buffer.jcc(Cond::E, after)?;
            buffer.mov_ri(FAST_C, LIST_MASK)?;
            buffer.mov_rr(TEST_T, TEST_A)?;
            buffer.and_rr(TEST_T, FAST_C)?;
            buffer.jcc(Cond::E, after)?;
        }
        TestPred::IsBoolean { truthy, falsy } => {
            for word in [*truthy, *falsy].into_iter().flatten() {
                buffer.mov_ri(TEST_T, word)?;
                buffer.cmp_rr(TEST_A, TEST_T)?;
                buffer.jcc(Cond::E, after)?;
            }
        }
        TestPred::Cmp(cond) => {
            buffer.cmp_rr(TEST_A, TEST_B)?;
            buffer.jcc(*cond, after)?;
        }
        // --- the boxed-term header tests ------------------------------------
        TestPred::IsInteger => emit_is_integer(buffer, after, failed)?,
        TestPred::IsNumber => emit_is_number(buffer, after, failed)?,
        TestPred::IsFloat => emit_is_float(buffer, after, failed)?,
        TestPred::IsBinary => emit_is_binary(buffer, after, failed)?,
        TestPred::IsPid => emit_is_pid(buffer, after, failed)?,
        TestPred::IsPort => emit_is_port(buffer, after, failed)?,
        TestPred::IsReference => emit_is_reference(buffer, after, failed)?,
        TestPred::IsHeader { mask, tag } => {
            load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
            header_matches(buffer, TEST_T, *mask, *tag, after)?;
        }
        TestPred::IsArity { arity } => emit_is_arity(buffer, *arity, after, failed)?,
        TestPred::IsFunArity { arity } => emit_is_fun_arity(buffer, *arity, after, failed)?,
        TestPred::IsTaggedTuple { arity } => {
            emit_is_tagged_tuple(buffer, *arity, after, failed)?
        }
    }

    // --- the test failed: branch to the fail label --------------------------
    // A compiled fail block is entered directly (the linker spills the cached
    // x-registers, leaves this frame and jumps to its entry); any other fail
    // target returns its `(func, pc)` word to the driver exactly as before.
    buffer.bind(failed)?;
    linker.emit_exit(buffer, allocation, done, plan.fail.0, plan.fail.1)?;

    // --- the guard rejected an operand: run the real instruction ------------
    if let Some(deopt) = deopt {
        buffer.bind(deopt)?;
        emit_step_and_continue(buffer, done, allocation, func, pc)?;
    }
    buffer.bind(after)?;
    Ok(true)
}

/// Lower a **comparison** `test` the inline word/header subset refuses — one operand a
/// `Y` slot, a float register or a literal-area term, or both operands boxed — through
/// the [`jit_test`] helper, branching to the instruction's fail label **in-block**.
///
/// Returns `false` **without having emitted a byte** for any shape the helper does not
/// own (`NO_FAIL`, an unresolved fail label, a non-comparison kind, an operand count
/// other than two), so the caller falls back to exactly today's `jit_step_one`.
///
/// # Why the helper
///
/// The exactness rules of a comparison forbid the pointer compare the inline subset is
/// built from: an immediate is never equal to a boxed term, and two structurally equal
/// boxed terms may live at different addresses. The helper therefore evaluates the
/// instruction with `beam_term`'s own comparison (`jit_test`), which is the
/// interpreter's evaluation **by construction** — while the emitted code avoids the
/// interpreter's generic dispatch, the `Step` fold and the driver's block-entry round
/// trip. The helper's own read of the operands goes through the shared `read_operand`,
/// which can allocate, so the continue arm refills the cached x-registers.
///
/// # The emitted shape
///
/// ```text
///   call jit_test(rt, func, pc)          ; rax = TEST_TRUE | TEST_FALSE | anything else
///   cmp rax, TEST_TRUE
///   je  success
///   cmp rax, TEST_FALSE
///   je  failed
///   jmp done                             ; ERROR / a decline word: the driver decodes it
/// success:
///   fill                                 ; the helper ran `read_operand` on `machine.x`
///   <gated jit_refresh_y>                ; only when a later in-block instruction uses Y
///   jmp after
/// failed:
///   <linker.emit_exit to (fail_func, fail_pc)>
/// after:
/// ```
///
/// The `failed` exit is the linker's: a fail label the compilation emitted as a block is
/// jumped to **directly** (its own frame teardown replaces this block's), and any other
/// resolved target returns its `(func, pc)` word to the driver exactly as a deopt does.
/// A word that is neither boolean is left in `rax` for the driver, which decodes it
/// exactly as it decodes any other block return — the cached x-registers were already
/// spilled by `emit_helper_call`.
fn emit_test_helper(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<bool> {
    let Instr::Test { kind, fail, args } = instr else {
        return Ok(false);
    };
    // The helper owns exactly the six comparison kinds (see `jit_test`).
    if !matches!(
        kind,
        TestKind::IsEqExact
            | TestKind::IsNeExact
            | TestKind::IsEq
            | TestKind::IsNe
            | TestKind::IsLt
            | TestKind::IsGe
    ) || args.len() != 2
        || *fail == NO_FAIL
    {
        return Ok(false);
    }
    // The in-block fail branch needs the resolved target at compile time; an unresolved
    // label keeps the interpreter's exact diagnostic.
    let Ok((fail_func, fail_pc)) = labels.resolve(*fail) else {
        return Ok(false);
    };

    let after: CodeLabel = buffer.label();
    let success: CodeLabel = buffer.label();
    let failed: CodeLabel = buffer.label();

    let helper: StepHelper = jit_test;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc)],
    )?;

    buffer.mov_ri(Reg::R11, TEST_TRUE)?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::E, success)?;
    buffer.mov_ri(Reg::R11, TEST_FALSE)?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::E, failed)?;
    buffer.jmp(done)?;

    buffer.bind(success)?;
    // The helper ran the shared `read_operand`, which may allocate (a non-small
    // `Integer`/`Character` materializes an exact bignum), so the cached x-registers the
    // continuation can still observe are stale and must be refilled (the same *partial*
    // refill `emit_continue_check` emits — see `super::liveness`). The Y window is
    // re-published on the **continue** arm under the same compile-time gate
    // `emit_continue_check` uses — a block whose later instructions still access a `Y`
    // slot pays the refresh, a `Y`-free block nothing.
    fill(buffer, allocation, FillSite::Helper, pc)?;
    if allocation.refresh_y_after(pc) {
        emit_refresh_y(buffer)?;
    }
    buffer.jmp(after)?;

    buffer.bind(failed)?;
    linker.emit_exit(buffer, allocation, done, fail_func, fail_pc)?;

    buffer.bind(after)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_loader::{lower, Export, Function};
    use beam_term::{map, make_small, Atom, AtomTable, Eterm};

    use crate::context::{ExecutionContext, MinimalContext};
    use crate::jit::Tier;
    use crate::machine::Machine;

    /// Write `words` into `ctx`'s arena and return the **boxed term word** pointing at
    /// them.
    ///
    /// The word is `pointer | TAG_PRIMARY_BOXED`, i.e. `(raw & 3) == 2` — the exact
    /// predicate [`load_boxed_header`] emits (`beam_term::boxed_val` masks
    /// `TAG_PTR_MASK`, and `is_boxed_term` is `is_boxed && is_term_word`).
    fn boxed(ctx: &mut MinimalContext, words: &[Eterm]) -> Eterm {
        let hp = ctx.alloc_words(words.len()).expect("arena words");
        // SAFETY: `alloc_words` hands out `words.len()` writable words that stay valid
        // for as long as `ctx` lives (a pointer-stable reserved-heap arena grown in
        // place) and every word is written before the term escapes.
        unsafe {
            for (i, word) in words.iter().enumerate() {
                *hp.add(i) = *word;
            }
        }
        Eterm::from_raw(hp as usize | 2)
    }

    /// A raw header word (`make_header(sz, subtag)`), built by hand so the test pins the
    /// exact bit pattern the emitter's masks assume.
    fn header(sz: usize, subtag: usize) -> Eterm {
        Eterm::from_raw((sz << 6) | subtag)
    }

    /// The terms the `test` fixtures below are driven with. Every boxed entry is a real
    /// imaged word, so a predicate dereferences only words this test owns.
    struct Zoo {
        terms: Vec<(&'static str, Eterm)>,
    }

    impl Zoo {
        fn get(&self, name: &str) -> Eterm {
            self.terms
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, term)| *term)
                .unwrap_or_else(|| panic!("the zoo has no `{name}`"))
        }
    }

    /// `foo` is interned by [`probe_module`] in every module, so its atom word is the
    /// same in all of them.
    fn zoo(ctx: &mut MinimalContext, foo: Eterm, table: &AtomTable) -> Zoo {
        let small = make_small(7);
        let atom = foo;
        let tuple1 = boxed(ctx, &[header(1, 0x00), small]);
        let tuple2 = boxed(ctx, &[header(2, 0x00), small, NIL]);
        let tuple2_tagged = boxed(ctx, &[header(2, 0x00), atom, small]);
        let tuple1_tagged = boxed(ctx, &[header(1, 0x00), atom]);
        // A real flat map (the `map` module's own writer), so `is_map` sees the exact
        // layout `beam_term` produces.
        let pairs = [(make_small(1), make_small(2))];
        let map_term = {
            let hp = ctx
                .alloc_words(map::map_words_capacity(pairs.len()))
                .expect("map words");
            // SAFETY: `map_words_capacity(1)` words, `write_map` fills them and returns
            // the boxed term; the words stay valid for `ctx`'s lifetime.
            unsafe { map::write_map(hp, &pairs, table) }.expect("flat map")
        };
        // A boxed float: `[HEADER_FLONUM, f64 bits]`.
        let float = boxed(
            ctx,
            &[Eterm::from_raw(0x58), Eterm::from_raw(1.5f64.to_bits() as usize)],
        );
        // Heap bits: `[thing_word, size, data...]` — `bitstring_size` reads `p[1]`.
        let bitstr8 = boxed(ctx, &[header(2, 0x20), Eterm::from_raw(8)]);
        let bitstr3 = boxed(ctx, &[header(2, 0x20), Eterm::from_raw(3)]);
        // Sub bits: `[thing_word, base_flags, start, end, orig]` — size is `end - start`.
        let subbits16 = boxed(
            ctx,
            &[
                Eterm::from_raw(0x124),
                Eterm::from_raw(0),
                Eterm::from_raw(0),
                Eterm::from_raw(16),
                Eterm::from_raw(0),
            ],
        );
        let subbits3 = boxed(
            ctx,
            &[
                Eterm::from_raw(0x124),
                Eterm::from_raw(0),
                Eterm::from_raw(0),
                Eterm::from_raw(3),
                Eterm::from_raw(0),
            ],
        );
        // A local fun header with arity 2 (`FUN_SUBTAG | arity << 8`); only `p[0]` is
        // ever read by `is_function`/`is_function2`.
        let fun2 = boxed(ctx, &[Eterm::from_raw(0x14 | (2usize << 8)), make_small(0)]);
        // The internal pid/port immediates: `(data << 4) | TAG_IMMED1_{PID,PORT}`.
        let int_pid = Eterm::from_raw((5usize << 4) | 0x3);
        let int_port = Eterm::from_raw((5usize << 4) | 0x7);
        // External pid/port/ref and internal ref: header subtag only (`0x30`/`0x34`/
        // `0x38`/`0x10`); no predicate reads past `p[0]`.
        let ext_pid = boxed(ctx, &[header(1, 0x30), small]);
        let ext_port = boxed(ctx, &[header(1, 0x34), small]);
        let ext_ref = boxed(ctx, &[header(1, 0x38), small]);
        let int_ref = boxed(ctx, &[header(1, 0x10), small]);

        Zoo {
            terms: vec![
                ("small", small),
                ("atom", atom),
                ("nil", NIL),
                ("tuple1", tuple1),
                ("tuple2", tuple2),
                ("tuple2_tagged", tuple2_tagged),
                ("tuple1_tagged", tuple1_tagged),
                ("map", map_term),
                ("float", float),
                ("bitstr8", bitstr8),
                ("bitstr3", bitstr3),
                ("subbits16", subbits16),
                ("subbits3", subbits3),
                ("fun2", fun2),
                ("int_pid", int_pid),
                ("int_port", int_port),
                ("ext_pid", ext_pid),
                ("ext_port", ext_port),
                ("ext_ref", ext_ref),
                ("int_ref", int_ref),
            ],
        }
    }

    /// Build a one-export module whose `probe/1` is
    /// `test -> x(0) = 1 ; fail -> x(0) = 0`, with `x(0)` the value under test.
    ///
    /// `foo` is interned right after the module name in every module built here, so
    /// [`zoo`]'s atom word is valid in all of them.
    fn probe_module(name: &str, kind: TestKind, extra: Vec<Operand>) -> (Module, Atom, Atom) {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        let foo = module.atom_table.intern("foo");
        module.atom_table.intern("true");
        module.atom_table.intern("false");
        let export = module.atom_table.intern("probe");
        let entry = Label(1);
        let fail = Label(2);
        let mut args = vec![Operand::X(0)];
        args.extend(extra);
        let code = vec![
            Instr::Label(entry),
            Instr::Test { kind, fail, args },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(fail),
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let (blocks, labels) = lower::build_blocks(&code);
        module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module: module.name,
            name: export,
            arity: 1,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function: export,
            arity: 1,
            label: entry,
        });
        (module, export, foo)
    }

    /// Run `probe/1` under both tiers with `value` in `x(0)` and return
    /// `(interpreter, baseline)`.
    fn both_tiers(module: &Module, export: Atom, value: Eterm) -> (Eterm, Eterm) {
        let args = [value];
        let mut interpreter_ctx = MinimalContext::new();
        let interpreter = Machine::new()
            .run_with_tier(
                module,
                export,
                1,
                &args,
                &mut interpreter_ctx,
                Tier::Interpreter,
            )
            .expect("the interpreter tier runs the fixture");
        let mut baseline_ctx = MinimalContext::new();
        let baseline = Machine::new()
            .run_with_tier(
                module,
                export,
                1,
                &args,
                &mut baseline_ctx,
                Tier::BaselineJit,
            )
            .expect("the baseline tier runs the fixture");
        (interpreter, baseline)
    }

    /// One table row: a `test` kind, its extra operands, the value it is driven with,
    /// and the answer the compiler must produce.
    struct Case {
        kind: TestKind,
        extra: Vec<Operand>,
        value: &'static str,
        expect: bool,
    }

    /// The boxed-term header-test matrix: every newly inlined kind, on the values that
    /// make it true **and** on the values that make it false (including the non-boxed
    /// immediates, whose fail path must not dereference anything).
    fn header_cases() -> Vec<Case> {
        let case = |kind, extra: Vec<Operand>, value: &'static str, expect| Case {
            kind,
            extra,
            value,
            expect,
        };
        let arity = |n: i64| vec![Operand::Integer(n)];
        vec![
            case(TestKind::IsInteger, vec![], "small", true),
            case(TestKind::IsInteger, vec![], "atom", false),
            case(TestKind::IsInteger, vec![], "nil", false),
            case(TestKind::IsInteger, vec![], "tuple2", false),
            case(TestKind::IsNumber, vec![], "small", true),
            case(TestKind::IsNumber, vec![], "float", true),
            case(TestKind::IsNumber, vec![], "atom", false),
            case(TestKind::IsNumber, vec![], "tuple2", false),
            case(TestKind::IsFloat, vec![], "float", true),
            case(TestKind::IsFloat, vec![], "small", false),
            case(TestKind::IsFloat, vec![], "tuple2", false),
            case(TestKind::IsTuple, vec![], "tuple2", true),
            case(TestKind::IsTuple, vec![], "tuple1", true),
            case(TestKind::IsTuple, vec![], "map", false),
            case(TestKind::IsTuple, vec![], "float", false),
            case(TestKind::IsTuple, vec![], "atom", false),
            case(TestKind::IsTuple, vec![], "nil", false),
            case(TestKind::IsMap, vec![], "map", true),
            case(TestKind::IsMap, vec![], "tuple2", false),
            case(TestKind::IsMap, vec![], "atom", false),
            case(TestKind::IsBitstr, vec![], "bitstr8", true),
            case(TestKind::IsBitstr, vec![], "bitstr3", true),
            case(TestKind::IsBitstr, vec![], "subbits3", true),
            case(TestKind::IsBitstr, vec![], "atom", false),
            case(TestKind::IsBitstr, vec![], "tuple2", false),
            case(TestKind::IsBinary, vec![], "bitstr8", true),
            case(TestKind::IsBinary, vec![], "subbits16", true),
            case(TestKind::IsBinary, vec![], "bitstr3", false),
            case(TestKind::IsBinary, vec![], "subbits3", false),
            case(TestKind::IsBinary, vec![], "atom", false),
            case(TestKind::IsBinary, vec![], "small", false),
            case(TestKind::IsFunction, vec![], "fun2", true),
            case(TestKind::IsFunction, vec![], "atom", false),
            case(TestKind::IsFunction, vec![], "tuple2", false),
            case(TestKind::IsFunction2, arity(2), "fun2", true),
            case(TestKind::IsFunction2, arity(3), "fun2", false),
            case(TestKind::IsFunction2, arity(2), "atom", false),
            case(TestKind::IsPid, vec![], "int_pid", true),
            case(TestKind::IsPid, vec![], "ext_pid", true),
            case(TestKind::IsPid, vec![], "int_port", false),
            case(TestKind::IsPid, vec![], "small", false),
            case(TestKind::IsPid, vec![], "ext_port", false),
            case(TestKind::IsPort, vec![], "int_port", true),
            case(TestKind::IsPort, vec![], "ext_port", true),
            case(TestKind::IsPort, vec![], "int_pid", false),
            case(TestKind::IsPort, vec![], "atom", false),
            case(TestKind::IsReference, vec![], "int_ref", true),
            case(TestKind::IsReference, vec![], "ext_ref", true),
            case(TestKind::IsReference, vec![], "ext_pid", false),
            case(TestKind::IsReference, vec![], "nil", false),
            case(TestKind::TestArity, arity(2), "tuple2", true),
            case(TestKind::TestArity, arity(1), "tuple1", true),
            case(TestKind::TestArity, arity(3), "tuple2", false),
            case(TestKind::TestArity, arity(1), "tuple2", false),
            case(TestKind::TestArity, arity(2), "atom", false),
            case(TestKind::TestArity, arity(1), "small", false),
            case(TestKind::IsTaggedTuple, arity(1), "tuple1_tagged", true),
            case(TestKind::IsTaggedTuple, arity(2), "tuple2_tagged", true),
            case(TestKind::IsTaggedTuple, arity(2), "tuple2", false),
            case(TestKind::IsTaggedTuple, arity(2), "tuple1_tagged", false),
            case(TestKind::IsTaggedTuple, arity(1), "atom", false),
        ]
    }

    /// Every newly inlined header kind agrees with the interpreter tier, on the true
    /// **and** the false values — the widening's correctness gate.
    #[test]
    fn header_tests_agree_with_the_interpreter_tier() {
        let mut ctx = MinimalContext::new();
        let table_owner = Module::default();
        // The atom word is the same in every `probe_module`, so any one of them can
        // provide it.
        let (_, _, foo) = probe_module("probe_atom", TestKind::IsNil, vec![]);
        let zoo = zoo(&mut ctx, make_atom(foo), &table_owner.atom_table);

        for case in header_cases() {
            let mut extra = case.extra.clone();
            if case.kind == TestKind::IsTaggedTuple {
                // The tag is the module's `foo` atom, matching the zoo's tagged tuples.
                extra.push(Operand::Atom(foo));
            }
            let (module, export, _) = probe_module("probe_header", case.kind, extra);
            let value = zoo.get(case.value);
            let (interpreter, baseline) = both_tiers(&module, export, value);
            let expected = make_small(i64::from(case.expect));
            assert_eq!(
                interpreter, expected,
                "{:?} on `{}` must be {} in the interpreter tier",
                case.kind, case.value, case.expect
            );
            assert_eq!(
                baseline, expected,
                "{:?} on `{}` must be {} in the baseline tier",
                case.kind, case.value, case.expect
            );
        }
    }


    /// `is_binary`'s heap-bits branch reads the size word at `p[1]` **only** after the
    /// bitstring-header test, and a header that is not a bitstring header (`2`, say)
    /// is rejected without reading it: the emitter never touches `p[1]` of a
    /// non-bitstring. This pins that a fabricated short thing cannot be misread.
    #[test]
    fn is_binary_rejects_a_non_bitstring_header_without_reading_the_size() {
        let mut ctx = MinimalContext::new();
        // `[arityval(1), small]` — a tuple-like header, so the bitstring test fails
        // *and* the term is shorter than a heap-bits thing.
        let not_bits = boxed(&mut ctx, &[header(1, 0x00), make_small(3)]);
        let (module, export, _) = probe_module("probe_binary_guard", TestKind::IsBinary, vec![]);
        let (interpreter, baseline) = both_tiers(&module, export, not_bits);
        assert_eq!(interpreter, make_small(0));
        assert_eq!(baseline, make_small(0));
    }

    /// The `Zoo` fixtures are sound against `beam_term`'s **own** predicates — the
    /// ground truth the emitter's masks mirror. A failure here means the fixture (not
    /// the emitter) is wrong.
    #[test]
    fn zoo_fixtures_match_the_beam_term_predicates() {
        let mut ctx = MinimalContext::new();
        let (_, _, foo) = probe_module("probe_atom_check", TestKind::IsNil, vec![]);
        let table_owner = Module::default();
        let zoo = zoo(&mut ctx, make_atom(foo), &table_owner.atom_table);

        assert!(crate::predicates::is_integer(zoo.get("small")));
        assert!(!crate::predicates::is_integer(zoo.get("atom")));
        assert!(crate::predicates::is_float(zoo.get("float")));
        assert!(crate::predicates::is_number(zoo.get("float")));
        assert!(crate::predicates::is_tuple(zoo.get("tuple2")));
        assert!(!crate::predicates::is_tuple(zoo.get("map")));
        assert!(crate::predicates::is_map(zoo.get("map")));
        assert!(crate::predicates::is_bitstring(zoo.get("bitstr8")));
        assert!(crate::predicates::is_bitstring(zoo.get("subbits16")));
        assert!(crate::predicates::is_binary(zoo.get("bitstr8")));
        assert!(!crate::predicates::is_binary(zoo.get("bitstr3")));
        assert!(crate::predicates::is_binary(zoo.get("subbits16")));
        assert!(!crate::predicates::is_binary(zoo.get("subbits3")));
        assert!(crate::terms::is_fun(zoo.get("fun2")));
        assert_eq!(crate::terms::fun_arity(zoo.get("fun2")).unwrap(), 2);
        assert!(crate::predicates::is_external_pid(zoo.get("ext_pid")));
        assert!(crate::predicates::is_external_port(zoo.get("ext_port")));
        assert!(crate::predicates::is_internal_ref(zoo.get("int_ref")));
        assert!(crate::predicates::is_external_ref(zoo.get("ext_ref")));
        assert!(crate::predicates::is_tuple_arity(zoo.get("tuple2"), 2));
    }
}
