//! `erlang:phash/2` — the legacy ERTS `make_hash` term hash.
//!
//! Ground truth (read-only reference; never edited):
//! `../../erts/emulator/beam/erl_term_hashing.c` — `hash_binary_bytes` (lines
//! 114–163) and `make_hash` (lines 165–425), the `bif erlang:phash/2` entry in
//! `../../erts/emulator/beam/bif.tab:82`, the BIF wrapper `phash_2`
//! (`../../erts/emulator/beam/bif.c:5370-5399`), the atom hash `atom_hash`
//! (`../../erts/emulator/beam/atom.c:82-105`) and `term_equals_2pow32`
//! (`../../erts/emulator/beam/big.c:4038-4044`).
//!
//! This is the **legacy** hash. `phash2/1,2` uses `make_hash2` (+ `MIX`) and is a
//! different function; it is registered as a typed `unsupported_bif` capability gap.
//!
//! # The algorithm
//!
//! A single `u32` accumulator `h`, starting at `0`, is threaded through the term;
//! every step is a multiply and an add, so [`u32::wrapping_mul`] /
//! [`u32::wrapping_add`] reproduce the C word arithmetic bit-exactly (each C
//! result is used only mod 2^32).
//!
//! * `[]` — `h*F3 + 1`.
//! * atom — `h*F1 + atom_hvalue(name)` (hashpjw over the atom's name bytes).
//! * small integer — a 32-bit byte-wise step over `|v|` (twice, when
//!   `|v| >= 2^32`), then `h*(v < 0 ? F4 : F3)`.
//! * bignum — the *same* bytes of `|N|` as the small path, least-significant byte
//!   first: eight bytes per full limb, four or eight for the top limb, then
//!   `h*(negative ? F4 : F3)`.
//! * float — `-0.0` is normalised to `+0.0`, then
//!   `h*F6 + (low_bits ^ high_bits)`.
//! * binary/bitstring — the significant bytes (MSB-first packing) via
//!   `hash_binary_bytes`, then the trailing partial byte as
//!   `(h*F1 + b)*F12 + bitsize`, then `h*F4 + bytesize`.
//! * tuple — every element left to right, then `h*F9 + arity`.
//! * list — `h*F2 + byte` per byte-sized head element, a full recursive hash for
//!   any other head, and exactly one `h*F8` at the terminating non-list tail
//!   (`[]` included — [`beam_term::is_list`] is **false** for `NIL`).
//! * pid — `UINT32_HASH_STEP(number, F5)`, then `h*F6`.
//! * port — the 64-bit number: a step over the high half (when set) with `F11`,
//!   a step over the low half with `F9`, then `h*F10`.
//! * external fun — `h*F11 + arity`, then `h*F1 + atom_hvalue(module)`, then
//!   `h*F1 + atom_hvalue(function)`.
//!
//! # Reproduction limits (always a typed error, never a wrong number)
//!
//! `make_hash`'s **map** and **record** branches read `make_hash2` / a record's
//! internals, an **internal reference** read reaches through the reference's
//! container (`internal_ref_numbers`, `erl_node_container_utils.h`), and a
//! **local/imported fun**'s C identity (`ErlFunEntry.module`/`index`/`old_uniq`)
//! is not carried by this tree's inlined fun descriptor ([`beam_term::fun`]). None
//! of those have a faithful `u32` here, so they raise the typed
//! [`unsupported_bif`] — see the crate `CONTEXT.md`, Known Issues.
//!
//! # `unsafe`
//!
//! None: every accessor goes through `crate::access` (the crate's only
//! `#[allow(unsafe_code)]` module) or through `beam_codec::term`'s safe readers.

use beam_codec::term::{fun_arity_term, fun_function, fun_kind_term, fun_module, FunKind};
use beam_common::{BeamError, Result};
use beam_term::{AtomTable, Eterm};

use crate::access;
use crate::arith;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

// ---------------------------------------------------------------------------
// The fifteen `FUNNY_NUMBER*` primes (`erl_term_hashing.c:98-112`)
// ---------------------------------------------------------------------------
//
// `F7` and `F13`–`F15` are deliberately absent: they belong to the map
// (`hash*F13 + F14 + make_hash2(term)`) and record (`hash*F15 + arity`) branches,
// which raise `unsupported_bif` (see the module docs).

