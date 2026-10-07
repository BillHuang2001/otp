//! Term-conversion and inspection BIFs for `erlang` and `binary`:
//! `bitstring_to_list/1`, `delete_element/2`, `insert_element/3`,
//! `iolist_to_iovec/1`, `is_integer/3`, `display/1`, `display_string/2` and
//! `binary:referenced_byte_size/1`.
//!
//! Ground truth (read-only): the entries in `../../erts/emulator/beam/bif.tab`
//! (`bif erlang:bitstring_to_list/1` :493, `ubif erlang:is_integer/3` :845,
//! `bif erlang:insert_element/3` :612, `bif erlang:delete_element/2` :613,
//! `bif erlang:iolist_to_iovec/1` :686, `bif erlang:display/1` :60,
//! `bif erlang:display_string/2` :61, `bif binary:referenced_byte_size/1` :561)
//! and their implementations — `binary.c`'s `bitstring_to_list_1`,
//! `bif.c`'s `insert_element_3`/`delete_element_2`/`display_1`/`display_string_2`,
//! `erl_bif_guard.c`'s `is_integer_3`, `erl_iolist.c`'s `iolist_to_iovec_1` and
//! `erl_bif_binary.c`'s `binary_referenced_byte_size_1`.
//!
//! # Roster
//!
//! Every BIF above is registered; each one is pinned against the OTP 29 /
//! erts-17.1 oracle (the probe is quoted at the implementation and in
//! `tests/term_conv.rs`).
//!
//! Two behaviours the oracle has are **not** reproduced, because this runtime has
//! no seam for them; both are a typed `error:unsupported_bif`, never a wrong
//! result (see [`display_string_2`]):
//!
//! * `display_string/2`'s `stderr`/`stdin` streams — the only output seam is the
//!   string-valued [`OutputSink`](crate::context::OutputSink) `io:format` uses, and
//!   the oracle writes those two streams to their own file descriptors;
//! * `display_string/2` of a binary payload that is not valid UTF-8 — the sink
//!   takes a `&str`, so the raw bytes cannot be handed through it.
//!
//! # Deliberately out of scope
//!
//! `erlang:pid_to_list/1`, `erlang:list_to_pid/1`, `erlang:ref_to_list/1`,
//! `erlang:list_to_ref/1` and `erlang:external_size/1` are **not** registered: the
//! first four need the decoded external pid/ref term shape and `external_size`
//! needs the ETF encoder's size probe. They stay unregistered (`undef`) rather than
//! approximated.

use beam_common::Result;
use beam_codec::term as codec_term;
use beam_term::prelude::signed_value;
use beam_term::{atom_val, Eterm, ERL_ONHEAP_BITS_LIMIT, NIL};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::{flatten, term_chunk, UNLIMITED_DEPTH};
use crate::registry::BifRegistry;

// ---------------------------------------------------------------------------
// bitstring_to_list/1
// ---------------------------------------------------------------------------

/// `bitstring_to_list(Bitstring)` (`binary.c:413`) — the bitstring's whole bytes
/// as small integers, plus its trailing `bit_size rem 8` bits as one final
/// bitstring when the size is not a whole number of bytes.
///
/// Pinned to the oracle:
/// `bitstring_to_list(<<1,2,3>>)` ⇒ `[1,2,3]`,
/// `bitstring_to_list(<<1,2,3:4>>)` ⇒ `[1,2,<<3:4>>]`,
/// `bitstring_to_list(<<1:9>>)` ⇒ `[0,<<1:1>>]`, `bitstring_to_list(<<>>)` ⇒ `[]`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the argument is not a
/// bitstring (`bitstring_to_list([1,2])`, `bitstring_to_list(5)`).
fn bitstring_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if !bits::is_bitstring(term) {
        return Err(badarg());
    }
    let bit_len = bits::bit_size(term)?;
    if bit_len == 0 {
        // `binary.c:429`: a zero-sized bitstring is the empty list.
        return Ok(NIL);
    }
    // `BYTE_SIZE(size)` whole bytes, then `TAIL_BITS(size)` bits of a sub-bitstring.
    let whole = bit_len / 8;
    let tail_bits = bit_len % 8;
    let packed = access::bitstring_bits(term)?;
    let mut elems: Vec<Eterm> = Vec::with_capacity(whole + 1);
    for byte in &packed[..whole] {
        elems.push(ctx.int(i64::from(*byte))?);
    }
    if tail_bits != 0 {
        // `packed` holds `ceil(bit_len / 8)` bytes, so `packed[whole..]` is exactly
        // the trailing partial byte.
        elems.push(bits::make_bitstring(ctx.heap(), &packed[whole..], tail_bits)?);
    }
    ctx.list(&elems)
}

