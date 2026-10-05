//! `erlang` binary/atom/integer conversions and term (de)serialization:
//! `list_to_binary/1`, `binary_to_list/1,3`, `byte_size/1`, `bit_size/1`,
//! `binary_part/2,3`, `integer_to_binary/1,2`, `binary_to_integer/1,2`,
//! `float_to_binary/1,2`, `binary_to_float/1`, `atom_to_binary/1,2`,
//! `binary_to_atom/1,2`, `binary_to_existing_atom/1,2`, `term_to_binary/1,2`
//! and `binary_to_term/1,2`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/bif.tab` (the roster),
//! `binary.c` (`binary_to_list/1,3`), `erl_bif_guard.c` (`byte_size/1` and the
//! `binary_part/2,3` guards), `erl_bif_binary.c` (`erts_binary_part`),
//! `erl_unicode.c` (`atom_to_binary/2`, `binary_to_atom/2`),
//! `../../erts/emulator/beam/external.c` (the term wire format) and
//! `../../erts/preloaded/src/erlang.erl` (the `atom_to_binary/1`,
//! `binary_to_atom/1`, `binary_to_existing_atom/1` and `binary_to_integer/1`
//! wrappers, which forward to the `/2` BIFs with `utf8`/`10`).
//!
//! The heap-bitstring helpers live in [`crate::bits`]; term (de)serialization
//! routes through `beam_codec`.
//!
//! # Pinned against the OTP 29 (erts-17.1) oracle
//!
//! * `byte_size/1` is `ceil(bit_size / 8)` — **not** `badarg` for a
//!   non-byte-aligned operand: `byte_size(<<1:1>>)` is `1` and
//!   `byte_size(<<1:17>>)` is `3` (`NBYTES(bitstring_size(x))`).
//!   `binary_to_list/1,3` *does* require a byte-aligned binary.
//! * `binary_to_list(Bin, Start, Stop)` is 1-based **and inclusive**, and
//!   requires `1 =< Start =< Stop =< byte_size(Bin)` — so
//!   `binary_to_list(<<1,2,3>>, 3, 2)` and `binary_to_list(<<>>, 1, 1)` are both
//!   `badarg` (an empty range is *not* accepted).
//! * `binary_part(Subject, Start, Len)` accepts a **negative** `Len`, which
//!   selects the `Len` bytes *ending* at `Start`
//!   (`binary_part(<<1,2,3,4,5>>, 4, -2)` is `<<3,4>>`); a byte-aligned subject
//!   is required, as is `Start + Len =< byte_size`.
//! * `list_to_binary/1` takes an **iolist** (a deep list of bytes/binaries, with
//!   a binary or `[]` as the improper tail) and rejects a bare binary
//!   (`list_to_binary(<<1,2>>)` is `badarg`, unlike `iolist_to_binary/1`).
//! * `integer_to_binary/2` renders uppercase digits `A`..`Z` for 10..35
//!   (`integer_to_binary(255, 16)` is `<<"FF">>`); `binary_to_integer/2` accepts
//!   both cases (`<<"ff">>` and `<<"Zz">>` are both `255`/`1295` in base 16/36) and
//!   an optional `+`/`-`. A base outside `2..=36` is `badarg`.
//! * `float_to_binary(Float)` is `float_to_binary(Float, [{scientific, 20}])`
//!   (`float_to_binary(1.5)` is `<<"1.50000000000000000000e+00">>`);
//!   `float_to_binary(Float, Options)` shares `float_to_list/2`'s option set
//!   (`{decimals, N}`, `{scientific, N}`, `compact`) and its renderings.
//! * `atom_to_binary/2` and `binary_to_atom/2` accept the encoding atoms
//!   `latin1`, `utf8` **and** `unicode` (`unicode` is an alias of `utf8` in
//!   `erl_unicode.c`); anything else is `badarg`. `latin1` fails on a code point
//!   above `U+00FF` (`atom_to_binary('π', latin1)` is `badarg`) and accepts every
//!   byte (`binary_to_atom(<<200>>, latin1)` is the atom `'È'`); `utf8` rejects an
//!   invalid sequence (`binary_to_atom(<<200>>, utf8)` is `badarg`). The empty
//!   binary is the empty atom `''`.
//! * `binary_to_atom/2` reports `system_limit` for a name longer than 255
//!   *characters*, while `binary_to_existing_atom/2` reports `badarg` for such a
//!   name (it cannot exist) — the `must_exist` split in `erl_unicode.c`.
//! * `term_to_binary/1` is the `beam_codec` image of the term;
//!   `term_to_binary/2` accepts `{minor_version, 0|1|2}` (`3` is `badarg`) and
//!   `compressed`/`{compressed, Level}`. `binary_to_term/1` ignores trailing
//!   bytes; `binary_to_term/2` takes `safe` and `used`
//!   (`binary_to_term(<<131,97,17>>, [used])` is `{17, 3}` — the count includes
//!   the `131` version byte). `safe` refuses a term that would create an atom.
//!
//! # Documented Milestone-1 gaps (never a silently wrong result)
//!
//! * **`float_to_binary/3` does not exist.** The objective's brief listed it, but
//!   erts-17.1 has no such function (`erlang:function_exported(erlang,
//!   float_to_binary, 3)` is `false`, and `bif.tab` has only `/1` and `/2`), so
//!   registering it would *invent* a BIF arity the oracle answers with `undef`.
//!   Following the same rule as `io:format`'s `~d` (see [`crate::io`]), the
//!   oracle wins over the illustrative list: only `/1` and `/2` are registered.
//! * **The `compressed` option is unsupported** (`term_to_binary/2` with
//!   `compressed` or `{compressed, Level}` raises `error:unsupported_bif`): the
//!   Stage-0 `beam_codec` has no zlib. The *option* is validated like the oracle
//!   (its value must be a small integer) and then reported.
//! * **`{minor_version, 0|1}` is accepted but has no effect.** The codec has no
//!   legacy-tag switch, so `term_to_binary(T, [{minor_version, 1}])` emits the
//!   minor-version-2 tag set (`SMALL_ATOM_UTF8_EXT` where the oracle emits
//!   `ATOM_EXT`). The image still decodes to the identical term — it is a
//!   byte-level, not a semantic, deviation.
//! * **`beam_codec` cannot encode or decode a binary** (`BINARY_EXT`/
//!   `BIT_BINARY_EXT`): `term_to_binary(Term)` for a `Term` that *contains* a
//!   binary, and `binary_to_term(Bin)` for an image that contains one, report the
//!   codec's explicit `unsupported_external_term` error (detectable with
//!   `beam_codec::error::is_unsupported`). This is the one place where a BIF in
//!   this module rejects an input the oracle accepts. Every other BIF here is
//!   total for the Milestone-1 term set, which is exactly why
//!   `list_to_binary/binary_to_list/binary_part/...` build their results on the
//!   heap (see [`crate::bits`]) rather than through the codec.
//! * **`beam_codec` encodes only *external* pids/ports/refs.** An internal pid
//!   (`self()`) is not representable, so `term_to_binary(self())` reports the same
//!   `unsupported` error instead of the oracle's `NEW_PID_EXT`. A decoded
//!   `NEW_PID_EXT` stays an external id, so `binary_to_term` ∘ `term_to_binary`
//!   round-trips the wire term, not an internal pid.
//! * **No bignums.** `integer_to_binary/1,2` and `binary_to_integer/1,2` cover the
//!   60-bit small range; an out-of-range value reports
//!   [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) where the
//!   oracle promotes to a bignum (the same documented gap as
//!   [`crate::convert`]'s `integer_to_list`/`list_to_integer`).
//! * **`float_to_binary/2` option `short` is unsupported** (`badarg`), exactly as
//!   in [`crate::convert`]'s `float_to_list/2`: it selects Ryu's shortest
//!   round-trip rendering, which the Stage-0 formatter does not implement.
//! * **`binary_to_atom`/`binary_to_existing_atom` and `binary_to_term` need the
//!   [`AtomInterner`](crate::context::AtomInterner) hook to *create* an atom.**
//!   The context only borrows the atom table immutably, so a name that is not
//!   already interned reports
//!   [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when no
//!   hook is installed (`beam_rt` always installs one) — never a fabricated
//!   index.
//! * **Sub-binaries are copied.** C makes `binary_part/2,3` a sub-binary
//!   reference; Milestone-1 keeps every binary on the heap and copies the range,
//!   so the value is identical but no shared storage is created (the placement
//!   deviation [`crate::bits`] documents).
//!
//! # The atom-index coupling of `binary_to_term`
//!
//! `beam_codec::Decoder::decode` needs a **mutable** [`AtomTable`] to intern the
//! atoms it meets, while [`BifContext::atoms`] only hands out `&AtomTable`. This
//! module therefore decodes into a **clone of the context's table** and then hands
//! every name the decoder appended to the runtime through
//! [`BifContext::intern_atom`]. That is sound because `AtomTable` assigns indices
//! in insertion order: the clone has the context's indices for every existing
//! name, and the names the decoder appends are the ones the runtime then interns,
//! in the same order and therefore at the same indices — so the decoded term's atom
//! words already refer to the runtime's atoms. The precondition (the interner must
//! extend the very table `ctx.atoms()` borrows) is the one
//! [`crate::context`] documents for the hook.

use beam_codec::{encode_term, Decoder};
use beam_common::Result;
use beam_term::{atom_val, signed_value, Atom, AtomTable, Eterm};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, system_limit, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// The maximum atom-name length in *characters* (erts-17.1, `ATOM_MAX_CHARS`).
const MAX_ATOM_CHARS: usize = 255;

/// `SYS_DEFAULT_FLOAT_DECIMALS` (`../../erts/emulator/beam/sys.h`): the mantissa
/// precision of the default `float_to_binary/1` form.
const DEFAULT_FLOAT_DECIMALS: i64 = 20;

