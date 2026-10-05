//! Arithmetic BIFs for `erlang`: `+`, `-`, `*`, `/`, `div`, `rem`, `abs` and the
//! unary `-`/`+`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_op.c` (the
//! operator BIFs and their guard variants) and `erl_arith.c` (`erts_arith_*`),
//! plus the `ubif erlang:'+'/2` … roster in
//! `../../erts/emulator/beam/bif.tab` (lines 295–316: `'>' '>=' '<' '=<' '=:='
//! '==' '=/' '/=' '+' '-' '*' '/' 'div' 'rem' '-' '/'`). The names are registered
//! *quoted*, exactly as `bif.tab` spells them.
//!
//! # Milestone-1 scope (documented gaps)
//!
//! Operands are the two numeric classes `beam_term` can build in M1 — a
//! **60-bit small integer** and an **`f64` float**. Consequences, all faithful
//! to the rewrite's stated M1 limits rather than to real Erlang:
//!
//! * A result outside the small range is [`BeamError::SystemLimit`]; real Erlang
//!   promotes to a bignum, which `beam_term` has no arithmetic for.
//! * A bignum operand is *not constructible* in M1 (`beam_term` exposes no safe
//!   constructor), so `is_small`/`is_float` is the complete number classification
//!   and everything else is [`BeamError::BadArith`].
//!
//! # Semantics pinned against the OTP 29 oracle
//!
//! * `+ - *`: integer when both operands are integers, float if either is a
//!   float (`1 + 1.5` ⇒ `2.5`).
//! * `abs/1`: `abs(-1.0)` ⇒ `1.0`.
//! * unary `-`: `-1.5` ⇒ `-1.5`; unary `+`: the number, unchanged.
//! * `div`/`rem`: integers only (`1.0 div 2` ⇒ `badarith`), truncating toward
//!   zero (`-7 div 2` ⇒ `-3`, `-7 rem 2` ⇒ `-1`, `7 div -2` ⇒ `-3`,
//!   `7 rem -2` ⇒ `1`); a zero divisor is `badarith`.
//! * `/`: *always* a float (`2/1` ⇒ `2.0`); a zero divisor is `badarith`.