// ---------------------------------------------------------------------------
// delete_element/2 and insert_element/3
// ---------------------------------------------------------------------------

/// The 0-based slot of the `1`-based element index `index` in a tuple of arity
/// `arity`, where the index may address one **past** the last element
/// (`for_insert`: the C's `arity + 1` bound).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) unless `index` is a small
/// integer in `1..=arity (+ 1)`.
fn element_slot(index: Eterm, arity: usize, for_insert: bool) -> Result<usize> {
    if !access::is_small(index) {
        return Err(badarg());
    }
    let ix = signed_value(index);
    let max = arity as i64 + i64::from(for_insert);
    if ix < 1 || ix > max {
        return Err(badarg());
    }
    Ok(ix as usize - 1)
}

/// The arity of `delete_element/2`'s and `insert_element/3`'s tuple argument, or
/// `badarg` (`bif.c:3118`, `bif.c:3156`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `term` is not a tuple.
fn element_tuple(term: Eterm) -> Result<usize> {
    if !access::is_tuple(term) {
        return Err(badarg());
    }
    Ok(access::tuple_size(term))
}

/// `delete_element(Index, Tuple)` (`bif.c:3148`) — a new tuple without element
/// `Index`.
///
/// Pinned to the oracle: `delete_element(1, {a,b,c})` ⇒ `{b,c}`,
/// `delete_element(3, {a,b,c})` ⇒ `{a,b}`, `delete_element(1, {a})` ⇒ `{}`. An
/// out-of-range index (`0` or `4` on `{a,b,c}`, any index on `{}`), a non-tuple and
/// a non-small-integer index are `badarg`.
fn delete_element_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (index, tuple) = (args[0], args[1]);
    let arity = element_tuple(tuple)?;
    let slot = element_slot(index, arity, false)?;
    let elems: Vec<Eterm> = (0..arity)
        .filter(|&i| i != slot)
        .map(|i| access::tuple_element(tuple, i))
        .collect();
    ctx.tuple(&elems)
}

/// `insert_element(Index, Tuple, Value)` (`bif.c:3110`) — a new tuple of arity
/// `arity + 1` with `Value` at element `Index` (so `Index` may be `arity + 1`).
///
/// Pinned to the oracle: `insert_element(1, {a,b}, x)` ⇒ `{x,a,b}`,
/// `insert_element(3, {a,b}, x)` ⇒ `{a,b,x}`, `insert_element(1, {}, x)` ⇒ `{x}`.
/// An out-of-range index (`0` or `4` on `{a,b}`), a non-tuple and a
/// non-small-integer index are `badarg`; `Value` is any term (never type-checked).
fn insert_element_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (index, tuple, value) = (args[0], args[1], args[2]);
    let arity = element_tuple(tuple)?;
    let slot = element_slot(index, arity, true)?;
    let mut elems: Vec<Eterm> = Vec::with_capacity(arity + 1);
    for i in 0..arity {
        if i == slot {
            elems.push(value);
        }
        elems.push(access::tuple_element(tuple, i));
    }
    if slot == arity {
        elems.push(value);
    }
    ctx.tuple(&elems)
}