/// `FUNNY_NUMBER1` — the atom/binary/byte step prime.
const F1: u32 = 268_440_163;
/// `FUNNY_NUMBER2` — the small-int/bignum/list-byte step prime.
const F2: u32 = 268_439_161;
/// `FUNNY_NUMBER3` — the non-negative magnitude factor.
const F3: u32 = 268_435_459;
/// `FUNNY_NUMBER4` — the negative magnitude / bitstring-tail factor.
const F4: u32 = 268_436_141;
/// `FUNNY_NUMBER5` — the pid-number step prime.
const F5: u32 = 268_438_633;
/// `FUNNY_NUMBER6` — the pid/float factor.
const F6: u32 = 268_437_017;
/// `FUNNY_NUMBER8` — the end-of-list factor.
const F8: u32 = 268_437_511;
/// `FUNNY_NUMBER9` — the tuple/port-low step prime.
const F9: u32 = 268_439_627;
/// `FUNNY_NUMBER10` — the ref/port factor.
const F10: u32 = 268_440_479;
/// `FUNNY_NUMBER11` — the external-fun/port-high step prime.
const F11: u32 = 268_440_577;
/// `FUNNY_NUMBER12` — the bitstring trailing-bits factor.
const F12: u32 = 268_440_581;

/// `UINT32_HASH_STEP(x, prime)` (`erl_term_hashing.c:184-192`): fold the four
/// bytes of `x`, most significant last, into the accumulator.
#[inline]
fn step(h: u32, x: u32, prime: u32) -> u32 {
    let h = h.wrapping_mul(prime).wrapping_add(x & 0xFF);
    let h = h.wrapping_mul(prime).wrapping_add((x >> 8) & 0xFF);
    let h = h.wrapping_mul(prime).wrapping_add((x >> 16) & 0xFF);
    h.wrapping_mul(prime).wrapping_add(x >> 24)
}

// ---------------------------------------------------------------------------
// Atom hashpjw (`atom.c:82-105`)
// ---------------------------------------------------------------------------

/// `atom_tab(i)->slot.bucket.hvalue` — the hashpjw value [`make_hash`] adds for an
/// atom, computed over the atom's name **bytes**.
///
/// The accumulator is a **`u64`** (the C `HashValue` is `typedef UWord HashValue`,
/// `hash.h:32`) and only the low 32 bits are used, because the C adds the 64-bit
/// hvalue into a 32-bit `Uint32 hash` (so the sum is truncated). The `0xf0000000`
/// mask clears bits 28–31 only: for a long name the remaining high bits survive and
/// the accumulator keeps growing, which `u64` wrapping reproduces exactly.
///
/// The "latin1 clutch for r16" is reproduced too: a `0xC2`/`0xC3` lead byte
/// followed by a `0x80..=0xBF` continuation collapses into the single latin1 byte
/// it encodes (`(v << 6) | (next & 0x3F)` truncated to eight bits).
fn atom_hvalue(name: &str) -> u32 {
    let bytes = name.as_bytes();
    let mut h: u64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let mut v = bytes[i];
        let mut width = 1;
        if i + 1 < bytes.len() && (v & 0xFE) == 0xC2 && (bytes[i + 1] & 0xC0) == 0x80 {
            // The C assigns into a `byte v`, so the shifted value is truncated.
            v = (((u32::from(v) << 6) | u32::from(bytes[i + 1] & 0x3F)) & 0xFF) as u8;
            width = 2;
        }
        h = h.wrapping_shl(4).wrapping_add(u64::from(v));
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
            h ^= g;
        }
        i += width;
    }
    (h & 0xFFFF_FFFF) as u32
}

// ---------------------------------------------------------------------------
// The term walk
// ---------------------------------------------------------------------------