/// The `char fbuf[256]` of `do_float_to_charbuf` (`bif.c`): a rendering is
/// `badarg` when it does not fit 255 characters plus the terminating NUL.
const FLOAT_BUF_CHARS: usize = 255;

/// `MAX_FLOAT` in `sys_double_to_chars_fast`: above this magnitude (and for
/// `decimals >= MAX_DECIMALS`) the fixed formatter falls back to `%.*f`.
const FIXED_FAST_MAX: f64 = 9_007_199_254_740_992.0; // 2^53

/// `MAX_DECIMALS` in `sys_double_to_chars_fast`: the number of entries of its
/// `pow10v` table.
const FIXED_FAST_DECIMALS: i64 = 19;

/// The largest `{minor_version, N}` the oracle accepts (`3` is `badarg`).
const MAX_MINOR_VERSION: i64 = 2;

// ---------------------------------------------------------------------------
// Shared argument helpers
// ---------------------------------------------------------------------------

/// The value of a small-integer argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `term` is not a
/// small integer (Milestone-1 has no bignums, so `is_small` is `is_integer`).
fn small_arg(term: Eterm) -> Result<i64> {
    if !access::is_small(term) {
        return Err(badarg());
    }
    Ok(signed_value(term))
}

/// The value of a non-negative integer argument (`term_to_Uint`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-integer or a
/// negative value.
fn uint_arg(term: Eterm) -> Result<usize> {
    usize::try_from(small_arg(term)?).map_err(|_| badarg())
}

/// The value of a `2..=36` conversion-base argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-integer or an
/// out-of-range base (the `base < 2 || base > 36` test in `bif.c`/`big.c`).
fn base_arg(term: Eterm) -> Result<u32> {
    let base = small_arg(term)?;
    if !(2..=36).contains(&base) {
        return Err(badarg());
    }
    Ok(base as u32)
}

/// The bytes of a byte-aligned bitstring argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for anything that is not
/// a binary (including a non-byte-aligned bitstring) — the oracle's
/// `is_not_bitstring`/`TAIL_BITS` rejections.
fn bytes_arg(term: Eterm) -> Result<Vec<u8>> {
    bits::bytes(term)
}

/// A binary of `bytes` built on the context heap.
///
/// # Errors
///
/// Propagates the heap's allocation failure.
fn make_bin(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
    bits::make_binary(ctx.heap(), bytes)
}

/// The byte list of `bytes`, built on the context heap.
///
/// # Errors
///
/// Propagates the heap's allocation failure.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        elems.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&elems)
}

/// The bytes of an iolist: an integer in `0..=255`, a binary, or a (possibly
/// improper) list of iolists whose tail is a binary or `[]`.
///
/// The walk is iterative (an explicit work stack) so a deeply nested iolist
/// cannot overflow the Rust stack — C's `erts_iolist_to_buf` is iterative for the
/// same reason.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an element that is
/// none of the three shapes (including a non-byte-aligned bitstring, an integer
/// outside `0..=255` and any improper tail that is not a binary).
fn flatten_iolist(root: Eterm, out: &mut Vec<u8>) -> Result<()> {
    let mut stack = vec![root];
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            let byte = u8::try_from(signed_value(term)).map_err(|_| badarg())?;
            out.push(byte);
            continue;
        }
        if bits::is_binary(term) {
            out.extend_from_slice(&bits::bytes(term)?);
            continue;
        }
        if access::is_list(term) {
            // The tail of a list is a *tail* position: for `list_to_binary/1` (as in
            // C's `erts_iolist_to_buf`) it may only be another list, `[]` or a binary
            // — `list_to_binary([1|2])` is `badarg`, `list_to_binary([1|<<2>>])` is not.
            let tail = access::cdr(term);
            if !access::is_list(tail) && !access::is_nil(tail) && !bits::is_binary(tail) {
                return Err(badarg());
            }
            // Push the tail first so the head is popped (and appended) first.
            stack.push(tail);
            stack.push(access::car(term));
            continue;
        }
        return Err(badarg());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// list_to_binary/1 and binary_to_list/1,3
// ---------------------------------------------------------------------------

/// `list_to_binary(IoList)`.
///
/// Unlike `iolist_to_binary/1`, a bare binary is *not* an iolist here
/// (`list_to_binary(<<1,2>>)` is `badarg` in the oracle too).
fn list_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_list(args[0]) && !access::is_nil(args[0]) {
        return Err(badarg());
    }
    let mut out = Vec::new();
    flatten_iolist(args[0], &mut out)?;
    make_bin(ctx, &out)
}

/// `binary_to_list(Binary)` — the bytes, in order.
fn binary_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let above = bytes_arg(args[0])?;
    byte_list(ctx, &above)
}

/// `binary_to_list(Binary, Start, Stop)` — 1-based and **inclusive**, requiring
/// `1 =< Start =< Stop =< byte_size(Binary)`.
fn binary_to_list_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let above = bytes_arg(args[0])?;
    let size = above.len();
    let start = uint_arg(args[1])?;
    let stop = uint_arg(args[2])?;
    if start < 1 || start > size || stop < 1 || stop > size || stop < start {
        return Err(badarg());
    }
    byte_list(ctx, &above[start - 1..stop])
}

// ---------------------------------------------------------------------------
// byte_size/1 and bit_size/1
// ---------------------------------------------------------------------------

/// `bit_size(Bitstring)` — the size in bits.
fn bit_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bits_size = bits::bit_size(args[0])?;
    let value = i64::try_from(bits_size).map_err(|_| system_limit())?;
    ctx.int(value)
}

/// `byte_size(Bitstring)` — `NBYTES(bit_size)`, i.e. the size rounded **up** to
/// whole bytes (`byte_size(<<1:1>>)` is `1`).
fn byte_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bits_size = bits::bit_size(args[0])?;
    let value = i64::try_from(bits_size.div_ceil(8)).map_err(|_| system_limit())?;
    ctx.int(value)
}

// ---------------------------------------------------------------------------
// binary_part/2,3
// ---------------------------------------------------------------------------

/// `binary_part(Subject, {Start, Length})`.
fn binary_part_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_tuple(args[1]) || access::tuple_size(args[1]) != 2 {
        return Err(badarg());
    }
    let start = access::tuple_element(args[1], 0);
    let length = access::tuple_element(args[1], 1);
    binary_part_range(ctx, args[0], start, length)
}

/// `binary_part(Subject, Start, Length)`.
fn binary_part_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    binary_part_range(ctx, args[0], args[1], args[2])
}

/// The shared `erts_binary_part` core.
///
/// A negative `Length` selects the `|Length|` bytes *ending* at `Start` (so
/// `binary_part(<<1,2,3,4,5>>, 4, -2)` is `<<3,4>>`), and `Start` itself is a
/// non-negative offset. The subject must be byte-aligned and the whole range must
/// lie inside it.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary
/// subject, a non-integer `Start`/`Length`, a `|Length|` larger than `Start`, or a
/// range that leaves the binary.
fn binary_part_range(
    ctx: &mut BifContext<'_>,
    subject: Eterm,
    start_term: Eterm,
    length_term: Eterm,
) -> Result<Eterm> {
    let data = bytes_arg(subject)?;
    let start = uint_arg(start_term)?;
    let length = small_arg(length_term)?;
    let size = data.len();
    let (start, length) = if length < 0 {
        let length = length.unsigned_abs() as usize;
        if length > start {
            return Err(badarg());
        }
        (start - length, length)
    } else {
        (start, length as usize)
    };
    let end = start.checked_add(length).ok_or_else(badarg)?;
    if end > size {
        return Err(badarg());
    }
    make_bin(ctx, &data[start..end])
}

// ---------------------------------------------------------------------------
// integer_to_binary/1,2 and binary_to_integer/1,2
// ---------------------------------------------------------------------------

/// Renders `value` in `base` the way `Sint_to_buf` does: uppercase digits
/// `A`..`Z` for 10..35 and a leading `-`.
fn int_to_digits(value: i64, base: u32) -> String {
    let mut magnitude = i128::from(value).unsigned_abs();
    let radix = u128::from(base);
    let mut digits = Vec::new();
    if magnitude == 0 {
        digits.push(b'0');
    }
    while magnitude > 0 {
        let digit = (magnitude % radix) as u8;
        digits.push(if digit < 10 {
            b'0' + digit
        } else {
            b'A' + (digit - 10)
        });
        magnitude /= radix;
    }
    let mut out = String::with_capacity(digits.len() + 1);
    if value < 0 {
        out.push('-');
    }
    for &digit in digits.iter().rev() {
        out.push(char::from(digit));
    }
    out
}

/// The value of the digit `byte` in `base`, or `None` when it is not a valid
/// digit (`c2int_is_valid_char`: `0`..`9`, plus `A`..`Z`/`a`..`z` when
/// `base > 10`).
fn digit_value(byte: u8, base: u32) -> Option<u32> {
    if base <= 10 {
        let value = u32::from(byte.wrapping_sub(b'0'));
        return (byte.is_ascii_digit() && value < base).then_some(value);
    }
    match byte {
        b'0'..=b'9' => Some(u32::from(byte - b'0')),
        b'A'..=b'Z' => {
            let value = 10 + u32::from(byte - b'A');
            (value < base).then_some(value)
        }
        b'a'..=b'z' => {
            let value = 10 + u32::from(byte - b'a');
            (value < base).then_some(value)
        }
        _ => None,
    }
}

/// Parses the whole of `text` as an integer in `base`, as
/// `erts_internal:binary_to_integer/2` does (an optional `+`/`-`, then digits).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a bad character or a
/// missing digit; [`BeamError::SystemLimit`] for a value beyond the `i64` range
/// (real Erlang promotes to a bignum, see the module docs).
fn parse_integer(text: &str, base: u32) -> Result<i64> {
    let bytes = text.as_bytes();
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return Err(badarg());
    }
    let ceiling: u128 = (i64::MAX as u128) + 1;
    let mut magnitude: u128 = 0;
    for &byte in digits {
        let digit = digit_value(byte, base).ok_or_else(badarg)?;
        magnitude = magnitude * u128::from(base) + u128::from(digit);
        if magnitude > ceiling {
            return Err(system_limit());
        }
    }
    let signed = if negative {
        -(magnitude as i128)
    } else {
        magnitude as i128
    };
    i64::try_from(signed).map_err(|_| system_limit())
}