// ---------------------------------------------------------------------------
// binary:referenced_byte_size/1
// ---------------------------------------------------------------------------

/// `binary:referenced_byte_size(Binary)` (`erl_bif_binary.c:2691`) — the size of
/// the whole binary the argument references.
///
/// Every binary this runtime builds is on-heap and whole (`binary:part/3` copies;
/// there is no off-heap `BinRef`/`SubBits` producer), so the size is the
/// argument's own **byte size** — the C's `br == NULL` branch, `BYTE_SIZE(size)`,
/// which truncates a partial trailing byte exactly like `size/1`. Verified against
/// the oracle for a runtime-built on-heap binary
/// (`binary:referenced_byte_size(list_to_binary([1,2,3]))` ⇒ `3`) and for a large
/// one off the 64-byte on-heap limit
/// (`binary:referenced_byte_size(binary:copy(<<1>>, 100000))` ⇒ `100000`).
///
/// Two documented divergences, both properties of terms this runtime cannot
/// produce rather than wrong answers for one it can:
///
/// * a **sub-binary** — the oracle reports the size of the whole binary it points
///   into (`binary:referenced_byte_size(binary:part(binary:copy(<<1>>,100),10,5))`
///   ⇒ `100`), which M1 does not model (`binary:part/3` copies, so that argument is
///   an ordinary 5-byte binary here);
/// * a **literal-area** binary — the `<<1,2,3>>` of a compiled module reports the
///   literal chunk's allocated size in the oracle (`256`); this runtime materializes
///   no such literal.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the argument is not a
/// bitstring (`binary:referenced_byte_size([1,2])`).
fn referenced_byte_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if !bits::is_bitstring(term) {
        return Err(badarg());
    }
    ctx.int((bits::bit_size(term)? / 8) as i64)
}

// ---------------------------------------------------------------------------
// iolist_to_iovec/1
// ---------------------------------------------------------------------------

/// `erl_iolist.c:37`: `IOL2V_COPY_THRESHOLD (ERL_ONHEAP_BITS_LIMIT * 8)` — the
/// largest component (in bits) that is copied into the common output buffer.
const IOL2V_COPY_THRESHOLD_BITS: usize = ERL_ONHEAP_BITS_LIMIT * 8;

/// Turns the pending copied byte run into one binary element
/// (`generic_iolist_copy_vec_enqueue`), leaving nothing pending.
///
/// # Errors
///
/// Propagates a heap-allocation failure.
fn flush_pending(
    ctx: &mut BifContext<'_>,
    elems: &mut Vec<Eterm>,
    pending: &mut Vec<u8>,
) -> Result<()> {
    if !pending.is_empty() {
        elems.push(bits::make_binary(ctx.heap(), pending)?);
        pending.clear();
    }
    Ok(())
}