/// `make_hash(term)` starting from accumulator `h`, with the atom table needed to
/// resolve atom names.
///
/// Lists are walked iteratively; only *nested* containers recurse (exactly as the
/// crate's other term walkers do).
///
/// # Errors
///
/// [`unsupported_bif`] for a tag class this crate cannot reproduce exactly (map,
/// record, reference, local/imported fun, …) and [`BeamError::TermNotFound`] for
/// an atom index absent from `atoms` — never a wrong hash.
fn hash_term(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    // `[]` is an immediate, and `is_list(NIL)` is false (see the module docs).
    if beam_term::is_nil(term) {
        return Ok(h.wrapping_mul(F3).wrapping_add(1));
    }
    if beam_term::is_small(term) {
        let y1 = beam_term::signed_value(term);
        let y2 = y1.unsigned_abs();
        let mut hash = step(h, y2 as u32, F2);
        if (y2 >> 32) != 0 {
            hash = step(hash, (y2 >> 32) as u32, F2);
        }
        return Ok(hash.wrapping_mul(if y1 < 0 { F4 } else { F3 }));
    }
    if beam_term::is_atom(term) {
        let name = atoms
            .name(beam_term::atom_val(term))
            .ok_or(BeamError::TermNotFound)?;
        return Ok(h.wrapping_mul(F1).wrapping_add(atom_hvalue(name)));
    }
    if access::is_list(term) {
        return hash_list(term, atoms, h);
    }
    if access::is_tuple(term) {
        let arity = access::tuple_size(term);
        let mut hash = h;
        for i in 0..arity {
            hash = hash_term(access::tuple_element(term, i), atoms, hash)?;
        }
        return Ok(hash.wrapping_mul(F9).wrapping_add(arity as u32));
    }
    if access::is_float(term) {
        let value = access::float_value(term);
        // The C forces a positive zero (`refs_get_positive_zero_float`), so `-0.0`
        // hashes like `+0.0`.
        let bits = if value == 0.0 { 0 } else { value.to_bits() };
        return Ok(h
            .wrapping_mul(F6)
            .wrapping_add((bits as u32) ^ ((bits >> 32) as u32)));
    }
    if access::is_big(term) {
        return hash_big(term, h);
    }
    if access::is_bitstring(term) {
        return hash_bitstring(term, h);
    }
    if access::is_internal_pid(term) {
        return Ok(step(h, beam_term::pid_number(term) as u32, F5).wrapping_mul(F6));
    }
    if access::is_external_pid(term) {
        let (_, number, _, _) = beam_codec::term::external_pid_parts(term)?;
        return Ok(step(h, number, F5).wrapping_mul(F6));
    }
    if access::is_internal_port(term) {
        return Ok(hash_port(h, beam_term::port_number(term)));
    }
    if access::is_external_port(term) {
        let (_, number, _) = beam_codec::term::external_port_parts(term)?;
        return Ok(hash_port(h, number));
    }
    if access::is_function(term) {
        return hash_fun(term, atoms, h);
    }
    // Map, record, reference and every other tag class: `make_hash` reads state
    // this tree's terms do not carry (see the module docs).
    Err(unsupported_bif())
}

/// The `LIST_DEF` rule (`erl_term_hashing.c:310-333`).
///
/// A byte-sized head element folds in with `F2` (the "optimization for strings");
/// any other head is hashed recursively. The walk stops at the first non-list tail,
/// which is hashed and then multiplied by `F8` — so each list level applies exactly
/// one `*F8`, at its terminator.
fn hash_list(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    let mut cur = term;
    let mut hash = h;
    loop {
        let car = access::car(cur);
        let cdr = access::cdr(cur);
        if beam_term::is_byte(car) {
            hash = hash
                .wrapping_mul(F2)
                .wrapping_add(beam_term::unsigned_value(car) as u32);
        } else {
            hash = hash_term(car, atoms, hash)?;
        }
        if !access::is_list(cdr) {
            return Ok(hash_term(cdr, atoms, hash)?.wrapping_mul(F8));
        }
        cur = cdr;
    }
}

/// The `BIG_DEF` rule (`erl_term_hashing.c:335-365`) — byte-identical to the small
/// integer path for equal magnitudes.
fn hash_big(term: Eterm, h: u32) -> Result<u32> {
    let (negative, digits) = beam_codec::term::big_parts(term)?;
    let mut hash = h;
    if let Some((&top, rest)) = digits.split_last() {
        for &digit in rest {
            let mut d = digit;
            for _ in 0..8 {
                hash = hash.wrapping_mul(F2).wrapping_add((d & 0xFF) as u32);
                d >>= 8;
            }
        }
        // `k = sizeof(ErtsDigit)`, halved on `ARCH_64` when the top limb fits 32 bits.
        let bytes = if (top >> 32) != 0 { 8 } else { 4 };
        let mut d = top;
        for _ in 0..bytes {
            hash = hash.wrapping_mul(F2).wrapping_add((d & 0xFF) as u32);
            d >>= 8;
        }
    }
    Ok(hash.wrapping_mul(if negative { F4 } else { F3 }))
}