/// `integer_to_binary(Integer)` — base 10.
fn integer_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = small_arg(args[0])?;
    let rendered = int_to_digits(value, 10);
    make_bin(ctx, rendered.as_bytes())
}

/// `integer_to_binary(Integer, Base)`.
fn integer_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = small_arg(args[0])?;
    let base = base_arg(args[1])?;
    let rendered = int_to_digits(value, base);
    make_bin(ctx, rendered.as_bytes())
}

/// `binary_to_integer(Binary)` — base 10.
fn binary_to_integer_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    integer_from_binary(ctx, args[0], 10)
}

/// `binary_to_integer(Binary, Base)`.
fn binary_to_integer_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let base = base_arg(args[1])?;
    integer_from_binary(ctx, args[0], base)
}

/// The shared `erts_internal:binary_to_integer/2` core.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary or a
/// string that is not a base-`base` integer; [`BeamError::SystemLimit`] for a
/// value outside the Milestone-1 small range.
fn integer_from_binary(ctx: &mut BifContext<'_>, input: Eterm, base: u32) -> Result<Eterm> {
    let data = bytes_arg(input)?;
    let text = core::str::from_utf8(&data).map_err(|_| badarg())?;
    let value = parse_integer(text, base)?;
    ctx.int(value)
}

// ---------------------------------------------------------------------------
// float_to_binary/1,2 and binary_to_float/1
// ---------------------------------------------------------------------------

/// The rendering strategy selected by a `float_to_binary/2` option list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FloatFormat {
    /// No `{decimals, _}`/`{scientific, _}` seen: the default scientific form.
    Legacy,
    /// `{scientific, Decimals}`.
    Scientific,
    /// `{decimals, Decimals}`.
    Fixed,
    /// `short` — Ryu's shortest round-trip rendering (unsupported in M1).
    Short,
}

/// Parses a `float_to_binary/2` option list the way `do_float_to_charbuf` does:
/// `compact`, `short`, `{decimals, N}`, `{scientific, N}`; the last format option
/// wins.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list, an
/// improper list or an option outside that set.
fn parse_float_options(atoms: &AtomTable, opts: Eterm) -> Result<(FloatFormat, i64, bool)> {
    let items = proper_list(opts).ok_or_else(badarg)?;
    let mut format = FloatFormat::Legacy;
    let mut decimals = DEFAULT_FLOAT_DECIMALS;
    let mut compact = false;
    for item in items {
        if access::is_atom(item) {
            match atoms.name(atom_val(item)) {
                Some("compact") => compact = true,
                Some("short") => format = FloatFormat::Short,
                _ => return Err(badarg()),
            }
            continue;
        }
        if access::is_tuple(item) && access::tuple_size(item) == 2 {
            let name = access::tuple_element(item, 0);
            let value = access::tuple_element(item, 1);
            if access::is_atom(name) && access::is_small(value) {
                match atoms.name(atom_val(name)) {
                    Some("decimals") => {
                        decimals = signed_value(value);
                        format = FloatFormat::Fixed;
                        continue;
                    }
                    Some("scientific") => {
                        decimals = signed_value(value);
                        format = FloatFormat::Scientific;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        return Err(badarg());
    }
    Ok((format, decimals, compact))
}

/// Renders `value` as `%.*e` with `decimals` mantissa digits, then rewrites the
/// exponent the way C does (`e+00`, never Rust's `e0`).
///
/// A negative precision is "omitted" for `printf`, so it selects the default of
/// 6 — `float_to_binary(1.5, [{scientific,-1}])` is `"1.500000e+00"` in the oracle
/// too.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the rendering does
/// not fit the oracle's 256-byte buffer.
fn format_scientific(value: f64, decimals: i64) -> Result<String> {
    let precision = if decimals < 0 { 6 } else { decimals };
    if precision > (FLOAT_BUF_CHARS - 6) as i64 {
        return Err(badarg());
    }
    let rendered = format!("{:.1$e}", value, precision as usize);
    let (mantissa, exponent) = rendered.split_once('e').ok_or_else(badarg)?;
    let exponent: i32 = exponent.parse().map_err(|_| badarg())?;
    let sign = if exponent < 0 { '-' } else { '+' };
    let out = format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs());
    if out.len() > FLOAT_BUF_CHARS {
        return Err(badarg());
    }
    Ok(out)
}

/// Trims a fixed rendering the way `find_first_trailing_zero` does: trailing
/// zeros go, but the digit right after the radix point stays.
fn trim_trailing_zeros(rendered: &mut String) {
    while rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.push('0');
    }
}

/// Renders `value` as `%.*f`, reproducing `sys_double_to_chars_fast`'s fast
/// path: for `|value| =< 2^53` and `decimals < 19` the fraction is scaled and
/// rounded *half away from zero*, which is why `float_to_binary(0.15,
/// [{decimals,1}])` is `"0.2"` (a plain correctly-rounded `%.1f` would say
/// `"0.1"`); larger magnitudes and precisions fall back to `%.*f`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a negative precision
/// (the C formatter reports `-1`) or a rendering beyond the buffer.
fn format_fixed(value: f64, decimals: i64, compact: bool) -> Result<String> {
    if decimals < 0 {
        return Err(badarg());
    }
    let precision = decimals as usize;
    if precision > FLOAT_BUF_CHARS - 2 {
        return Err(badarg());
    }
    let has_decimals = precision != 0;
    let negative = value.is_sign_negative();
    let magnitude = value.abs();

    let mut out = String::with_capacity(precision + 24);
    if negative {
        out.push('-');
    }
    if magnitude > FIXED_FAST_MAX || decimals >= FIXED_FAST_DECIMALS {
        out.push_str(&format!("{magnitude:.precision$}"));
    } else if has_decimals {
        let scale = 10f64.powi(precision as i32);
        let integer_f = magnitude.floor();
        let fraction_f = ((magnitude - integer_f) * scale).round();
        let mut integer = integer_f as u64;
        let mut fraction = fraction_f as u64;
        let limit = 10u64.pow(precision as u32);
        if fraction >= limit {
            // The rounded fraction carried into the integer part.
            integer += 1;
            fraction = 0;
        }
        out.push_str(&integer.to_string());
        out.push('.');
        out.push_str(&format!("{fraction:0width$}", width = precision));
    } else {
        out.push_str(&(magnitude.round() as u64).to_string());
    }
    if compact && has_decimals {
        trim_trailing_zeros(&mut out);
    }
    if out.len() > FLOAT_BUF_CHARS {
        return Err(badarg());
    }
    Ok(out)
}

/// Renders `value` for `float_to_binary/2`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for the unsupported
/// `short` format or a rendering the oracle's buffer cannot hold.
fn render_float(value: f64, format: FloatFormat, decimals: i64, compact: bool) -> Result<String> {
    match format {
        FloatFormat::Legacy | FloatFormat::Scientific => format_scientific(value, decimals),
        FloatFormat::Fixed => format_fixed(value, decimals, compact),
        FloatFormat::Short => Err(badarg()),
    }
}

/// `float_to_binary(Float)` — `[{scientific, 20}]`.
fn float_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_float(args[0]) {
        return Err(badarg());
    }
    let rendered = format_scientific(access::float_value(args[0]), DEFAULT_FLOAT_DECIMALS)?;
    make_bin(ctx, rendered.as_bytes())
}

/// `float_to_binary(Float, Options)`.
fn float_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_float(args[0]) {
        return Err(badarg());
    }
    let value = access::float_value(args[0]);
    let (format, decimals, compact) = parse_float_options(ctx.atoms(), args[1])?;
    let rendered = render_float(value, format, decimals, compact)?;
    make_bin(ctx, rendered.as_bytes())
}

/// Parses a float the way `sys_chars_to_double` does: an optional sign, digits,
/// a `.`/`,` radix, digits, an optional `e`/`E` exponent — and nothing else. The
/// value must be finite (`"1.0e309"` is `badarg`, an underflow to `0.0` is not).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for anything the
/// grammar rejects, a non-finite value, or (unreachable after the grammar check)
/// a rejected `f64` parse.
fn parse_float(text: &str) -> Result<f64> {
    let bytes = text.as_bytes();
    let mut at = 0;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        at += 1;
    }
    let integer_start = at;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }
    if at == integer_start || at >= bytes.len() || !matches!(bytes[at], b'.' | b',') {
        return Err(badarg());
    }
    at += 1;
    let fraction_start = at;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }
    if at == fraction_start {
        return Err(badarg());
    }
    if at < bytes.len() && matches!(bytes[at], b'e' | b'E') {
        at += 1;
        if at < bytes.len() && matches!(bytes[at], b'+' | b'-') {
            at += 1;
        }
        let exponent_start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        if at == exponent_start {
            return Err(badarg());
        }
    }
    if at != bytes.len() {
        return Err(badarg());
    }
    let normalized = text.replace(',', ".");
    let value: f64 = normalized.parse().map_err(|_| badarg())?;
    if !value.is_finite() {
        return Err(badarg());
    }
    Ok(value)
}

/// `binary_to_float(Binary)` — the same grammar as `list_to_float/1`, on a
/// binary.
fn binary_to_float_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let data = bytes_arg(args[0])?;
    let text = core::str::from_utf8(&data).map_err(|_| badarg())?;
    let value = parse_float(text)?;
    ctx.make_float(value)
}

// ---------------------------------------------------------------------------
// atom_to_binary/1,2, binary_to_atom/1,2 and binary_to_existing_atom/1,2
// ---------------------------------------------------------------------------