/// `iolist_to_iovec(IoList)` (`erl_iolist.c:1265`) — the iolist as a list of
/// binaries, merging every run of copied components into one binary and passing a
/// large byte-aligned component through **as-is**.
///
/// The result shape is pinned to the oracle:
///
/// * `iolist_to_iovec([1,2,<<"ab">>,[3,[<<"c">>]],<<"de">>])` ⇒
///   `[<<1,2,97,98,3,99,100,101>>]` (every copied component merges into one);
/// * `iolist_to_iovec([])`, `iolist_to_iovec(<<>>)`, `iolist_to_iovec([<<>>])` and
///   `iolist_to_iovec([<<>>,<<>>])` ⇒ `[]` (an empty result is the empty list, not
///   `[<<>>]`);
/// * `iolist_to_iovec(<<"abc">>)` ⇒ `[<<"abc">>]` and `iolist_to_iovec([1|<<2>>])`
///   ⇒ `[<<1,2>>]` (a bare byte-aligned binary and a bitstring **cons tail** are
///   both accepted);
/// * a component larger than [`IOL2V_COPY_THRESHOLD_BITS`] that is byte aligned is
///   *borrowed*, so it keeps its own slot:
///   `iolist_to_iovec([binary:copy(<<7>>,513), binary:copy(<<7>>,513)])` ⇒ two
///   513-byte binaries while the 512-byte form merges into one 1024-byte binary
///   (the C's `IOL_SHOULD_COPY`), and `iolist_to_iovec([B513,1,2])` ⇒
///   `[B513,<<1,2>>]`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an element that is
/// neither a byte (`0..=255`), a byte-aligned bitstring, nor a nested list — and
/// for a non-list, non-nil, non-bitstring argument (`iolist_to_iovec(5)`). Note
/// that *every* bitstring leaf must be byte aligned
/// (`iolist_to_iovec([<<1:4>>,<<2:4>>])` ⇒ `badarg`, even though the two halves
/// would total a whole byte), unlike `iolist_to_binary/1`'s final-partial-byte
/// rule.
fn iolist_to_iovec_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let root = args[0];
    // The C's fast path: a bare bitstring (`erl_iolist.c:1272`).
    if bits::is_bitstring(root) {
        let size_bits = bits::bit_size(root)?;
        if size_bits % 8 != 0 {
            return Err(badarg());
        }
        return if size_bits == 0 { Ok(NIL) } else { ctx.list(&[root]) };
    }
    if !access::is_nil(root) && !access::is_list(root) {
        return Err(badarg());
    }

    let mut elems: Vec<Eterm> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    // An explicit work stack, so a deeply nested iolist cannot overflow the Rust
    // stack; the loop is bounded by the finite input term.
    let mut stack = vec![root];
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            let byte = signed_value(term);
            if !(0..=255).contains(&byte) {
                return Err(badarg());
            }
            pending.push(byte as u8);
            continue;
        }
        if bits::is_bitstring(term) {
            let size_bits = bits::bit_size(term)?;
            if size_bits % 8 != 0 {
                return Err(badarg());
            }
            // `IOL_SHOULD_COPY(Threshold, Unit, Offset, Size)` is false — i.e. the
            // component is enqueued *unmerged* — exactly when it is byte aligned and
            // larger than the threshold (`erl_iolist.c:39`).
            let (_, offset, _) = codec_term::bitstring_bytes(term)?;
            if size_bits > IOL2V_COPY_THRESHOLD_BITS && offset % 8 == 0 {
                flush_pending(ctx, &mut elems, &mut pending)?;
                elems.push(term);
            } else {
                // The copied run is byte aligned, so its MSB-first packing is its
                // byte image (the C's `generic_iolist_copy_bitstring` run).
                let packed = access::bitstring_bits(term)?;
                pending.extend_from_slice(&packed[..size_bits / 8]);
            }
            continue;
        }
        if access::is_list(term) {
            let tail = access::cdr(term);
            if !access::is_nil(tail) && !access::is_list(tail) && !bits::is_bitstring(tail) {
                return Err(badarg());
            }
            // Push the tail first so the head is processed next (LIFO).
            stack.push(tail);
            stack.push(access::car(term));
            continue;
        }
        return Err(badarg());
    }
    flush_pending(ctx, &mut elems, &mut pending)?;
    ctx.list(&elems)
}

// ---------------------------------------------------------------------------
// is_integer/3
// ---------------------------------------------------------------------------

