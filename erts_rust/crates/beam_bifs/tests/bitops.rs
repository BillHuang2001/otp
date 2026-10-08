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

//! Integration tests for the `erlang` integer bit-operator BIFs — `band/2`,
//! `bor/2`, `bxor/2`, `bnot/1`, `bsl/2`, `bsr/2` — driven through the public
//! [`BifRegistry`] seam (public API only).
//!
//! The six operators are Erlang *keywords* — there is no Rust spelling for them —
//! so every case is invoked as `erlang:function/arity` through
//! [`BifRegistry::call`], exactly the way the loader/engine resolves the BIF.
//!
//! Every expectation is pinned against the OTP 29 / erts-17.1 oracle. The whole
//! battery was read back with a single `erl` batch, e.g.:
//!
//! ```text
//! $ erl -noshell -eval 'io:format("~p~n", [255 band 15]), halt(0).'
//! 15
//! ```
//!
//! (each test cites the corresponding `erl` expression, quoted as the operator
//! form, since the BIFs themselves are called through the registry here).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{make_small, signed_value, AtomTable, Eterm};

/// A registry + heap + atom table the tests drive BIF calls through.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok"]);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(8192).expect("test heap"),
            atoms,
        }
    }

    /// Calls `erlang:function/args.len()` through the registry.
    fn call(&mut self, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call("erlang", function, args.len() as u32, &mut ctx, args)
    }

    /// Builds a bignum term on the fixture's heap from its little-endian limbs.
    fn bignum(&mut self, negative: bool, digits: &[u64]) -> Eterm {
        beam_codec::term::make_big(&mut self.heap, negative, digits).expect("test bignum")
    }

    /// Builds a float term on the fixture's heap.
    fn float(&mut self, value: f64) -> Eterm {
        beam_codec::term::make_float(&mut self.heap, value).expect("test float")
    }
}

/// The `(negative, digits)` parts of a bignum result (`digits` least significant
/// first, copied out of the term's storage).
fn parts(term: Eterm) -> (bool, Vec<u64>) {
    let (negative, digits) = beam_codec::term::big_parts(term).expect("expected a bignum");
    (negative, digits.to_vec())
}

/// The value of a small-integer result.
fn small(term: Eterm) -> i64 {
    assert!(beam_term::is_small(term), "expected a small int, got {term:?}");
    signed_value(term)
}

/// Whether `term` is a bignum (the safe reading of `beam_term::is_big`, whose
/// predicate is `unsafe`).
fn is_bignum(term: Eterm) -> bool {
    beam_codec::term::big_parts(term).is_ok()
}

/// The exact `i128` value of a small integer or a bignum result.
///
/// Every value these tests exercise fits an `i128` (the widest is `±2^71`), so the
/// conversion is a two-limb magnitude reassembly — enough to compare a folded-back
/// small integer and a bignum by value, whichever the BIF produced.
fn value(term: Eterm) -> i128 {
    if beam_term::is_small(term) {
        return i128::from(signed_value(term));
    }
    let (negative, digits) = parts(term);
    let mut magnitude = 0i128;
    for (limb, digit) in digits.iter().enumerate() {
        assert!(limb < 2, "test value wider than two limbs");
        magnitude |= i128::from(*digit) << (64 * limb as u32);
    }
    if negative {
        -magnitude
    } else {
        magnitude
    }
}

#[test]
fn bitwise_operators_on_small_integers_match_the_oracle() {
    let mut fx = Fixture::new();

    // Oracle: 255 band 15 ⇒ 15, (-2) band 3 ⇒ 2, (-2) bor 3 ⇒ -1,
    // (-2) bxor 3 ⇒ -3.
    let and = fx.call("band", &[make_small(255), make_small(15)]).expect("band/2");
    assert_eq!(small(and), 15);
    let and = fx.call("band", &[make_small(-2), make_small(3)]).expect("band/2");
    assert_eq!(small(and), 2);
    let or = fx.call("bor", &[make_small(-2), make_small(3)]).expect("bor/2");
    assert_eq!(small(or), -1);
    let xor = fx.call("bxor", &[make_small(-2), make_small(3)]).expect("bxor/2");
    assert_eq!(small(xor), -3);

    // Oracle: 0 band 0 ⇒ 0, (-1) band 0 ⇒ 0, (-1) bor 0 ⇒ -1.
    let and = fx.call("band", &[make_small(0), make_small(0)]).expect("band/2");
    assert_eq!(small(and), 0);
    let and = fx.call("band", &[make_small(-1), make_small(0)]).expect("band/2");
    assert_eq!(small(and), 0);
    let or = fx.call("bor", &[make_small(-1), make_small(0)]).expect("bor/2");
    assert_eq!(small(or), -1);
}