/// `hash_binary_bytes` (`erl_term_hashing.c:114-163`).
///
/// The C shifts the *unaligned* bit stream in place, which is exactly the MSB-first
/// byte packing [`access::bitstring_bits`] produces: packed byte `i` holds bits
/// `[8i, 8i+8)` of the bitstring, so both the `bitoffs == 0` loop and the
/// `bitoffs != 0` shift loop add the same bytes. The trailing `bitsize = size % 8`
/// bits are the **high** bits of packed byte `bytesize`, and the result ends with
/// `h*F4 + bytesize` — the count of *whole* bytes.
fn hash_bitstring(term: Eterm, h: u32) -> Result<u32> {
    let bit_len = access::bitstring_bit_size(term)?;
    let packed = access::bitstring_bits(term)?;
    let bytesize = bit_len / 8;
    let bitsize = bit_len % 8;

    let mut hash = h;
    for &byte in &packed[..bytesize] {
        hash = hash.wrapping_mul(F1).wrapping_add(u32::from(byte));
    }
    if bitsize > 0 {
        let tail = u32::from(packed[bytesize] >> (8 - bitsize));
        hash = hash
            .wrapping_mul(F1)
            .wrapping_add(tail)
            .wrapping_mul(F12)
            .wrapping_add(bitsize as u32);
    }
    Ok(hash.wrapping_mul(F4).wrapping_add(bytesize as u32))
}

/// The `PORT_DEF`/`EXTERNAL_PORT_DEF` rule (`erl_term_hashing.c:279-287`).
fn hash_port(h: u32, number: u64) -> u32 {
    let low = number as u32;
    let high = (number >> 32) as u32;
    let hash = if high != 0 { step(h, high, F11) } else { h };
    step(hash, low, F9).wrapping_mul(F10)
}

/// The `FUN_DEF` rule for the one kind this crate can reproduce.
///
/// An **external** fun (`EXPORT_EXT`) carries the exported MFA's module and
/// function atoms plus the arity in its header, so
/// `h*F11 + arity; h*F1 + hvalue(module); h*F1 + hvalue(function)` is exact.
/// A local (`make_fun3`) or imported (`NEW_FUN_EXT`) fun's C identity
/// (`ErlFunEntry.module`/`index`/`old_uniq`, and the free-variable environment) is
/// not part of this tree's inlined descriptor, so those raise `unsupported_bif`.
fn hash_fun(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    match fun_kind_term(term)? {
        FunKind::External => {
            let arity = fun_arity_term(term)? as u32;
            let module = atoms
                .name(fun_module(term)?)
                .ok_or(BeamError::TermNotFound)?;
            let function = atoms
                .name(fun_function(term)?)
                .ok_or(BeamError::TermNotFound)?;
            let hash = h.wrapping_mul(F11).wrapping_add(arity);
            let hash = hash.wrapping_mul(F1).wrapping_add(atom_hvalue(module));
            Ok(hash.wrapping_mul(F1).wrapping_add(atom_hvalue(function)))
        }
        FunKind::Local | FunKind::Imported => Err(unsupported_bif()),
    }
}

// ---------------------------------------------------------------------------
// The BIF
// ---------------------------------------------------------------------------

/// `phash_2`'s `Range` argument (`bif.c:5376-5385`) — the `Uint32` modulus, or `0`
/// for the special `2^32` range.
///
/// # Errors
///
/// `badarg` for anything that is not an integer `u` with `1 <= u <= 0xFFFF_FFFF`
/// (`term_to_Uint` followed by the C's `((u >> 16) >> 16) != 0 || !u` check) — a
/// negative or too-large integer, a float, an atom, a list, `Range = 0`. The value
/// `2^32` is *accepted* and mapped to `0` (`term_equals_2pow32`).
fn range_of(term: Eterm) -> Result<u32> {
    let value: u64 = if beam_term::is_small(term) {
        let v = beam_term::signed_value(term);
        if v < 0 {
            return Err(badarg());
        }
        v as u64
    } else if access::is_big(term) {
        let (negative, digits) = beam_codec::term::big_parts(term)?;
        if negative {
            return Err(badarg());
        }
        // The magnitude with any (non-canonical) leading zero limbs dropped.
        let significant = match digits.iter().rposition(|&d| d != 0) {
            Some(top) => &digits[..=top],
            None => &digits[..0],
        };
        match significant {
            [] => 0,
            [single] => *single,
            // More than one significant limb is far beyond 2^32 (and u64).
            _ => return Err(badarg()),
        }
    } else {
        return Err(badarg());
    };

    if value == (1u64 << 32) {
        return Ok(0);
    }
    if (1..=0xFFFF_FFFF).contains(&value) {
        return Ok(value as u32);
    }
    Err(badarg())
}