/// `is_integer(Term, Lower, Upper)` (`erl_bif_guard.c:337`) — the OTP-29 guard:
/// true iff `Term` is an integer in `Lower..=Upper`.
///
/// Pinned to the oracle: both bounds are validated **first** and must be integers,
/// so `is_integer(5,1.0,10)`, `is_integer(5,1,10.0)` and `is_integer(5,1,inf)` are
/// `badarg`; a non-integer `Term` is `false`, never an error
/// (`is_integer(a,1,10)` ⇒ `false`); the range need not be ordered
/// (`is_integer(5,10,1)` ⇒ `false`). The comparison is the C's arithmetic
/// `CMP_LE`, so either bound may be a bignum.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Lower` or `Upper` is
/// not an integer.
fn is_integer_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (term, lower, upper) = (args[0], args[1], args[2]);
    if !access::is_integer(lower) || !access::is_integer(upper) {
        return Err(badarg());
    }
    if !access::is_integer(term) {
        return ctx.boolean(false);
    }
    // Every operand is an integer, so the arithmetic order is the numeric one.
    let atoms = ctx.atoms();
    let not_greater = core::cmp::Ordering::Greater;
    let at_least_lower = beam_term::compare::cmp_arith(lower, term, atoms) != not_greater;
    let at_most_upper = beam_term::compare::cmp_arith(term, upper, atoms) != not_greater;
    ctx.boolean(at_least_lower && at_most_upper)
}

// ---------------------------------------------------------------------------
// display/1 and display_string/2
// ---------------------------------------------------------------------------

/// `display(Term)` (`bif.c:4148`) — the term as `~p` (the printable string rule,
/// no depth limit, **no line wrapping**), followed by a line terminator, written
/// through the [`OutputSink`](crate::context::OutputSink) `io:format/1,2` uses;
/// returns `true`.
///
/// The terminator is `\r\n`, not `\n`: the C is `erts_printf("%.*T\n", …)` and
/// `erts_printf` writes through `write_f_add_cr` on Unix
/// (`erts/lib_src/common/erl_printf.c:135`, whose flag
/// `erts_printf_add_cr_to_stdout` is set by `erts/emulator/sys/unix/sys.c:342`),
/// which turns every `\n` into `\r\n`. Pinned to the oracle — the bytes of
/// `erlang:display(<<"a b">>)`, `erlang:display([97,98])` and
/// `erlang:display("ström")` are `<<"a b">>\r\n`, `"ab"\r\n` and `"ström"\r\n`
/// (with the raw latin-1 byte `0xF6`), and the return value is `true`.
///
/// # Errors
///
/// Propagates the sink's I/O failure, and the leaf renderer's typed error for a
/// term outside the Milestone-1 printable set (never a truncated line).
fn display_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let text = flatten(&[term_chunk(ctx, args[0], true, UNLIMITED_DEPTH)?]);
    ctx.write_output(&text)?;
    ctx.write_output("\r\n")?;
    ctx.boolean(true)
}

/// The UTF-8 string of `erts_unicode_list_to_buf`'s charlist argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the list is improper,
/// has a non-integer element, or holds a code point that is not a Unicode scalar
/// value (`[16#D800]` — a surrogate — and `[16#110000]` are both `badarg` in the
/// oracle).
fn unicode_charlist_string(list: Eterm) -> Result<String> {
    let mut out = String::new();
    let mut cur = list;
    while access::is_list(cur) {
        let elem = access::car(cur);
        if !access::is_small(elem) {
            return Err(badarg());
        }
        // `char::from_u32` rejects the surrogate range and everything above
        // `16#10FFFF`; `u32::try_from` rejects a negative code point.
        let ch = u32::try_from(signed_value(elem))
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(badarg)?;
        out.push(ch);
        cur = access::cdr(cur);
    }
    if !access::is_nil(cur) {
        return Err(badarg());
    }
    Ok(out)
}