/// The text encoding selected by an `Encoding` argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TextEncoding {
    /// One byte per code point; only `U+0000..=U+00FF` is representable.
    Latin1,
    /// UTF-8.
    Utf8,
}

/// The `Encoding` argument: `latin1`, `utf8` or `unicode` (`unicode` is an alias
/// of `utf8` in `erl_unicode.c`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-atom or any
/// other atom.
fn encoding_arg(atoms: &AtomTable, term: Eterm) -> Result<TextEncoding> {
    if !access::is_atom(term) {
        return Err(badarg());
    }
    match atoms.name(atom_val(term)) {
        Some("latin1") => Ok(TextEncoding::Latin1),
        Some("utf8") | Some("unicode") => Ok(TextEncoding::Utf8),
        _ => Err(badarg()),
    }
}

/// Encodes `name` in `encoding`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `latin1` is asked
/// for a code point above `U+00FF` (`erts_utf8_to_latin1`'s failure).
fn encode_text(name: &str, encoding: TextEncoding) -> Result<Vec<u8>> {
    match encoding {
        TextEncoding::Utf8 => Ok(name.as_bytes().to_vec()),
        TextEncoding::Latin1 => {
            let mut out = Vec::with_capacity(name.len());
            for ch in name.chars() {
                let code = ch as u32;
                if code > 0xFF {
                    return Err(badarg());
                }
                out.push(code as u8);
            }
            Ok(out)
        }
    }
}

/// Decodes `bytes` in `encoding`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `utf8` is given an
/// invalid UTF-8 sequence.
fn decode_text(bytes: &[u8], encoding: TextEncoding) -> Result<String> {
    match encoding {
        // Latin-1 is one byte per code point, `U+0000..=U+00FF`.
        TextEncoding::Latin1 => Ok(bytes.iter().map(|&byte| char::from(byte)).collect()),
        TextEncoding::Utf8 => core::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| badarg()),
    }
}

/// `atom_to_binary(Atom)` — UTF-8.
fn atom_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    atom_to_bin(ctx, args[0], TextEncoding::Utf8)
}

/// `atom_to_binary(Atom, Encoding)`.
fn atom_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let encoding = encoding_arg(ctx.atoms(), args[1])?;
    atom_to_bin(ctx, args[0], encoding)
}

/// The shared `atom_to_binary/2` core.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-atom or a
/// `latin1` encoding the name cannot be written in.
fn atom_to_bin(ctx: &mut BifContext<'_>, term: Eterm, encoding: TextEncoding) -> Result<Eterm> {
    if !access::is_atom(term) {
        return Err(badarg());
    }
    let name = ctx
        .atoms()
        .name(atom_val(term))
        .ok_or_else(badarg)?
        .to_string();
    let bytes = encode_text(&name, encoding)?;
    make_bin(ctx, &bytes)
}

/// `binary_to_atom(Binary)` — UTF-8.
fn binary_to_atom_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    atom_from_binary(ctx, args[0], TextEncoding::Utf8, false)
}

/// `binary_to_atom(Binary, Encoding)`.
fn binary_to_atom_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let encoding = encoding_arg(ctx.atoms(), args[1])?;
    atom_from_binary(ctx, args[0], encoding, false)
}

/// `binary_to_existing_atom(Binary)` — UTF-8, never creates.
fn binary_to_existing_atom_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    atom_from_binary(ctx, args[0], TextEncoding::Utf8, true)
}

/// `binary_to_existing_atom(Binary, Encoding)` — never creates.
fn binary_to_existing_atom_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let encoding = encoding_arg(ctx.atoms(), args[1])?;
    atom_from_binary(ctx, args[0], encoding, true)
}

/// The shared `binary_to_atom/2` core (`must_exist` is the
/// `binary_to_existing_atom/2` split in `erl_unicode.c`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary, a bad
/// encoding of the bytes, or (when `must_exist`) a name that is not interned
/// (which includes a name longer than 255 characters: it cannot exist);
/// [`BeamError::SystemLimit`] for a *new* name longer than 255 characters;
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the name
/// is new and the runtime installed no interner.
fn atom_from_binary(
    ctx: &mut BifContext<'_>,
    input: Eterm,
    encoding: TextEncoding,
    must_exist: bool,
) -> Result<Eterm> {
    let data = bytes_arg(input)?;
    let name = decode_text(&data, encoding)?;
    if must_exist {
        return ctx.atom(&name).map_err(|_| badarg());
    }
    if name.chars().count() > MAX_ATOM_CHARS {
        return Err(system_limit());
    }
    ctx.intern_atom(&name)
}

// ---------------------------------------------------------------------------
// term_to_binary/1,2 and binary_to_term/1,2
// ---------------------------------------------------------------------------

/// `term_to_binary(Term)` — the `beam_codec` external term image.
fn term_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = encode_term(args[0], ctx.atoms())?;
    make_bin(ctx, &bytes)
}

/// `term_to_binary(Term, Options)`.
///
/// The option list is validated exactly like the oracle; the `compressed` family
/// is then reported as unsupported (the codec has no zlib) and an accepted
/// `{minor_version, 0|1}` has no effect on the emitted tags — both documented in
/// the module docs.
fn term_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    check_term_to_binary_options(ctx.atoms(), args[1])?;
    term_to_binary_1(ctx, &args[..1])
}

/// Validates a `term_to_binary/2` option list.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list or an
/// unknown option, a `{minor_version, N}` outside `0..=2`, or a non-integer
/// compression level; `error:unsupported_bif` for the recognised-but-unimplemented
/// `compressed`/`{compressed, Level}` (see the module docs).
fn check_term_to_binary_options(atoms: &AtomTable, opts: Eterm) -> Result<()> {
    for item in proper_list(opts).ok_or_else(badarg)? {
        if access::is_atom(item) {
            if atoms.name(atom_val(item)) == Some("compressed") {
                return Err(unsupported_bif());
            }
            return Err(badarg());
        }
        if !access::is_tuple(item) || access::tuple_size(item) != 2 {
            return Err(badarg());
        }
        let name = access::tuple_element(item, 0);
        let value = access::tuple_element(item, 1);
        if !access::is_atom(name) {
            return Err(badarg());
        }
        match atoms.name(atom_val(name)) {
            Some("minor_version") => {
                let version = small_arg(value)?;
                if !(0..=MAX_MINOR_VERSION).contains(&version) {
                    return Err(badarg());
                }
            }
            Some("compressed") => {
                // The level is still validated (the oracle reports `badarg` for a
                // non-integer level), then the option itself is reported.
                small_arg(value)?;
                return Err(unsupported_bif());
            }
            _ => return Err(badarg()),
        }
    }
    Ok(())
}

/// `binary_to_term(Binary)` — trailing bytes are ignored.
fn binary_to_term_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    decode_term_bif(ctx, args[0], false, false)
}

/// `binary_to_term(Binary, Options)` — the `safe` and `used` options.
fn binary_to_term_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut safe = false;
    let mut used = false;
    for item in proper_list(args[1]).ok_or_else(badarg)? {
        if !access::is_atom(item) {
            return Err(badarg());
        }
        match ctx.atoms().name(atom_val(item)) {
            Some("safe") => safe = true,
            Some("used") => used = true,
            _ => return Err(badarg()),
        }
    }
    decode_term_bif(ctx, args[0], safe, used)
}