#[test]
fn bnot_matches_the_oracle() {
    let mut fx = Fixture::new();

    // Oracle: bnot 5 ⇒ -6, bnot -1 ⇒ 0, bnot 0 ⇒ -1, bnot 16#F0F0 ⇒ -61681.
    let complement = fx.call("bnot", &[make_small(5)]).expect("bnot/1");
    assert_eq!(small(complement), -6);
    let complement = fx.call("bnot", &[make_small(-1)]).expect("bnot/1");
    assert_eq!(small(complement), 0);
    let complement = fx.call("bnot", &[make_small(0)]).expect("bnot/1");
    assert_eq!(small(complement), -1);
    let complement = fx.call("bnot", &[make_small(0xF0F0)]).expect("bnot/1");
    assert_eq!(small(complement), -61681);
}

#[test]
fn shifts_on_small_integers_match_the_oracle() {
    let mut fx = Fixture::new();

    // Oracle: 1 bsl 10 ⇒ 1024, (-1) bsl 4 ⇒ -16, 8 bsl -2 ⇒ 2 (a negative count
    // flips the direction).
    let shifted = fx.call("bsl", &[make_small(1), make_small(10)]).expect("bsl/2");
    assert_eq!(small(shifted), 1024);
    let shifted = fx.call("bsl", &[make_small(-1), make_small(4)]).expect("bsl/2");
    assert_eq!(small(shifted), -16);
    let shifted = fx.call("bsl", &[make_small(8), make_small(-2)]).expect("bsl/2");
    assert_eq!(small(shifted), 2);

    // Oracle: 255 bsr 4 ⇒ 15, (-1) bsr 4 ⇒ -1, (-3) bsr 1 ⇒ -2 (an arithmetic,
    // floor shift), 1 bsr -2 ⇒ 4, 8 bsr -1 ⇒ 16.
    let shifted = fx.call("bsr", &[make_small(255), make_small(4)]).expect("bsr/2");
    assert_eq!(small(shifted), 15);
    let shifted = fx.call("bsr", &[make_small(-1), make_small(4)]).expect("bsr/2");
    assert_eq!(small(shifted), -1);
    let shifted = fx.call("bsr", &[make_small(-3), make_small(1)]).expect("bsr/2");
    assert_eq!(small(shifted), -2);
    let shifted = fx.call("bsr", &[make_small(1), make_small(-2)]).expect("bsr/2");
    assert_eq!(small(shifted), 4);
    let shifted = fx.call("bsr", &[make_small(8), make_small(-1)]).expect("bsr/2");
    assert_eq!(small(shifted), 16);

    // Oracle: a shift past the word width keeps only the sign —
    // 255 bsr 100 ⇒ 0, (-255) bsr 100 ⇒ -1, (-1) bsr 100 ⇒ -1, (-255) bsr 4 ⇒ -16.
    let shifted = fx.call("bsr", &[make_small(255), make_small(100)]).expect("bsr/2");
    assert_eq!(small(shifted), 0);
    let shifted = fx.call("bsr", &[make_small(-255), make_small(100)]).expect("bsr/2");
    assert_eq!(small(shifted), -1);
    let shifted = fx.call("bsr", &[make_small(-1), make_small(100)]).expect("bsr/2");
    assert_eq!(small(shifted), -1);
    let shifted = fx.call("bsr", &[make_small(-255), make_small(4)]).expect("bsr/2");
    assert_eq!(small(shifted), -16);
}