/// `display_string(Stream, String)` (`bif.c:4196`) — writes the string to
/// `Stream` and returns `true`; no line terminator is added.
///
/// `Stream` must be one of the atoms `stdout`/`stderr`/`stdin` (any other term is
/// `badarg`), and `String` must be a Unicode charlist or a byte-aligned binary —
/// `display_string(stdout, 5)`, `display_string(stdout, [<<1:1>>])` and
/// `display_string(stdout, <<1:1>>)` are all `badarg`, while the empty list writes
/// nothing. A charlist is encoded as UTF-8, a binary is written byte for byte:
/// pinned to the oracle, `display_string(stdout, [246])` writes `0xC3 0xB6`,
/// `display_string(stdout, [16#10FFFF])` writes `0xF4 0x8F 0xBF 0xBF`, and
/// `display_string(stdout, <<255,0>>)` writes the raw bytes `0xFF 0x00`.
///
/// Only `stdout` is reproduced: it is the stream the installed
/// [`OutputSink`](crate::context::OutputSink) models. `stderr`/`stdin` (the C's
/// `write(2)`/`ioctl(TIOCSTI)` paths) and a binary payload that is not valid UTF-8
/// (the sink takes a `&str`) report the typed `unsupported_bif` instead of writing
/// the bytes to the wrong place or substituting them.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) as described above;
/// `error:unsupported_bif` for the two unmodelled cases; otherwise the sink's I/O
/// failure.
fn display_string_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let stream = args[0];
    if !access::is_atom(stream) {
        return Err(badarg());
    }
    // The C accepts exactly `stdout`, `stderr` and (with TIOCSTI) `stdin`; every
    // other atom — including one that merely *names* a stream — is `badarg`.
    let name = ctx.atoms().name(atom_val(stream));
    let stdout = name == Some("stdout");
    if !stdout && name != Some("stderr") && name != Some("stdin") {
        return Err(badarg());
    }

    let payload = args[1];
    let text = if access::is_nil(payload) || access::is_list(payload) {
        unicode_charlist_string(payload)?
    } else if bits::is_bitstring(payload) {
        // `erts_get_aligned_binary_bytes`: a non-byte-aligned bitstring is `badarg`.
        match String::from_utf8(bits::bytes(payload)?) {
            Ok(text) => text,
            Err(_) => return Err(unsupported_bif()),
        }
    } else {
        return Err(badarg());
    };

    if !stdout {
        return Err(unsupported_bif());
    }
    ctx.write_output(&text)?;
    ctx.boolean(true)
}