/// Decodes an external term image (see the module docs for the atom-index
/// coupling this relies on).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary, a
/// malformed/truncated image, an unknown option, or `safe` with an image that
/// would create an atom; the codec's `unsupported_external_term` raise for a tag
/// Milestone-1 cannot build (in particular a binary).
fn decode_term_bif(
    ctx: &mut BifContext<'_>,
    input: Eterm,
    safe: bool,
    used: bool,
) -> Result<Eterm> {
    let data = bytes_arg(input)?;
    // Decode into a *clone* of the context's table: the decoder needs `&mut`, and
    // the clone has the context's indices (the names are interned in order).
    let mut local = ctx.atoms().clone();
    let known = local.len();
    let (term, position) = {
        let heap = ctx.heap();
        let mut decoder = Decoder::new(&data)?;
        let decoded = decoder.decode(heap, &mut local)?;
        (decoded, decoder.position())
    };
    if local.len() > known {
        if safe {
            // `safe` forbids creating atoms; nothing has been created yet.
            return Err(badarg());
        }
        let fresh: Vec<String> = (known..local.len())
            .map(|index| {
                local
                    .name(Atom::from_raw(index as u32))
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        for name in &fresh {
            ctx.intern_atom(name)?;
        }
    }
    if used {
        let count = ctx.int(i64::try_from(position).map_err(|_| system_limit())?)?;
        ctx.tuple(&[term, count])
    } else {
        Ok(term)
    }
}

/// Registers the `erlang` binary/atom conversion and term (de)serialization BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "list_to_binary", 1, list_to_binary_1);
    reg.register("erlang", "binary_to_list", 1, binary_to_list_1);
    reg.register("erlang", "binary_to_list", 3, binary_to_list_3);
    reg.register("erlang", "byte_size", 1, byte_size_1);
    reg.register("erlang", "bit_size", 1, bit_size_1);
    reg.register("erlang", "binary_part", 2, binary_part_2);
    reg.register("erlang", "binary_part", 3, binary_part_3);
    reg.register("erlang", "integer_to_binary", 1, integer_to_binary_1);
    reg.register("erlang", "integer_to_binary", 2, integer_to_binary_2);
    reg.register("erlang", "binary_to_integer", 1, binary_to_integer_1);
    reg.register("erlang", "binary_to_integer", 2, binary_to_integer_2);
    reg.register("erlang", "float_to_binary", 1, float_to_binary_1);
    reg.register("erlang", "float_to_binary", 2, float_to_binary_2);
    reg.register("erlang", "binary_to_float", 1, binary_to_float_1);
    reg.register("erlang", "atom_to_binary", 1, atom_to_binary_1);
    reg.register("erlang", "atom_to_binary", 2, atom_to_binary_2);
    reg.register("erlang", "binary_to_atom", 1, binary_to_atom_1);
    reg.register("erlang", "binary_to_atom", 2, binary_to_atom_2);
    reg.register("erlang", "binary_to_existing_atom", 1, binary_to_existing_atom_1);
    reg.register("erlang", "binary_to_existing_atom", 2, binary_to_existing_atom_2);
    reg.register("erlang", "term_to_binary", 1, term_to_binary_1);
    reg.register("erlang", "term_to_binary", 2, term_to_binary_2);
    reg.register("erlang", "binary_to_term", 1, binary_to_term_1);
    reg.register("erlang", "binary_to_term", 2, binary_to_term_2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_codec::error::is_unsupported;
    use beam_codec::term::external_pid_parts;
    use beam_process::ProcessId;
    use beam_term::AtomTable;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The names every test table is seeded with, **in this order** — a clone then
    /// has identical indices, which is what [`decode_term_bif`] relies on, and the
    /// option atoms (`latin1`, `minor_version`, …) must be present for the BIFs to
    /// recognise them.
    const NAMES: &[&str] = &[
        "true",
        "false",
        "ok",
        "a",
        "b",
        "hello",
        "abc",
        "\u{c8}",
        "nonode@nohost",
        "latin1",
        "utf8",
        "unicode",
        "compressed",
        "minor_version",
        "safe",
        "used",
        "decimals",
        "scientific",
        "compact",
        "short",
        "",
        "\u{3c0}",
    ];

    /// `self()` — `NEW_PID_EXT` on `nonode@nohost`, id 10, serial/creation 0 (the
    /// vector `beam_codec/tests/ground_truth.rs` pins).
    const PID_SELF: &[u8] = &[
        131, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, 0, 0, 0,
        10, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    /// `<<"hi">>` — `BINARY_EXT`, the tag the codec cannot build (M1 gap).
    const BINARY_HI: &[u8] = &[131, 109, 0, 0, 0, 2, 104, 105];

    fn table() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(NAMES.iter().copied());
        atoms
    }

    fn heap() -> Heap {
        Heap::new(8192).expect("test heap")
    }

    /// Runs `f` over a fresh context (no seams installed).
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = heap();
        let atoms = table();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// An [`AtomInterner`](crate::context::AtomInterner) over a *clone* of the
    /// context table, so a name it interns lands at exactly the index the context's
    /// own table would have assigned it.
    struct Interner {
        table: Rc<RefCell<AtomTable>>,
    }

    impl crate::context::AtomInterner for Interner {
        fn intern(&mut self, name: &str) -> Atom {
            self.table.borrow_mut().intern(name)
        }
    }

    /// Runs `f` over a fresh context with the atom interner installed, handing back
    /// the table the interner owns (used to read back a freshly interned name).
    fn with_interner<R>(f: impl FnOnce(&mut BifContext<'_>, &Rc<RefCell<AtomTable>>) -> R) -> R {
        let mut heap = heap();
        let atoms = table();
        let shared = Rc::new(RefCell::new(atoms.clone()));
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        f(&mut ctx, &shared)
    }

    // A per-thread scratch heap used *only* to build argument terms. Keeping it
    // separate from the context's own heap lets `bin(..)`/`bitstring(..)` be called
    // inside an expression that already borrows the context (`ctx.list(&[bin(..)])`)
    // — a single `&mut BifContext` would forbid that double borrow. Each `#[test]`
    // runs on its own thread, so the heap is per-test and never shared across tests.
    thread_local! {
        static SCRATCH: RefCell<Heap> = RefCell::new(Heap::new(1 << 20).expect("scratch heap"));
    }

    /// A binary term of `bytes`, allocated on the per-thread scratch heap.
    fn bin(bytes: &[u8]) -> Eterm {
        SCRATCH.with(|h| bits::make_binary(&mut h.borrow_mut(), bytes).expect("the binary fits"))
    }

    /// A (possibly non-byte-aligned) bitstring term on the per-thread scratch heap.
    fn bitstring(data: &[u8], bits: usize) -> Eterm {
        SCRATCH.with(|h| bits::make_bitstring(&mut h.borrow_mut(), data, bits).expect("fits"))
    }

    /// The bytes of a binary term.
    fn text(term: Eterm) -> Vec<u8> {
        bits::bytes(term).expect("a byte-aligned binary")
    }

    /// The small integers of a proper list.
    fn values(term: Eterm) -> Vec<i64> {
        proper_list(term)
            .expect("a proper list")
            .into_iter()
            .map(signed_value)
            .collect()
    }

    /// The small integer a BIF returned.
    fn int_of(term: Eterm) -> i64 {
        assert!(access::is_small(term), "not a small integer");
        signed_value(term)
    }

    /// The float value a BIF returned.
    fn float_of(term: Eterm) -> f64 {
        assert!(access::is_float(term), "not a float");
        access::float_value(term)
    }

    /// Asserts `result` is `badarg`.
    fn assert_badarg(result: Result<Eterm>) {
        assert!(
            matches!(result, Err(beam_common::BeamError::BadArg)),
            "expected badarg, got {result:?}"
        );
    }

    // -- list_to_binary / binary_to_list ----------------------------------

    #[test]
    fn list_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            let digits = ctx
                .list(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                ])
                .expect("fits");
            assert_eq!(text(list_to_binary_1(ctx, &[digits]).unwrap()), vec![1, 2, 3]);

            // An iolist: <<"ab">>, <<"cd">>, [101,[102]], 1 and the improper tail <<2>>.
            let nested = ctx.list(&[ctx.int(102).expect("fits")]).expect("fits");
            let inner = ctx
                .list(&[ctx.int(101).expect("fits"), nested])
                .expect("fits");
            let iolist = ctx
                .list(&[
                    bin(b"ab"),
                    bin(b"cd"),
                    inner,
                    ctx.int(1).expect("fits"),
                ])
                .expect("fits");
            let iolist = ctx.cons(iolist, bin(&[2])).expect("fits");
            assert_eq!(
                text(list_to_binary_1(ctx, &[iolist]).unwrap()),
                vec![97, 98, 99, 100, 101, 102, 1, 2]
            );

            // `[]` is the empty binary.
            let empty = ctx.list(&[]).expect("nil");
            assert!(text(list_to_binary_1(ctx, &[empty]).unwrap()).is_empty());

            // A bare binary is *not* an iolist for `list_to_binary/1`.
            for (i, bad) in [
                bin(&[1, 2]),
                ctx.int(1).expect("fits"),
                ctx.atom("a").expect("seeded"),
                ctx.list(&[ctx.int(256).expect("fits")]).expect("fits"),
                ctx.list(&[ctx.int(-1).expect("fits")]).expect("fits"),
                ctx.list(&[ctx.atom("a").expect("seeded")]).expect("fits"),
                // An improper integer tail.
                ctx.cons(ctx.int(1).expect("fits"), ctx.int(2).expect("fits"))
                    .expect("fits"),
                // A non-byte-aligned bitstring element.
                ctx.list(&[bitstring(&[0x80], 1)])
                    .expect("fits"),
            ]
            .into_iter()
            .enumerate()
            {
                assert!(
                    matches!(
                        list_to_binary_1(ctx, &[bad]),
                        Err(beam_common::BeamError::BadArg)
                    ),
                    "case {i} should be badarg"
                );
            }
        });
    }

    #[test]
    fn binary_to_list_matches_the_oracle() {
        with_ctx(|ctx| {
            let three = bin(&[1, 2, 3]);
            assert_eq!(values(binary_to_list_1(ctx, &[three]).unwrap()), vec![1, 2, 3]);
            // The empty binary is `[]` (`NIL`).
            let empty = bin(&[]);
            assert_eq!(binary_to_list_1(ctx, &[empty]).unwrap(), beam_term::NIL);

            // `binary_to_list/3` is 1-based and inclusive.
            for (start, stop, expected) in [
                (2, 3, vec![2, 3]),
                (1, 1, vec![1]),
                (3, 3, vec![3]),
                (2, 2, vec![2]),
                (1, 3, vec![1, 2, 3]),
            ] {
                let args = [
                    three,
                    ctx.int(start).expect("fits"),
                    ctx.int(stop).expect("fits"),
                ];
                assert_eq!(
                    values(binary_to_list_3(ctx, &args).unwrap()),
                    expected,
                    "binary_to_list(<<1,2,3>>, {start}, {stop})"
                );
            }
            // Out of range, inverted, non-integer and non-binary are `badarg`.
            for (start, stop) in [(4i64, 3i64), (3, 2), (0, 1), (1, 4)] {
                let args = [
                    three,
                    ctx.int(start).expect("fits"),
                    ctx.int(stop).expect("fits"),
                ];
                assert_badarg(binary_to_list_3(ctx, &args));
            }
            let float_start = [
                three,
                ctx.make_float(1.0).expect("fits"),
                ctx.int(3).expect("fits"),
            ];
            assert_badarg(binary_to_list_3(ctx, &float_start));
            // The empty binary accepts no range at all.
            let empty_args = [
                empty,
                ctx.int(1).expect("fits"),
                ctx.int(0).expect("fits"),
            ];
            assert_badarg(binary_to_list_3(ctx, &empty_args));

            // A non-byte-aligned bitstring and a non-bitstring are `badarg`.
            let odd = bits::make_bitstring(ctx.heap(), &[0x80], 1).expect("fits");
            for bad in [odd, ctx.int(1).expect("fits"), ctx.list(&[]).expect("nil")] {
                assert_badarg(binary_to_list_1(ctx, &[bad]));
            }
        });
    }

    // -- byte_size / bit_size ---------------------------------------------

    #[test]
    fn byte_and_bit_size_match_the_oracle() {
        with_ctx(|ctx| {
            for (bytes, expected) in [
                (&[1u8, 2, 3][..], 3i64),
                (&[][..], 0),
            ] {
                let term = bin(bytes);
                assert_eq!(int_of(bit_size_1(ctx, &[term]).unwrap()), expected * 8);
                assert_eq!(int_of(byte_size_1(ctx, &[term]).unwrap()), expected);
            }
            // `byte_size/1` rounds *up*: 1, 9, 16 and 17 bits are 1, 2, 2 and 3 bytes.
            for (bit_len, expected) in [(1usize, 1i64), (9, 2), (16, 2), (17, 3), (0, 0)] {
                let data = vec![0xFFu8; bit_len.div_ceil(8).max(1)];
                let term = bits::make_bitstring(ctx.heap(), &data, bit_len).expect("fits");
                assert_eq!(
                    int_of(bit_size_1(ctx, &[term]).unwrap()),
                    bit_len as i64,
                    "bit_size(<<..:{bit_len}>>)"
                );
                assert_eq!(
                    int_of(byte_size_1(ctx, &[term]).unwrap()),
                    expected,
                    "byte_size(<<..:{bit_len}>>)"
                );
            }
            // A non-bitstring is `badarg`.
            for bad in [
                ctx.int(1).expect("fits"),
                ctx.atom("a").expect("seeded"),
                ctx.list(&[ctx.int(1).expect("fits")]).expect("fits"),
            ] {
                assert_badarg(bit_size_1(ctx, &[bad]));
                assert_badarg(byte_size_1(ctx, &[bad]));
            }
        });
    }

    // -- binary_part ------------------------------------------------------

    #[test]
    fn binary_part_matches_the_oracle() {
        with_ctx(|ctx| {
            let five = bin(&[1, 2, 3, 4, 5]);
            // `binary_part/3`: `Start` non-negative, `Len` signed.
            for (start, len, expected) in [
                (1i64, 2i64, vec![2, 3]),
                (5, 0, vec![]),
                (4, -2, vec![3, 4]),
                (2, -1, vec![2]),
                (0, 0, vec![]),
                (0, 5, vec![1, 2, 3, 4, 5]),
                (1, -1, vec![1]),
            ] {
                let args = [
                    five,
                    ctx.int(start).expect("fits"),
                    ctx.int(len).expect("fits"),
                ];
                assert_eq!(
                    text(binary_part_3(ctx, &args).unwrap()),
                    expected,
                    "binary_part(<<1,2,3,4,5>>, {start}, {len})"
                );
            }
            // `binary_part/2` destructures a 2-tuple the same way.
            let pair = ctx
                .tuple(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
                .expect("fits");
            assert_eq!(text(binary_part_2(ctx, &[five, pair]).unwrap()), vec![2, 3]);

            // Out of range, inverted-length and non-integer are `badarg`.
            for (start, len) in [
                (6i64, 0i64),
                (5, 1),
                (-1, 1),
                (0, -1),
                (1, -2),
                (-1, -1),
                (0, 6),
            ] {
                let args = [
                    five,
                    ctx.int(start).expect("fits"),
                    ctx.int(len).expect("fits"),
                ];
                assert_badarg(binary_part_3(ctx, &args));
            }
            let float_len = [
                five,
                ctx.int(1).expect("fits"),
                ctx.make_float(2.0).expect("fits"),
            ];
            assert_badarg(binary_part_3(ctx, &float_len));

            // The 2-tuple form requires exactly two elements.
            let triple = ctx
                .tuple(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                ])
                .expect("fits");
            for bad in [triple, ctx.int(1).expect("fits")] {
                assert_badarg(binary_part_2(ctx, &[five, bad]));
            }

            // The subject must be a byte-aligned binary.
            let odd = bits::make_bitstring(ctx.heap(), &[0x80], 1).expect("fits");
            let nil = ctx.list(&[]).expect("nil");
            let zero = ctx.int(0).expect("fits");
            assert_badarg(binary_part_3(ctx, &[odd, zero, zero]));
            assert_badarg(binary_part_3(ctx, &[nil, zero, zero]));
        });
    }

    // -- integer conversions ----------------------------------------------

    #[test]
    fn integer_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            for (value, expected) in [
                (255i64, "255"),
                (-17, "-17"),
                (0, "0"),
                (17, "17"),
            ] {
                let term = integer_to_binary_1(ctx, &[ctx.int(value).expect("fits")]).unwrap();
                assert_eq!(text(term), expected.as_bytes());
            }
            for (value, base, expected) in [
                (255i64, 16i64, "FF"),
                (255, 2, "11111111"),
                (255, 8, "377"),
                (-255, 36, "-73"),
                (0, 10, "0"),
                (35, 36, "Z"),
            ] {
                let args = [ctx.int(value).expect("fits"), ctx.int(base).expect("fits")];
                let term = integer_to_binary_2(ctx, &args).unwrap();
                assert_eq!(text(term), expected.as_bytes(), "base {base}");
            }
            for base in [1i64, 37, 0, -2] {
                let args = [ctx.int(5).expect("fits"), ctx.int(base).expect("fits")];
                assert_badarg(integer_to_binary_2(ctx, &args));
            }
            let float_base = [ctx.int(5).expect("fits"), ctx.make_float(16.0).expect("fits")];
            assert_badarg(integer_to_binary_2(ctx, &float_base));
            let float = ctx.make_float(5.0).expect("fits");
            assert_badarg(integer_to_binary_1(ctx, &[float]));
        });
    }

    #[test]
    fn binary_to_integer_matches_the_oracle() {
        with_ctx(|ctx| {
            for (input, expected) in [
                (&b"123"[..], 123i64),
                (b"-99", -99),
                (b"+33", 33),
                (b"00123", 123),
                (b"0", 0),
            ] {
                let term = binary_to_integer_1(ctx, &[bin(input)]).unwrap();
                assert_eq!(int_of(term), expected, "binary_to_integer({input:?})");
            }
            for (input, base, expected) in [
                (&b"3FF"[..], 16i64, 1023i64),
                (b"ff", 16, 255),
                (b"Zz", 36, 1295),
                (b"zz", 36, 1295),
                (b"101", 2, 5),
            ] {
                let args = [bin(input), ctx.int(base).expect("fits")];
                let term = binary_to_integer_2(ctx, &args).unwrap();
                assert_eq!(int_of(term), expected, "binary_to_integer({input:?}, {base})");
            }
            for (input, base) in [
                (&b""[..], 10i64),
                (b" ", 10),
                (b"-", 10),
                (b"12.0", 10),
                (b"2", 2),
                (b"1:1", 10),
                (b"\x01\x02", 10),
            ] {
                let args = [bin(input), ctx.int(base).expect("fits")];
                assert_badarg(binary_to_integer_2(ctx, &args));
                if base == 10 {
                    assert_badarg(binary_to_integer_1(ctx, &[bin(input)]));
                }
            }
            for base in [1i64, 37, 0] {
                let args = [bin(b"10"), ctx.int(base).expect("fits")];
                assert_badarg(binary_to_integer_2(ctx, &args));
            }
            let atom_base = [bin(b"10"), ctx.atom("a").expect("seeded")];
            assert_badarg(binary_to_integer_2(ctx, &atom_base));
            // A non-binary is `badarg`.
            let nil = ctx.list(&[]).expect("nil");
            assert_badarg(binary_to_integer_1(ctx, &[nil]));
            // Beyond the 60-bit small range (real Erlang promotes to a bignum).
            assert!(matches!(
                binary_to_integer_1(ctx, &[bin(b"12345678901234567890")]),
                Err(beam_common::BeamError::SystemLimit)
            ));
        });
    }

    // -- float conversions ------------------------------------------------

    #[test]
    fn float_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            let cases: &[(f64, &[u8])] = &[
                (1.5, b"1.50000000000000000000e+00"),
                (1.0, b"1.00000000000000000000e+00"),
                (1.0e10, b"1.00000000000000000000e+10"),
                (-0.0, b"-0.00000000000000000000e+00"),
            ];
            for (value, expected) in cases {
                let number = ctx.make_float(*value).expect("fits");
                let term = float_to_binary_1(ctx, &[number]).unwrap();
                assert_eq!(&text(term), expected, "float_to_binary({value})");
            }
            // `/2` renderings, pinned against the oracle.
            for (value, opts, expected) in [
                (7.12, "[{decimals,4}]", &b"7.1200"[..]),
                (7.12, "[{decimals,4},compact]", b"7.12"),
                (7.12, "[{scientific,3}]", b"7.120e+00"),
                (7.12, "[]", b"7.12000000000000010658e+00"),
                (1.5, "[{decimals,2}]", b"1.50"),
                (1.5, "[{scientific,0}]", b"2e+00"),
                (1.5, "[{decimals,25}]", b"1.5000000000000000000000000"),
                (1.0, "[{decimals,2},compact]", b"1.0"),
                (1.0e10, "[{decimals,2}]", b"10000000000.00"),
                (2.5, "[{decimals,0}]", b"3"),
            ] {
                let options = match opts {
                    "[{decimals,4}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(4).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[{decimals,4},compact]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(4).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t, ctx.atom("compact").expect("seeded")])
                            .expect("fits")
                    }
                    "[{scientific,3}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("scientific").expect("seeded"),
                                ctx.int(3).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[]" => ctx.list(&[]).expect("nil"),
                    "[{decimals,2}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(2).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[{scientific,0}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("scientific").expect("seeded"),
                                ctx.int(0).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[{decimals,25}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(25).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[{decimals,2},compact]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(2).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t, ctx.atom("compact").expect("seeded")])
                            .expect("fits")
                    }
                    "[{decimals,0}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(0).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    other => panic!("unhandled option literal {other}"),
                };
                let args = [ctx.make_float(value).expect("fits"), options];
                let term = float_to_binary_2(ctx, &args).unwrap();
                assert_eq!(&text(term), expected, "float_to_binary({value}, {opts})");
            }

            // Bad arguments and options.
            let one = ctx.int(1).expect("fits");
            assert_badarg(float_to_binary_1(ctx, &[one]));
            let float = ctx.make_float(1.5).expect("fits");
            let negative = ctx
                .tuple(&[
                    ctx.atom("decimals").expect("seeded"),
                    ctx.int(-1).expect("fits"),
                ])
                .expect("fits");
            let negative = ctx.list(&[negative]).expect("fits");
            assert_badarg(float_to_binary_2(ctx, &[float, negative]));
            let unknown = ctx
                .tuple(&[
                    ctx.atom("a").expect("seeded"),
                    ctx.int(2).expect("fits"),
                ])
                .expect("fits");
            let both = ctx
                .list(&[ctx.atom("a").expect("seeded"), unknown])
                .expect("fits");
            assert_badarg(float_to_binary_2(ctx, &[float, both]));
            assert_badarg(float_to_binary_2(ctx, &[float, ctx.atom("a").expect("seeded")]));
            // `short` selects Ryu's shortest rendering, which M1 does not implement
            // (documented gap).
            let short = ctx.list(&[ctx.atom("short").expect("seeded")]).expect("fits");
            assert_badarg(float_to_binary_2(ctx, &[float, short]));
        });
    }

    #[test]
    fn binary_to_float_matches_the_oracle() {
        with_ctx(|ctx| {
            for (input, expected) in [
                (&b"1.5"[..], 1.5f64),
                (b"1.5e3", 1500.0),
                (b"1,5", 1.5),
                (b"-1.5", -1.5),
            ] {
                let term = binary_to_float_1(ctx, &[bin(input)]).unwrap();
                assert_eq!(float_of(term), expected, "binary_to_float({input:?})");
            }
            for bad in [
                &b"1"[..],
                b"1.",
                b".5",
                b"",
                b"1.5 ",
                b"1.0e309",
                b" 1.5",
                b"\xff\xff",
            ] {
                assert_badarg(binary_to_float_1(ctx, &[bin(bad)]));
            }
            let nil = ctx.list(&[]).expect("nil");
            assert_badarg(binary_to_float_1(ctx, &[nil]));
            let odd = bits::make_bitstring(ctx.heap(), &[0x80], 1).expect("fits");
            assert_badarg(binary_to_float_1(ctx, &[odd]));
        });
    }

    // -- atom conversions -------------------------------------------------

    #[test]
    fn atom_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            let cases: &[(&str, &str, &[u8])] = &[
                ("a", "utf8", b"a"),
                ("abc", "latin1", b"abc"),
                ("\u{c8}", "latin1", &[200]),
                ("", "utf8", b""),
                ("\u{3c0}", "utf8", &[207, 128]),
                ("\u{3c0}", "unicode", &[207, 128]),
            ];
            for (name, encoding, expected) in cases {
                let atom = ctx.atom(name).expect("interned");
                let enc = ctx.atom(encoding).expect("seeded");
                let term = atom_to_binary_2(ctx, &[atom, enc]).unwrap();
                assert_eq!(text(term), expected.to_vec(), "atom_to_binary({name}, {encoding})");
            }
            // `/1` is UTF-8.
            let term = atom_to_binary_1(ctx, &[ctx.atom("a").expect("seeded")]).unwrap();
            assert_eq!(text(term), b"a".to_vec());

            // `latin1` cannot represent a code point above `U+00FF`.
            let enc = ctx.atom("latin1").expect("seeded");
            let pi = ctx.atom("\u{3c0}").expect("interned");
            assert_badarg(atom_to_binary_2(ctx, &[pi, enc]));
            // A non-atom first argument or an unknown encoding is `badarg`.
            let latin1 = ctx.atom("latin1").expect("seeded");
            assert_badarg(atom_to_binary_1(ctx, &[ctx.int(1).expect("fits")]));
            assert_badarg(atom_to_binary_2(ctx, &[ctx.int(1).expect("fits"), latin1]));
            let a = ctx.atom("a").expect("seeded");
            assert_badarg(atom_to_binary_2(ctx, &[a, ctx.atom("hello").expect("seeded")]));
            assert_badarg(atom_to_binary_2(ctx, &[a, ctx.int(1).expect("fits")]));
        });
    }

    #[test]
    fn binary_to_atom_matches_the_oracle() {
        with_interner(|ctx, interned| {
            // An existing name resolves through the table.
            let term = binary_to_atom_1(ctx, &[bin(b"abc")]).unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some("abc"));
            let term = binary_to_atom_2(
                ctx,
                &[bin(b"abc"), ctx.atom("latin1").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some("abc"));

            // `latin1` maps each byte to the matching code point; `utf8` requires a
            // valid sequence.
            let term = binary_to_atom_2(
                ctx,
                &[bin(&[200]), ctx.atom("latin1").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some("\u{c8}"));
            assert_badarg(binary_to_atom_2(
                ctx,
                &[bin(&[200]), ctx.atom("utf8").expect("seeded")],
            ));
            assert_badarg(binary_to_atom_2(
                ctx,
                &[bin(&[255, 255]), ctx.atom("utf8").expect("seeded")],
            ));

            // The empty binary is the empty atom, and `unicode` aliases `utf8`.
            let term = binary_to_atom_2(
                ctx,
                &[bin(&[]), ctx.atom("unicode").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some(""));
            let term = binary_to_atom_2(
                ctx,
                &[bin(b"abc"), ctx.atom("unicode").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some("abc"));

            // A new name is created through the interner, at the same index the
            // cloned table assigned it (the coupling `decode_term_bif` relies on).
            let term = binary_to_atom_1(ctx, &[bin(b"brand_new_atom")]).unwrap();
            assert_eq!(
                interned.borrow().name(atom_val(term)),
                Some("brand_new_atom")
            );
            // A 256-character name is `system_limit`; 255 fit.
            let long = vec![b'a'; 255];
            assert!(binary_to_atom_1(ctx, &[bin(&long)]).is_ok());
            let too_long = vec![b'a'; 256];
            assert!(matches!(
                binary_to_atom_1(ctx, &[bin(&too_long)]),
                Err(beam_common::BeamError::SystemLimit)
            ));
            // `binary_to_existing_atom` never creates: `badarg` for a new name (and
            // for an over-long one, which cannot exist either).
            assert_badarg(binary_to_existing_atom_1(ctx, &[bin(b"brand_new_atom2")]));
            assert_badarg(binary_to_existing_atom_1(ctx, &[bin(&too_long)]));
            let hit = binary_to_existing_atom_1(ctx, &[bin(b"abc")]).unwrap();
            assert_eq!(ctx.atoms().name(atom_val(hit)), Some("abc"));
            let hit = binary_to_existing_atom_2(
                ctx,
                &[bin(b"abc"), ctx.atom("unicode").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(hit)), Some("abc"));

            // A non-binary or an unknown encoding is `badarg`.
            let utf8 = ctx.atom("utf8").expect("seeded");
            assert_badarg(binary_to_atom_1(ctx, &[ctx.int(1).expect("fits")]));
            assert_badarg(binary_to_atom_2(ctx, &[bin(b"abc"), ctx.atom("a").expect("seeded")]));
            assert_badarg(binary_to_atom_2(ctx, &[ctx.int(1).expect("fits"), utf8]));
        });
        // Without an interner a *new* atom cannot be created (documented gap).
        with_ctx(|ctx| {
            assert!(matches!(
                binary_to_atom_1(ctx, &[bin(b"no_interner_here")]),
                Err(beam_common::BeamError::TermNotFound)
            ));
        });
    }

    // -- term_to_binary ---------------------------------------------------

    #[test]
    fn term_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let tuple = ctx.tuple(&[ctx.atom("a").expect("seeded"), one]).expect("fits");
            let list = ctx.list(&[one, two, ctx.int(3).expect("fits")]).expect("fits");
            let map = access::make_flatmap(ctx.heap(), &[(one, two)]).expect("fits");

            let cases: &[(Eterm, &[u8])] = &[
                (ctx.int(17).expect("fits"), &[131, 97, 17]),
                (
                    ctx.atom("hello").expect("seeded"),
                    &[131, 119, 5, 104, 101, 108, 108, 111],
                ),
                (
                    ctx.make_float(1.5).expect("fits"),
                    &[131, 70, 63, 248, 0, 0, 0, 0, 0, 0],
                ),
                (tuple, &[131, 104, 2, 119, 1, 97, 97, 1]),
                (list, &[131, 107, 0, 3, 1, 2, 3]),
                (map, &[131, 116, 0, 0, 0, 1, 97, 1, 97, 2]),
                (beam_term::NIL, &[131, 106]),
            ];
            for (term, expected) in cases {
                assert_eq!(&text(term_to_binary_1(ctx, &[*term]).unwrap()), expected);
            }

            // `{minor_version, 0|1|2}` is accepted (the tags stay at version 2 —
            // documented deviation); `3` and an unknown option are `badarg`.
            for version in [0i64, 1, 2] {
                let option = ctx
                    .tuple(&[ctx.atom("minor_version").expect("seeded"), ctx.int(version).expect("fits")])
                    .expect("fits");
                let options = ctx.list(&[option]).expect("fits");
                let args = [ctx.int(17).expect("fits"), options];
                assert_eq!(
                    text(term_to_binary_2(ctx, &args).unwrap()),
                    vec![131, 97, 17],
                    "{{minor_version, {version}}}"
                );
            }
            let bad_version = ctx
                .tuple(&[
                    ctx.atom("minor_version").expect("seeded"),
                    ctx.int(3).expect("fits"),
                ])
                .expect("fits");
            let bad_options = ctx.list(&[bad_version]).expect("fits");
            let seventeen = ctx.int(17).expect("fits");
            assert_badarg(term_to_binary_2(ctx, &[seventeen, bad_options]));
            let unknown = ctx.list(&[ctx.atom("a").expect("seeded")]).expect("fits");
            assert_badarg(term_to_binary_2(ctx, &[seventeen, unknown]));
            assert_badarg(term_to_binary_2(
                ctx,
                &[seventeen, ctx.atom("a").expect("seeded")],
            ));

            // `compressed` is recognised but unimplemented (documented gap).
            let compressed = ctx.list(&[ctx.atom("compressed").expect("seeded")]).expect("fits");
            match term_to_binary_2(ctx, &[seventeen, compressed]) {
                Err(beam_common::BeamError::Raise(raise)) => assert_eq!(
                    raise.reason,
                    beam_common::ExitReason::atom("unsupported_bif")
                ),
                other => panic!("expected unsupported_bif, got {other:?}"),
            }
            let level = ctx
                .tuple(&[
                    ctx.atom("compressed").expect("seeded"),
                    ctx.int(6).expect("fits"),
                ])
                .expect("fits");
            let level = ctx.list(&[level]).expect("fits");
            assert!(matches!(
                term_to_binary_2(ctx, &[seventeen, level]),
                Err(beam_common::BeamError::Raise(_))
            ));
        });
    }

    // -- binary_to_term ---------------------------------------------------

    #[test]
    fn binary_to_term_matches_the_oracle() {
        with_ctx(|ctx| {
            // Every supported tag decodes, and the `used` count is the decoder's
            // position (including the `131` version byte).
            let images: &[(&[u8], u64)] = &[
                (&[131, 97, 17], 3),
                (&[131, 119, 5, 104, 101, 108, 108, 111], 8),
                (&[131, 70, 63, 240, 0, 0, 0, 0, 0, 0], 10),
                (&[131, 106], 2),
                (&[131, 116, 0, 0, 0, 0], 6),
                (&[131, 104, 2, 119, 1, 97, 97, 1], 8),
            ];
            for (image, used) in images {
                let term = binary_to_term_1(ctx, &[bin(image)]).unwrap();
                // The `used` form wraps the very same term.
                let options = ctx.list(&[ctx.atom("used").expect("seeded")]).expect("fits");
                let wrapped = binary_to_term_2(ctx, &[bin(image), options]).unwrap();
                assert!(access::is_tuple(wrapped));
                assert_eq!(access::tuple_size(wrapped), 2);
                // The two decodes allocate *separate* terms, so compare by
                // re-encoding: `compare::eq` deep-compares every M1 term class but a
                // map (see the module notes), and one of the images is a map.
                assert_eq!(
                    text(term_to_binary_1(ctx, &[access::tuple_element(wrapped, 0)]).unwrap()),
                    text(term_to_binary_1(ctx, &[term]).unwrap()),
                    "image {image:?}"
                );
                assert_eq!(int_of(access::tuple_element(wrapped, 1)), *used as i64);
            }

            // The decoded values are the expected terms.
            let term = binary_to_term_1(ctx, &[bin(&[131, 97, 17])]).unwrap();
            assert_eq!(int_of(term), 17);
            let term = binary_to_term_1(ctx, &[bin(&[131, 104, 2, 119, 1, 97, 97, 1])]).unwrap();
            assert!(access::is_tuple(term));
            assert_eq!(access::tuple_element(term, 0), ctx.atom("a").expect("seeded"));
            assert_eq!(int_of(access::tuple_element(term, 1)), 1);
            let term = binary_to_term_1(ctx, &[bin(&[131, 107, 0, 3, 1, 2, 3])]).unwrap();
            assert_eq!(values(term), vec![1, 2, 3]);
            let term = binary_to_term_1(ctx, &[bin(&[131, 70, 63, 240, 0, 0, 0, 0, 0, 0])]).unwrap();
            assert_eq!(float_of(term), 1.0);

            // Trailing bytes are ignored by `/1` (but counted by `used`).
            let trailing = bin(&[131, 97, 17, 99]);
            assert_eq!(int_of(binary_to_term_1(ctx, &[trailing]).unwrap()), 17);
            let options = ctx.list(&[ctx.atom("used").expect("seeded")]).expect("fits");
            let wrapped = binary_to_term_2(ctx, &[trailing, options]).unwrap();
            assert_eq!(int_of(access::tuple_element(wrapped, 1)), 3);

            // A malformed/truncated/non-binary image is `badarg`.
            for bad in [
                &[][..],
                &[131][..],
                &[131, 110][..],
                &[0][..],
                &[131, 0][..],
                &[130, 97, 17][..],
            ] {
                assert_badarg(binary_to_term_1(ctx, &[bin(bad)]));
            }
            assert_badarg(binary_to_term_1(ctx, &[ctx.int(1).expect("fits")]));
            let nil = ctx.list(&[]).expect("nil");
            assert_badarg(binary_to_term_1(ctx, &[nil]));
            let odd = bits::make_bitstring(ctx.heap(), &[0x80], 1).expect("fits");
            assert_badarg(binary_to_term_1(ctx, &[odd]));

            // An unknown option (or a non-list) is `badarg`; `safe` accepts a term
            // whose atoms already exist.
            let unknown = ctx.list(&[ctx.atom("a").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(&[131, 97, 17]), unknown]));
            assert_badarg(binary_to_term_2(
                ctx,
                &[bin(&[131, 97, 17]), ctx.atom("used").expect("seeded")],
            ));
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            let term = binary_to_term_2(ctx, &[bin(&[131, 106]), safe]).unwrap();
            assert_eq!(term, beam_term::NIL);
        });
    }

    #[test]
    fn binary_to_term_safe_rejects_new_atoms_and_interns_them_otherwise() {
        // `<<131,119,8,"tjenixen">>` — the oracle's `[safe]` example: a *new* atom.
        const NEW_ATOM: &[u8] = &[131, 119, 8, 116, 106, 101, 110, 105, 120, 101, 110];
        // Without `safe` the atom is created through the interner, at the index the
        // cloned table assigned it.
        with_interner(|ctx, interned| {
            let term = binary_to_term_1(ctx, &[bin(NEW_ATOM)]).unwrap();
            assert!(access::is_atom(term));
            assert_eq!(interned.borrow().name(atom_val(term)), Some("tjenixen"));
            // The `safe` form consults the runtime table (`ctx.atoms()`), which this
            // harness keeps distinct from the interner's clone — so the name the
            // interner just created is still unknown here and the decode is refused.
            // In the runtime the interner mutates that very table, so the atom then
            // exists and `safe` accepts it (the oracle's behaviour).
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(NEW_ATOM), safe]));
            assert!(interned.borrow().lookup("tjenixen").is_some());
        });
        // With `safe` and an unknown atom the decode is `badarg` — and *creates*
        // nothing.
        with_interner(|ctx, interned| {
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(NEW_ATOM), safe]));
            assert!(interned.borrow().lookup("tjenixen").is_none());
        });
    }

    #[test]
    fn binary_to_term_round_trips_pids_and_maps() {
        with_ctx(|ctx| {
            // An external pid (what the wire carries) round-trips byte-exactly.
            let term = binary_to_term_1(ctx, &[bin(PID_SELF)]).unwrap();
            assert!(access::is_pid(term));
            let (node, number, serial, creation) = external_pid_parts(term).expect("an external pid");
            assert_eq!(ctx.atoms().name(atom_val(node)), Some("nonode@nohost"));
            assert_eq!((number, serial, creation), (10, 0, 0));
            assert_eq!(text(term_to_binary_1(ctx, &[term]).unwrap()), PID_SELF.to_vec());

            // A map round-trips (structurally).
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let map = access::make_flatmap(ctx.heap(), &[(one, two)]).expect("fits");
            let image = term_to_binary_1(ctx, &[map]).unwrap();
            let back = binary_to_term_1(ctx, &[image]).unwrap();
            assert!(access::is_map(back));
            // A structural `compare::eq` on maps is not available in M1, so the
            // round-trip is pinned byte-exactly instead.
            assert_eq!(text(term_to_binary_1(ctx, &[back]).unwrap()), text(image.clone()));
        });
    }

    #[test]
    fn binary_terms_are_the_documented_codec_gap() {
        with_ctx(|ctx| {
            // Decoding a `BINARY_EXT` image is the codec's explicit "unsupported".
            let err = binary_to_term_1(ctx, &[bin(BINARY_HI)]).unwrap_err();
            assert!(is_unsupported(&err), "expected unsupported, got {err:?}");

            // Encoding a term that *contains* a binary reports the same, never a
            // wrong image.
            let nested = ctx.tuple(&[bin(b"hi")]).expect("fits");
            let err = term_to_binary_1(ctx, &[nested]).unwrap_err();
            assert!(is_unsupported(&err), "expected unsupported, got {err:?}");

            // An internal pid is not encodable either (the codec has only the
            // external id forms).
            let self_pid = ctx.self_pid().to_eterm();
            let err = term_to_binary_1(ctx, &[self_pid]).unwrap_err();
            assert!(is_unsupported(&err), "expected unsupported, got {err:?}");
        });
    }

    // -- registration -----------------------------------------------------

    #[test]
    fn register_installs_every_bif() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("list_to_binary", 1),
            ("binary_to_list", 1),
            ("binary_to_list", 3),
            ("byte_size", 1),
            ("bit_size", 1),
            ("binary_part", 2),
            ("binary_part", 3),
            ("integer_to_binary", 1),
            ("integer_to_binary", 2),
            ("binary_to_integer", 1),
            ("binary_to_integer", 2),
            ("float_to_binary", 1),
            ("float_to_binary", 2),
            ("binary_to_float", 1),
            ("atom_to_binary", 1),
            ("atom_to_binary", 2),
            ("binary_to_atom", 1),
            ("binary_to_atom", 2),
            ("binary_to_existing_atom", 1),
            ("binary_to_existing_atom", 2),
            ("term_to_binary", 1),
            ("term_to_binary", 2),
            ("binary_to_term", 1),
            ("binary_to_term", 2),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 24);
        // `float_to_binary/3` is deliberately absent: erts-17.1 has no such BIF.
        assert!(!reg.contains("erlang", "float_to_binary", 3));
    }
}