#[test]
fn bitwise_operators_on_bignum_operands_match_the_oracle() {
    let mut fx = Fixture::new();
    // 2^70 and -(2^70), built from their little-endian limbs (independent of the
    // shift BIF under test).
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let plus_one = fx.bignum(false, &[1, 64]); // 2^70 + 1
    let minus_two_to_70 = fx.bignum(true, &[0, 64]);
    let below = fx.bignum(false, &[u64::MAX, 63]); // 2^70 - 1
    let one = make_small(1);

    // Oracle: (1 bsl 70) band ((1 bsl 70)+1) ⇒ 1180591620717411303424 (= 2^70).
    let and = fx.call("band", &[two_to_70, plus_one]).expect("band/2");
    assert_eq!(value(and), 1180591620717411303424);
    assert!(is_bignum(and));
    // Oracle: (1 bsl 70) bor 1 ⇒ 1180591620717411303425.
    let or = fx.call("bor", &[two_to_70, one]).expect("bor/2");
    assert_eq!(value(or), 1180591620717411303425);
    assert_eq!(parts(or), (false, vec![1, 64]));
    // Oracle: -(1 bsl 70) bor 1 ⇒ -1180591620717411303423 (= -(2^70 - 1)).
    let or = fx.call("bor", &[minus_two_to_70, one]).expect("bor/2");
    assert_eq!(value(or), -1180591620717411303423);
    assert_eq!(parts(or), (true, vec![u64::MAX, 63]));
    // Oracle: -(1 bsl 70) bxor (1 bsl 70) ⇒ -2361183241434822606848 (= -2^71).
    let xor = fx.call("bxor", &[minus_two_to_70, two_to_70]).expect("bxor/2");
    assert_eq!(value(xor), -2361183241434822606848);
    assert_eq!(parts(xor), (true, vec![0, 128]));
    // Oracle: -(1 bsl 70) band (1 bsl 70) ⇒ 1180591620717411303424.
    let and = fx.call("band", &[minus_two_to_70, two_to_70]).expect("band/2");
    assert_eq!(value(and), 1180591620717411303424);
    // Oracle: bnot (1 bsl 70) ⇒ -1180591620717411303425 (= -(2^70 + 1)).
    let complement = fx.call("bnot", &[two_to_70]).expect("bnot/1");
    assert_eq!(value(complement), -1180591620717411303425);
    assert_eq!(parts(complement), (true, vec![1, 64]));

    // Oracle: ((1 bsl 70)-1) band 16#F0F0 ⇒ 61680 (folds back to a small int).
    let and = fx.call("band", &[below, make_small(0xF0F0)]).expect("band/2");
    assert!(beam_term::is_small(and));
    assert_eq!(small(and), 61680);
    // Oracle: (1 bsl 70) band ((1 bsl 70)-1) ⇒ 0; (1 bsl 70) bxor (1 bsl 70) ⇒ 0;
    // (1 bsl 70) bor (1 bsl 70) ⇒ 1180591620717411303424.
    let and = fx.call("band", &[two_to_70, below]).expect("band/2");
    assert_eq!(small(and), 0);
    let xor = fx.call("bxor", &[two_to_70, two_to_70]).expect("bxor/2");
    assert_eq!(small(xor), 0);
    let or = fx.call("bor", &[two_to_70, two_to_70]).expect("bor/2");
    assert_eq!(value(or), 1180591620717411303424);
}

#[test]
fn shifts_on_bignum_operands_match_the_oracle() {
    let mut fx = Fixture::new();
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let minus_two_to_70_plus_one = fx.bignum(true, &[1, 64]); // -(2^70 + 1)
    let one = make_small(1);

    // Oracle: 1 bsl 70 ⇒ 1180591620717411303424 (a bignum).
    let shifted = fx.call("bsl", &[one, make_small(70)]).expect("bsl/2");
    assert!(is_bignum(shifted));
    assert_eq!(value(shifted), 1180591620717411303424);
    assert_eq!(parts(shifted), (false, vec![0, 64]));

    // Oracle: (1 bsl 70) bsl 1 ⇒ 2361183241434822606848 (= 2^71).
    let shifted = fx.call("bsl", &[two_to_70, make_small(1)]).expect("bsl/2");
    assert_eq!(value(shifted), 2361183241434822606848);

    // Oracle: (1 bsl 70) bsr 65 ⇒ 32 (folds back to a small int), bsr 70 ⇒ 1,
    // bsr 71 ⇒ 0.
    let shifted = fx.call("bsr", &[two_to_70, make_small(65)]).expect("bsr/2");
    assert!(beam_term::is_small(shifted));
    assert_eq!(small(shifted), 32);
    let shifted = fx.call("bsr", &[two_to_70, make_small(70)]).expect("bsr/2");
    assert_eq!(small(shifted), 1);
    let shifted = fx.call("bsr", &[two_to_70, make_small(71)]).expect("bsr/2");
    assert_eq!(small(shifted), 0);

    // Oracle: (-((1 bsl 70)+1)) bsr 1 ⇒ -590295810358705651713 (= -(2^69 + 1)).
    let shifted = fx.call("bsr", &[minus_two_to_70_plus_one, make_small(1)]).expect("bsr/2");
    assert_eq!(value(shifted), -590295810358705651713);

    // Oracle: 16#F0F0 bsl 70 ⇒ 72818891165849929195192320.
    let shifted = fx.call("bsl", &[make_small(0xF0F0), make_small(70)]).expect("bsl/2");
    assert_eq!(value(shifted), 72818891165849929195192320);
}