/// Registers this module's BIFs (eight keys: seven `erlang` and one `binary`).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "bitstring_to_list", 1, bitstring_to_list_1);
    reg.register("erlang", "delete_element", 2, delete_element_2);
    reg.register("erlang", "insert_element", 3, insert_element_3);
    reg.register("erlang", "iolist_to_iovec", 1, iolist_to_iovec_1);
    reg.register("erlang", "is_integer", 3, is_integer_3);
    reg.register("erlang", "display", 1, display_1);
    reg.register("erlang", "display_string", 2, display_string_2);
    reg.register("binary", "referenced_byte_size", 1, referenced_byte_size_1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a", "b"]);
        atoms
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    #[test]
    fn register_installs_the_roster() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 8);
        assert!(reg.contains("erlang", "bitstring_to_list", 1));
        assert!(reg.contains("binary", "referenced_byte_size", 1));
        // Deliberately out of scope, so unregistered.
        assert!(!reg.contains("erlang", "pid_to_list", 1));
        assert!(!reg.contains("erlang", "external_size", 1));
    }

    #[test]
    fn bitstring_to_list_splits_the_trailing_bits() {
        with_ctx(|ctx| {
            // `<<1,2,3>>` ⇒ `[1,2,3]`.
            let three = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            let list = bitstring_to_list_1(ctx, &[three]).expect("a bitstring");
            assert_eq!(signed_value(access::car(list)), 1);
            let rest = access::cdr(access::cdr(list));
            assert_eq!(signed_value(access::car(rest)), 3);
            // `<<>>` ⇒ `[]`.
            let empty = bits::make_binary(ctx.heap(), &[]).expect("fits");
            assert_eq!(bitstring_to_list_1(ctx, &[empty]).expect("a bitstring"), NIL);
            // A non-bitstring is `badarg`.
            assert!(matches!(
                bitstring_to_list_1(ctx, &[make_small(7)]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn delete_and_insert_element_are_one_based() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let tuple = ctx.tuple(&[a, b]).expect("fits");

            let dropped = delete_element_2(ctx, &[make_small(1), tuple]).expect("fits");
            assert_eq!(access::tuple_size(dropped), 1);
            assert_eq!(access::tuple_element(dropped, 0), b);

            let inserted = insert_element_3(ctx, &[make_small(2), tuple, a]).expect("fits");
            assert_eq!(access::tuple_size(inserted), 3);
            assert_eq!(access::tuple_element(inserted, 1), a);

            // `0`, `arity + 1` (for delete) and a non-tuple are `badarg`.
            assert!(matches!(
                delete_element_2(ctx, &[make_small(0), tuple]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                delete_element_2(ctx, &[make_small(3), tuple]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                insert_element_3(ctx, &[make_small(4), tuple, a]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                insert_element_3(ctx, &[make_small(1), NIL, a]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn referenced_byte_size_is_the_byte_size() {
        with_ctx(|ctx| {
            for n in [0usize, 3, 64, 65, 1000] {
                let bin = bits::make_binary(ctx.heap(), &vec![7u8; n]).expect("fits");
                assert_eq!(
                    signed_value(referenced_byte_size_1(ctx, &[bin]).expect("a bitstring")),
                    n as i64
                );
            }
            // A partial trailing byte truncates, like `size/1`.
            let one = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert_eq!(signed_value(referenced_byte_size_1(ctx, &[one]).expect("a string")), 0);
            assert!(matches!(
                referenced_byte_size_1(ctx, &[NIL]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn is_integer_3_checks_the_range_arithmetically() {
        with_ctx(|ctx| {
            let five = ctx.int(5).expect("fits");
            let one = ctx.int(1).expect("fits");
            let ten = ctx.int(10).expect("fits");
            let within = is_integer_3(ctx, &[five, one, ten]).expect("integer bounds");
            assert_eq!(ctx.atoms().name(atom_val(within)), Some("true"));
            let without = is_integer_3(ctx, &[five, ten, one]).expect("integer bounds");
            assert_eq!(ctx.atoms().name(atom_val(without)), Some("false"));

            let float = ctx.make_float(1.0).expect("fits");
            assert!(matches!(
                is_integer_3(ctx, &[five, float, ten]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                is_integer_3(ctx, &[five, one, float]),
                Err(BeamError::BadArg)
            ));
            // A non-integer `Term` is `false` with valid bounds.
            let a = ctx.atom("a").expect("seeded");
            let non_integer = is_integer_3(ctx, &[a, one, ten]).expect("integer bounds");
            assert_eq!(ctx.atoms().name(atom_val(non_integer)), Some("false"));
        });
    }

    #[test]
    fn display_and_display_string_route_through_the_sink() {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = atoms();
        atoms.seed(["stdout", "stderr", "stdin"]);
        let mut sink = crate::context::BytesSink::new();
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_output(&mut sink);
            // `erlang:display(<<"a b">>)` ⇒ `<<"a b">>\r\n` (the oracle's bytes).
            let bin = bits::make_binary(ctx.heap(), b"a b").expect("fits");
            let result = display_1(&mut ctx, &[bin]).expect("renderable");
            assert_eq!(ctx.atoms().name(atom_val(result)), Some("true"));

            // `display_string(stdout, [246])` ⇒ the UTF-8 bytes `0xC3 0xB6`.
            let stdout = ctx.atom("stdout").expect("seeded");
            let umlaut = ctx.list(&[246].map(|c| ctx.int(c).expect("fits"))).expect("fits");
            display_string_2(&mut ctx, &[stdout, umlaut]).expect("writable");

            // `stderr` has no seam: a typed error, never a write to the wrong stream.
            let stderr = ctx.atom("stderr").expect("seeded");
            assert!(display_string_2(&mut ctx, &[stderr, NIL]).is_err());
        }
        assert_eq!(sink.contents(), b"<<\"a b\">>\r\n\xC3\xB6".to_vec());
    }
}