use beam_common::Result;
use beam_term::{signed_value, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::{badarith, system_limit};
use crate::registry::BifRegistry;

/// A numeric operand, in one of the two classes M1 can represent.
#[derive(Clone, Copy)]
enum Num {
    /// A 60-bit small integer.
    Int(i64),
    /// An `f64` float.
    Float(f64),
}

/// Classifies `t` as one of the two M1 numeric classes.
///
/// # Errors
///
/// [`BeamError::BadArith`](beam_common::BeamError::BadArith) for anything that is
/// not a small integer or a float (matching the oracle's `badarith` for
/// `1 + a`, and the M1 limit for the unconstructible bignum).
fn number(t: Eterm) -> Result<Num> {
    if access::is_small(t) {
        Ok(Num::Int(signed_value(t)))
    } else if access::is_float(t) {
        Ok(Num::Float(access::float_value(t)))
    } else {
        Err(badarith())
    }
}

/// Classifies `t` as an **integer** operand (the `div`/`rem` domain).
///
/// # Errors
///
/// Like [`number`]: a non-number *and* a float are both `badarith`
/// (`1.0 div 2` ⇒ `badarith` in the oracle).
fn integer(t: Eterm) -> Result<i64> {
    match number(t)? {
        Num::Int(i) => Ok(i),
        Num::Float(_) => Err(badarith()),
    }
}

/// The `f64` value of a numeric operand (an integer is promoted).
fn as_float(n: Num) -> f64 {
    match n {
        Num::Int(i) => i as f64,
        Num::Float(f) => f,
    }
}

/// `A + B`.
fn add(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match (number(args[0])?, number(args[1])?) {
        (Num::Int(x), Num::Int(y)) => ctx.int(x.checked_add(y).ok_or_else(system_limit)?),
        (x, y) => ctx.make_float(as_float(x) + as_float(y)),
    }
}

/// `A - B`.
fn sub(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match (number(args[0])?, number(args[1])?) {
        (Num::Int(x), Num::Int(y)) => ctx.int(x.checked_sub(y).ok_or_else(system_limit)?),
        (x, y) => ctx.make_float(as_float(x) - as_float(y)),
    }
}

/// `A * B`.
fn mul(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match (number(args[0])?, number(args[1])?) {
        (Num::Int(x), Num::Int(y)) => ctx.int(x.checked_mul(y).ok_or_else(system_limit)?),
        (x, y) => ctx.make_float(as_float(x) * as_float(y)),
    }
}

/// `A / B` — always a float.
fn divide(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let x = as_float(number(args[0])?);
    let y = as_float(number(args[1])?);
    if y == 0.0 {
        return Err(badarith());
    }
    ctx.make_float(x / y)
}

/// `A div B` — integer division, truncating toward zero.
fn int_div(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let x = integer(args[0])?;
    let y = integer(args[1])?;
    if y == 0 {
        return Err(badarith());
    }
    // `i64::MIN / -1` overflows in Rust (a panic, not a wrong answer). It cannot
    // arise from 60-bit smalls, but the guard keeps the operation total.
    if x == i64::MIN && y == -1 {
        return Err(system_limit());
    }
    ctx.int(x / y)
}

/// `A rem B` — the remainder of a division truncating toward zero.
fn int_rem(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let x = integer(args[0])?;
    let y = integer(args[1])?;
    if y == 0 {
        return Err(badarith());
    }
    // As in [`int_div`]: `i64::MIN % -1` is an overflow in Rust.
    if x == i64::MIN && y == -1 {
        return Err(system_limit());
    }
    ctx.int(x % y)
}

/// `abs(A)`.
fn abs(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match number(args[0])? {
        Num::Int(i) => ctx.int(i.checked_abs().ok_or_else(system_limit)?),
        Num::Float(f) => ctx.make_float(f.abs()),
    }
}

/// Unary `-A` (negation).
fn negate(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match number(args[0])? {
        Num::Int(i) => ctx.int(i.checked_neg().ok_or_else(system_limit)?),
        Num::Float(f) => ctx.make_float(-f),
    }
}

/// Unary `+A` (numeric identity).
fn identity(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match number(args[0])? {
        Num::Int(i) => ctx.int(i),
        Num::Float(f) => ctx.make_float(f),
    }
}

/// Registers the `erlang` arithmetic BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "+", 2, add);
    reg.register("erlang", "-", 2, sub);
    reg.register("erlang", "*", 2, mul);
    reg.register("erlang", "/", 2, divide);
    reg.register("erlang", "div", 2, int_div);
    reg.register("erlang", "rem", 2, int_rem);
    reg.register("erlang", "abs", 1, abs);
    reg.register("erlang", "-", 1, negate);
    reg.register("erlang", "+", 1, identity);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, MAX_SMALL, MIN_SMALL};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a", "b"]);
        atoms
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    fn int(t: Eterm) -> i64 {
        assert!(access::is_small(t), "expected a small int, got {t:?}");
        signed_value(t)
    }

    fn float(t: Eterm) -> f64 {
        assert!(access::is_float(t), "expected a float, got {t:?}");
        access::float_value(t)
    }

    #[test]
    fn integer_addition_subtraction_multiplication() {
        with_ctx(|ctx| {
            assert_eq!(int(add(ctx, &[make_small(2), make_small(3)]).unwrap()), 5);
            assert_eq!(int(sub(ctx, &[make_small(2), make_small(3)]).unwrap()), -1);
            assert_eq!(int(mul(ctx, &[make_small(4), make_small(6)]).unwrap()), 24);
        });
    }

    #[test]
    fn division_always_returns_a_float() {
        with_ctx(|ctx| {
            assert_eq!(float(divide(ctx, &[make_small(2), make_small(1)]).unwrap()), 2.0);
            assert_eq!(float(divide(ctx, &[make_small(7), make_small(2)]).unwrap()), 3.5);
        });
    }

    #[test]
    fn mixed_int_float_promotes_to_float() {
        with_ctx(|ctx| {
            let half = ctx.make_float(1.5).unwrap();
            assert_eq!(float(add(ctx, &[make_small(1), half]).unwrap()), 2.5);
            assert_eq!(float(sub(ctx, &[half, make_small(1)]).unwrap()), 0.5);
            assert_eq!(float(mul(ctx, &[half, make_small(2)]).unwrap()), 3.0);
        });
    }

    #[test]
    fn div_and_rem_truncate_toward_zero() {
        with_ctx(|ctx| {
            assert_eq!(int(int_div(ctx, &[make_small(-7), make_small(2)]).unwrap()), -3);
            assert_eq!(int(int_div(ctx, &[make_small(7), make_small(-2)]).unwrap()), -3);
            assert_eq!(int(int_rem(ctx, &[make_small(-7), make_small(2)]).unwrap()), -1);
            assert_eq!(int(int_rem(ctx, &[make_small(7), make_small(-2)]).unwrap()), 1);
        });
    }

    #[test]
    fn abs_and_unary_operators() {
        with_ctx(|ctx| {
            assert_eq!(int(abs(ctx, &[make_small(-5)]).unwrap()), 5);
            assert_eq!(int(abs(ctx, &[make_small(5)]).unwrap()), 5);
            let neg = ctx.make_float(-1.5).unwrap();
            assert_eq!(float(abs(ctx, &[neg]).unwrap()), 1.5);

            assert_eq!(int(negate(ctx, &[make_small(5)]).unwrap()), -5);
            let half = ctx.make_float(1.5).unwrap();
            assert_eq!(float(negate(ctx, &[half]).unwrap()), -1.5);
            assert_eq!(int(identity(ctx, &[make_small(-5)]).unwrap()), -5);
            assert_eq!(float(identity(ctx, &[half]).unwrap()), 1.5);
        });
    }

    #[test]
    fn non_number_operands_are_badarith() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            assert!(matches!(add(ctx, &[make_small(1), a]), Err(BeamError::BadArith)));
            assert!(matches!(abs(ctx, &[a]), Err(BeamError::BadArith)));
            assert!(matches!(negate(ctx, &[a]), Err(BeamError::BadArith)));
            assert!(matches!(identity(ctx, &[a]), Err(BeamError::BadArith)));
            assert!(matches!(divide(ctx, &[make_small(1), a]), Err(BeamError::BadArith)));
        });
    }

    #[test]
    fn zero_divisors_are_badarith() {
        with_ctx(|ctx| {
            assert!(matches!(int_div(ctx, &[make_small(1), make_small(0)]), Err(BeamError::BadArith)));
            assert!(matches!(int_rem(ctx, &[make_small(1), make_small(0)]), Err(BeamError::BadArith)));
            assert!(matches!(divide(ctx, &[make_small(1), make_small(0)]), Err(BeamError::BadArith)));
            // A float zero divisor is `badarith` too (`1.0/0.0` in the oracle).
            let zero = ctx.make_float(0.0).unwrap();
            assert!(matches!(divide(ctx, &[make_small(1), zero]), Err(BeamError::BadArith)));
        });
    }

    #[test]
    fn float_operands_are_badarith_for_div_and_rem() {
        with_ctx(|ctx| {
            let one = ctx.make_float(1.0).unwrap();
            assert!(matches!(int_div(ctx, &[one, make_small(2)]), Err(BeamError::BadArith)));
            assert!(matches!(int_div(ctx, &[make_small(2), one]), Err(BeamError::BadArith)));
            assert!(matches!(int_rem(ctx, &[one, make_small(2)]), Err(BeamError::BadArith)));
        });
    }

    #[test]
    fn overflow_past_the_small_range_is_system_limit() {
        with_ctx(|ctx| {
            // MAX_SMALL + 1 promotes to a bignum in real Erlang; M1 reports the limit.
            assert!(matches!(
                add(ctx, &[make_small(MAX_SMALL), make_small(1)]),
                Err(BeamError::SystemLimit)
            ));
            // MIN_SMALL - 1 likewise.
            assert!(matches!(
                sub(ctx, &[make_small(MIN_SMALL), make_small(1)]),
                Err(BeamError::SystemLimit)
            ));
            // A product far outside the small range.
            assert!(matches!(
                mul(ctx, &[make_small(MAX_SMALL), make_small(MAX_SMALL)]),
                Err(BeamError::SystemLimit)
            ));
            // `abs(MIN_SMALL)` is `2^59`, just past `MAX_SMALL`.
            assert!(matches!(abs(ctx, &[make_small(MIN_SMALL)]), Err(BeamError::SystemLimit)));
            // `-MIN_SMALL` likewise.
            assert!(matches!(negate(ctx, &[make_small(MIN_SMALL)]), Err(BeamError::SystemLimit)));
            // `MIN_SMALL div -1` ⇒ `2^59`.
            assert!(matches!(
                int_div(ctx, &[make_small(MIN_SMALL), make_small(-1)]),
                Err(BeamError::SystemLimit)
            ));
        });
    }

    #[test]
    fn register_installs_the_quoted_operator_names() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("+", 2),
            ("-", 2),
            ("*", 2),
            ("/", 2),
            ("div", 2),
            ("rem", 2),
            ("abs", 1),
            ("-", 1),
            ("+", 1),
        ] {
            assert!(reg.contains("erlang", name, arity), "missing erlang:{name}/{arity}");
        }
        assert_eq!(reg.len(), 9);
    }
}