/// `erlang:phash(Term, Range)` (`bif.tab:82`, `phash_2`).
///
/// # Errors
///
/// `badarg` for an invalid `Range` and the `hash_term` errors above.
fn phash_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let range = range_of(args[1])?;
    let hash = hash_term(args[0], ctx.atoms(), 0)?;
    // `range != 0` ⇒ `1 + (hash % range)`, i.e. `[1..range]`; the `2^32` range
    // (`range == 0`) ⇒ `hash + 1` (one past the top of the `u32` space, promoted
    // exactly as the C `erts_mixed_plus` does).
    let final_hash: u64 = if range != 0 {
        u64::from(hash % range) + 1
    } else {
        u64::from(hash) + 1
    };
    arith::int_from_i64(ctx, final_hash as i64)
}

/// Registers `erlang:phash/2` plus the `erlang:phash2/1,2` capability gaps.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "phash", 2, phash_2);
    reg.register("erlang", "phash2", 1, phash2_gap);
    reg.register("erlang", "phash2", 2, phash2_gap);
}

/// `erlang:phash2/1,2` — the *other* hash (`make_hash2`), a capability gap.
///
/// These are real `bif.tab` BIFs (`../../erts/emulator/beam/bif.tab:83-84`) whose
/// algorithm (`make_hash2`, a `MIX`-based hash unrelated to `phash`'s `make_hash`)
/// Milestone-1 does not reproduce. Registering them to raise `unsupported_bif`
/// (which the runtime reports as a documented capability gap / exit 3) is the
/// honest classification — never a plain `undef`, and never a guessed value.
fn phash2_gap(_ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    Err(unsupported_bif())
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pieces `../../tests/phash.rs` cannot reach directly: the
    //! `atom_hash` hashpjw (including the latin1 clutch) and the bit-exactness of
    //! the wrapping `step` helper. The oracle-pinned end-to-end values live in the
    //! integration suite.

    use super::*;

    #[test]
    fn step_folds_bytes_most_significant_last() {
        // `(((0*F1 + 0x11) * F1 + 0x22) * F1 + 0x33) * F1 + 0x44`, wrapping.
        let expected = 0x11u32
            .wrapping_mul(F1)
            .wrapping_add(0x22)
            .wrapping_mul(F1)
            .wrapping_add(0x33)
            .wrapping_mul(F1)
            .wrapping_add(0x44);
        assert_eq!(step(0, 0x4433_2211, F1), expected);
    }

    #[test]
    fn atom_hvalue_matches_hashpjw_for_ascii() {
        // Hand-computed hashpjw for "abc" (3 bytes, no clutch triggered).
        let mut h: u64 = 0;
        for v in b"abc" {
            h = (h << 4) + u64::from(*v);
            let g = h & 0xf000_0000;
            if g != 0 {
                h ^= g >> 24;
                h ^= g;
            }
        }
        assert_eq!(atom_hvalue("abc"), (h & 0xFFFF_FFFF) as u32);
        // The empty name hashes to 0 (no iterations).
        assert_eq!(atom_hvalue(""), 0);
    }

    #[test]
    fn atom_hvalue_applies_the_latin1_clutch() {
        // U+0080..U+00FF is a two-byte UTF-8 sequence (0xC2/0xC3 + 0x80..0xBF) that
        // the r16 clutch collapses into the single latin1 byte it encodes.
        for (name, latin1) in [("\u{80}", 0x80u8), ("\u{bf}", 0xBF), ("\u{c0}", 0xC0), ("\u{ff}", 0xFF)]
        {
            let mut expected: u64 = 0;
            expected = (expected << 4) + u64::from(latin1);
            assert_eq!(atom_hvalue(name), (expected & 0xFFFF_FFFF) as u32, "{name}");
        }
        // A lead byte outside 0xC2/0xC3 is hashed as its own raw bytes: U+0100 is
        // `0xC4 0x80`, so both bytes fold in separately.
        let mut expected: u64 = 0;
        for v in [0xC4u8, 0x80] {
            expected = (expected << 4) + u64::from(v);
            let g = expected & 0xf000_0000;
            if g != 0 {
                expected ^= g >> 24;
                expected ^= g;
            }
        }
        assert_eq!(atom_hvalue("\u{100}"), (expected & 0xFFFF_FFFF) as u32);
    }
}
