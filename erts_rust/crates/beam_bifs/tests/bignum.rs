//! Integration tests for the exact, bignum-promoting `erlang` arithmetic BIFs,
//! driven through the public [`BifRegistry`] seam (public API only).
//!
//! Every expectation is pinned against the OTP 29 oracle (`erl -noshell -eval`),
//! including the `term_to_binary` image the differential corpus case
//! `semantics/bignum_multiplication` compares.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

/// The golden `term_to_binary(123456789012345678 * 98)` image — the differential
/// corpus case `semantics/bignum_multiplication`: a `SMALL_BIG_EXT` (110) with
/// eight base-256 magnitude digits and a positive sign.
const BIGNUM_IMAGE: [u8; 12] = [131, 110, 8, 0, 220, 35, 189, 158, 245, 114, 231, 167];

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

    /// The boolean value of a `true`/`false` atom term.
    fn boolean(&self, term: Eterm) -> bool {
        match self.atoms.name(atom_val(term)) {
            Some("true") => true,
            Some("false") => false,
            other => panic!("expected a boolean atom, got {other:?}"),
        }
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

/// The value of a float result.
fn float(term: Eterm) -> f64 {
    beam_codec::term::float_value(term).expect("expected a float")
}

#[test]
fn the_differential_bignum_case_agrees_byte_for_byte() {
    let mut fx = Fixture::new();
    let left = beam_term::make_small(123456789012345678);
    let right = beam_term::make_small(98);

    let product = fx.call("*", &[left, right]).expect("erlang:'*'/2");
    assert!(
        is_bignum(product),
        "the exact product must promote to a bignum"
    );
    // Oracle: 12098765323209876444.
    assert_eq!(parts(product), (false, vec![12098765323209876444]));

    // The differential compares the `term_to_binary` image of the result.
    let image = fx.call("term_to_binary", &[product]).expect("term_to_binary/1");
    let (bytes, offset, size) = beam_codec::term::bitstring_bytes(image).expect("a binary");
    assert_eq!(offset, 0, "term_to_binary returns a whole-byte binary");
    assert_eq!(size, BIGNUM_IMAGE.len() * 8);
    assert_eq!(bytes, &BIGNUM_IMAGE);
}

#[test]
fn bignum_operands_and_results_are_exact() {
    let mut fx = Fixture::new();
    // `1 bsl 70`, `1 bsl 35` and `-(1 bsl 70)` (oracle-exact values as bignums).
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let two_to_35 = fx.bignum(false, &[1 << 35]);
    let minus_two_to_70 = fx.bignum(true, &[0, 64]);
    let small_operand = beam_term::make_small(123456789012345678);

    // `(1 bsl 70) + 123456789012345678` ⇒ 1180715077506423649102.
    let sum = fx.call("+", &[two_to_70, small_operand]).expect("erlang:'+'/2");
    assert_eq!(parts(sum), (false, vec![123456789012345678, 64]));

    // `(1 bsl 70) - (1 bsl 35)` ⇒ [18446744039349813248, 63].
    let difference = fx.call("-", &[two_to_70, two_to_35]).expect("erlang:'-'/2");
    assert_eq!(parts(difference), (false, vec![18446744039349813248, 63]));

    // `abs(-(1 bsl 70))` and `-(1 bsl 70)` keep the exact magnitude.
    let absolute = fx.call("abs", &[minus_two_to_70]).expect("erlang:abs/1");
    assert_eq!(parts(absolute), (false, vec![0, 64]));
    let negated = fx.call("-", &[two_to_70]).expect("erlang:'-'/1");
    assert_eq!(parts(negated), (true, vec![0, 64]));

    // `(1 bsl 70) div (1 bsl 35)` = 2^35 and `rem` = 0, both collapsing back to a
    // small integer.
    let quotient = fx.call("div", &[two_to_70, two_to_35]).expect("erlang:div/2");
    assert_eq!(small(quotient), 1 << 35);
    let remainder = fx.call("rem", &[two_to_70, two_to_35]).expect("erlang:rem/2");
    assert_eq!(small(remainder), 0);

    // `rem` carries the dividend's sign.
    let seven = beam_term::make_small(7);
    let remainder = fx.call("rem", &[two_to_70, seven]).expect("erlang:rem/2");
    assert_eq!(small(remainder), 2);
    let remainder = fx.call("rem", &[minus_two_to_70, seven]).expect("erlang:rem/2");
    assert_eq!(small(remainder), -2);
}

#[test]
fn multi_limb_division_matches_the_oracle() {
    let mut fx = Fixture::new();
    // X = 1234567890123456789012345678909012345678901234567890 (three limbs),
    // Y = 987654321098765432109876543210 (two limbs).
    let dividend = fx.bignum(false, &[15020605774147554002, 14481235555578642862, 3628068951366]);
    let divisor = fx.bignum(false, &[18425954751889178346, 53540848029]);

    let quotient = fx.call("div", &[dividend, divisor]).expect("erlang:div/2");
    assert_eq!(parts(quotient), (false, vec![14068135670835041870, 67]));
    let remainder = fx.call("rem", &[dividend, divisor]).expect("erlang:rem/2");
    assert_eq!(parts(remainder), (false, vec![13889941006043235206, 20496106279]));

    // A single-limb divisor (the short division path).
    let seven = beam_term::make_small(7);
    let quotient = fx.call("div", &[dividend, seven]).expect("erlang:div/2");
    assert_eq!(
        parts(quotient),
        (false, vec![7416299131652379604, 17880242856833707508, 518295564480])
    );
    let remainder = fx.call("rem", &[dividend, seven]).expect("erlang:rem/2");
    assert_eq!(small(remainder), 6);
}

#[test]
fn exact_results_fold_back_into_small_integers() {
    let mut fx = Fixture::new();
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let one = fx.call("div", &[two_to_70, two_to_70]).expect("erlang:div/2");
    assert!(beam_term::is_small(one));
    assert_eq!(small(one), 1);
    let zero = fx.call("rem", &[two_to_70, two_to_70]).expect("erlang:rem/2");
    assert!(beam_term::is_small(zero));
    assert_eq!(small(zero), 0);    // `MAX_SMALL * MAX_SMALL` is a bignum; subtracting it from itself is a small 0.
    let max_small = beam_term::make_small(beam_term::MAX_SMALL);
    let product = fx.call("*", &[max_small, max_small]).expect("erlang:'*'/2");
    assert_eq!(
        parts(product),
        (false, vec![17293822569102704641, 18014398509481983])
    );
    let back = fx.call("-", &[product, product]).expect("erlang:'-'/2");
    assert!(beam_term::is_small(back));
    assert_eq!(small(back), 0);
}

/// Comparison of a bignum against a specific term — the order itself is
/// `beam_term::compare`'s; `beam_bifs` never re-derives term order.
///
/// # Scope note: two upstream `beam_term` ordering defects
///
/// `beam_term::thing::write_big` stores a bignum header's arity as `digits - 1`,
/// but `beam_term::compare` reads that field *as* the digit count:
///
/// 1. `number_as_f64` — the arithmetic-mode (`==`, `<`, `>`, `=<`, `>=`)
///    bignum→`f64` conversion — drops the most significant limb, so
///    `(1 bsl 70) > 1.0` compares `false` (oracle `true`).
/// 2. `cmp_bigs` — the exact-mode bignum/bignum loop — also drops the most
///    significant limb, so `(63 bsl 128) =:= (1 bsl 134)` compares `true`
///    (oracle `false`).
///
/// Both are pre-existing and live in the sibling `beam_term` crate, so this test
/// pins only the pairs that are exact today: a `small` against a `big` (decided
/// by the bignum's sign, `cmp_numbers`' `(Small, Big)` arms), a `big` against
/// itself, and single-limb bignums. `beam_bifs` must not re-implement term order
/// (crate `CONTEXT.md`, Constraints) — the defects are reported upstream instead,
/// together with the `beam_term::thing::write_big` header-arity deviation from
/// the C `BIG_ARITY` (which is the digit count, `big.h:70`).
#[test]
fn mixed_small_and_bignum_comparisons_use_the_term_order() {
    let mut fx = Fixture::new();
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let minus_two_to_70 = fx.bignum(true, &[0, 64]);
    let small_operand = beam_term::make_small(123456789012345678);

    // A positive bignum is above every small integer; a negative one is below.
    let greater = fx.call(">", &[two_to_70, small_operand]).expect("erlang:'>'/2");
    assert!(fx.boolean(greater));
    let less = fx.call("<", &[small_operand, two_to_70]).expect("erlang:'<'/2");
    assert!(fx.boolean(less));
    let less = fx.call("<", &[minus_two_to_70, small_operand]).expect("erlang:'<'/2");
    assert!(fx.boolean(less));
    let greater_equal = fx.call(">=", &[small_operand, minus_two_to_70]).expect("erlang:'>='/2");
    assert!(fx.boolean(greater_equal));
    let less_equal = fx.call("=<", &[two_to_70, minus_two_to_70]).expect("erlang:'=<'/2");
    assert!(!fx.boolean(less_equal));

    // The equal-value cases: a bignum is exactly equal to itself.
    let same_bignum = fx.call("=:=", &[two_to_70, two_to_70]).expect("erlang:'=:='/2");
    assert!(fx.boolean(same_bignum));
    let equal = fx.call("==", &[two_to_70, two_to_70]).expect("erlang:'=='/2");
    assert!(fx.boolean(equal));
    let not_equal = fx.call("=/=", &[two_to_70, two_to_70]).expect("erlang:'=/='/2");
    assert!(!fx.boolean(not_equal));

    // Distinct terms: a bignum is never `=:=` a smaller small integer, and a
    // separately built single-limb bignum of the same value is.
    let bignum_vs_small = fx.call("=:=", &[two_to_70, small_operand]).expect("erlang:'=:='/2");
    assert!(!fx.boolean(bignum_vs_small));
    let same_again = fx.bignum(false, &[0, 64]);
    let rebuilt = fx.call("=:=", &[two_to_70, same_again]).expect("erlang:'=:='/2");
    assert!(fx.boolean(rebuilt));
}

#[test]
fn bignum_operands_mix_with_floats_like_the_oracle() {
    let mut fx = Fixture::new();
    // 123456789012345678901234567890, least significant limb first.
    let decimal = fx.bignum(false, &[14083847773837265618, 6692605942]);
    let one_point_oh = fx.float(1.0);
    let two = beam_term::make_small(2);

    // `123456789012345678901234567890 + 1.0` ⇒ 1.2345678901234568e29.
    let sum = fx.call("+", &[decimal, one_point_oh]).expect("erlang:'+'/2");
    assert_eq!(float(sum), 1.2345678901234568e29);
    // `123456789012345678901234567890 / 2` ⇒ 6.172839450617284e28.
    let quotient = fx.call("/", &[decimal, two]).expect("erlang:'/'/2");
    assert_eq!(float(quotient), 6.172839450617284e28);

    // `(1 bsl 70) + 0.5` ⇒ 1.1805916207174113e21.
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let half = fx.float(0.5);
    let sum = fx.call("+", &[two_to_70, half]).expect("erlang:'+'/2");
    assert_eq!(float(sum), 1.1805916207174113e21);

    // An integer beyond the double range makes the mixed operation `badarith` (the
    // oracle's `big_to_double` failure): `(1 bsl 1025)`.
    let mut huge_digits = vec![0u64; 16];
    huge_digits.push(2);
    let huge = fx.bignum(false, &huge_digits);
    assert!(matches!(
        fx.call("+", &[huge, one_point_oh]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(fx.call("/", &[huge, two]), Err(BeamError::BadArith)));
}

#[test]
fn bad_operands_stay_badarg_family_errors() {
    let mut fx = Fixture::new();
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let zero = beam_term::make_small(0);
    let one_point_oh = fx.float(1.0);
    let ok = fx.call("*", &[two_to_70, beam_term::make_small(1)]).expect("erlang:'*'/2");

    // A zero divisor is `badarith` for both `div` and `rem` and `/`.
    assert!(matches!(fx.call("div", &[two_to_70, zero]), Err(BeamError::BadArith)));
    assert!(matches!(fx.call("rem", &[two_to_70, zero]), Err(BeamError::BadArith)));
    assert!(matches!(fx.call("/", &[two_to_70, zero]), Err(BeamError::BadArith)));
    // A float operand is `badarith` for `div`/`rem` (they are integer operations).
    assert!(matches!(fx.call("div", &[one_point_oh, two_to_70]), Err(BeamError::BadArith)));
    assert!(matches!(fx.call("rem", &[two_to_70, one_point_oh]), Err(BeamError::BadArith)));
    // A non-number is `badarith` (`true + 1` in the oracle).
    let truth = fx.call("=:=", &[two_to_70, two_to_70]).expect("erlang:'=:='/2");
    assert!(matches!(fx.call("+", &[truth, ok]), Err(BeamError::BadArith)));
    assert!(matches!(fx.call("abs", &[truth]), Err(BeamError::BadArith)));
}