#[test]
fn bitwise_operators_mix_small_and_bignum_operands() {
    let mut fx = Fixture::new();
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let one = make_small(1);

    // Oracle: (1 bsl 70) band 3 ⇒ 0 and 3 band (1 bsl 70) ⇒ 0 (commutative).
    let and = fx.call("band", &[two_to_70, make_small(3)]).expect("band/2");
    assert_eq!(small(and), 0);
    let and = fx.call("band", &[make_small(3), two_to_70]).expect("band/2");
    assert_eq!(small(and), 0);

    // Oracle: (1 bsl 70) bor 15 ⇒ 1180591620717411303439, and the mirrored order
    // agrees.
    let or = fx.call("bor", &[two_to_70, make_small(15)]).expect("bor/2");
    assert_eq!(value(or), 1180591620717411303439);
    let or = fx.call("bor", &[make_small(15), two_to_70]).expect("bor/2");
    assert_eq!(value(or), 1180591620717411303439);

    // Oracle: (1 bsl 70) bxor 1 ⇒ 1180591620717411303425.
    let xor = fx.call("bxor", &[two_to_70, one]).expect("bxor/2");
    assert_eq!(value(xor), 1180591620717411303425);
    let xor = fx.call("bxor", &[one, two_to_70]).expect("bxor/2");
    assert_eq!(value(xor), 1180591620717411303425);
}

#[test]
fn non_integer_operands_are_badarith() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let one_point_oh = fx.float(1.0);
    // A non-number term: the `true` atom (built through a registered BIF).
    let truth = fx.call("=:=", &[one, one]).expect("erlang:'=:='/2");

    // Oracle: 1.0 band 1 ⇒ error:badarith.
    assert!(matches!(
        fx.call("band", &[one_point_oh, one]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(
        fx.call("bor", &[one_point_oh, one]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(
        fx.call("bxor", &[one, one_point_oh]),
        Err(BeamError::BadArith)
    ));
    // Oracle: bnot 1.0 ⇒ error:badarith.
    assert!(matches!(fx.call("bnot", &[one_point_oh]), Err(BeamError::BadArith)));
    // Oracle: 1 bsl 1.0 ⇒ error:badarith (and the left operand likewise).
    assert!(matches!(
        fx.call("bsl", &[one, one_point_oh]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(
        fx.call("bsl", &[one_point_oh, one]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(
        fx.call("bsr", &[one_point_oh, one]),
        Err(BeamError::BadArith)
    ));
    // A non-number operand is `badarith` too (`true band 1` in the oracle).
    assert!(matches!(fx.call("band", &[truth, one]), Err(BeamError::BadArith)));
    assert!(matches!(fx.call("bnot", &[truth]), Err(BeamError::BadArith)));
}

#[test]
fn oversized_shifts_match_the_oracle() {
    let mut fx = Fixture::new();
    let one = make_small(1);
    let minus_one = make_small(-1);
    let zero = make_small(0);
    // 2^70 as a shift *count* — a bignum far larger than any `usize` magnitude.
    let huge = fx.bignum(false, &[0, 64]);

    // Oracle: 1 bsl (1 bsl 70) ⇒ error:system_limit (a left shift that cannot even
    // fit its shift amount).
    assert!(matches!(fx.call("bsl", &[one, huge]), Err(BeamError::SystemLimit)));
    // Oracle: 1 bsl 4194240 ⇒ error:system_limit — rejected *before* the limb
    // vector is allocated (`BIG_ARITY_MAX` limbs); 1 bsl 4194239 still succeeds.
    assert!(matches!(
        fx.call("bsl", &[one, make_small(4194240)]),
        Err(BeamError::SystemLimit)
    ));

    // Oracle: a right shift by an unrepresentably large amount keeps only the
    // sign — 1 bsr (1 bsl 70) ⇒ 0, (-1) bsr (1 bsl 70) ⇒ -1.
    let shifted = fx.call("bsr", &[one, huge]).expect("bsr/2");
    assert_eq!(small(shifted), 0);
    let shifted = fx.call("bsr", &[minus_one, huge]).expect("bsr/2");
    assert_eq!(small(shifted), -1);

    // Oracle: 0 bsl (1 bsl 70) ⇒ 0 — a zero left operand short-circuits before the
    // shift amount is even considered.
    let shifted = fx.call("bsl", &[zero, huge]).expect("bsl/2");
    assert_eq!(small(shifted), 0);
}
