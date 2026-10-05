//! The instruction-stream decoder: `Code` bytes → [`RawInstr`]s.
//!
//! Each instruction is one opcode byte (`1..=max_opcode`) followed by
//! [`Opcode::arity`] operands. Each operand is one *tag/length* byte plus an
//! optional payload (`beam_file.c:167-240`); the tag is `byte & 0x07` (a file only
//! ever carries tags 0–7):
//!
//! | tag | meaning |
//! |-----|---------|
//! | `0` | `u` untagged integer |
//! | `1` | `i` integer (may be negative) |
//! | `2` | `a` atom index (0 ⇒ NIL) |
//! | `3` | `x` X register |
//! | `4` | `y` Y register |
//! | `5` | `f` label (0 ⇒ [`crate::operand::NO_FAIL`]) |
//! | `6` | `h` character |
//! | `7` | `z` extended sub-tag (see the crate `CONTEXT.md`) |
//!
//! The tag byte also encodes the operand's variable-length integer: `byte & 0x08 ==
//! 0` ⇒ value `byte >> 4`; else `byte & 0x10 == 0` ⇒ a 1-byte big-endian tail
//! (0–0x7FF); else `(byte >> 5) + 2` big-endian magnitude bytes, with
//! `(byte >> 5) == 7` meaning a `u`-tagged sub-varint gives `count = S + 9` bytes.
//!
//! All reads go through [`beam_codec::Reader`] (the shared bounds-checked
//! big-endian cursor); a malformed stream yields `BeamError::BadArg`, never a panic.
use beam_common::Result;
use beam_term::Atom;

use crate::error::bad_file;
use crate::ir::{CodeVersion, TypeTable};
use crate::opcode::Opcode;
use crate::operand::{AllocKind, Label, Operand, Reg, TypeIndex};

/// Reads one packed "tagged number" from a stream (`beam_file.c:167-240`,
/// `beamreader_read_tagged`).
///
/// This is the single lowest-common-ancestor decoder for the tag/length byte used by
/// the instruction operand decoder ([`decode_operand`]) *and* by the `AtU8`/`Line`
/// chunk parsers ([`crate::chunk::atom`], [`crate::chunk::line`]); it is exposed at
/// crate scope so those callers share one implementation rather than re-deriving the
/// encoding.
///
/// The first byte is `tag = byte & 0x07` and encodes the value's length:
/// `byte & 0x08 == 0` ⇒ value `byte >> 4` (0–15); else `byte & 0x10 == 0` ⇒ a 1-byte
/// big-endian tail with value `((byte >> 5) << 8) | tail`; else `(byte >> 5) + 2`
/// big-endian magnitude bytes, where `(byte >> 5) == 7` means a `u`-tagged sub-length
/// `S` precedes `S + 9` magnitude bytes.
///
/// Returns `(tag, magnitude, byte_count)`:
/// * `tag` — the low three bits of the tag byte (`0 u`, `1 i`, `2 a`, …).
/// * `magnitude` — the raw big-endian integer value.
/// * `byte_count` — the number of magnitude bytes the value was packed in, `0` for
///   the in-nibble form (used by [`decode_operand`] to sign-extend an `i` value).
///
/// A magnitude wider than eight bytes yields `BeamError::SystemLimit`; a sub-length
/// whose tag is not `u` yields `BeamError::BadArg`. Never panics.
pub(crate) fn read_tagged(r: &mut beam_codec::Reader<'_>) -> Result<(u8, u64, usize)> {
    let b = r.u8()?;
    let tag = b & 0x07;
    if (b & 0x08) == 0 {
        return Ok((tag, (b >> 4) as u64, 0));
    }
    if (b & 0x10) == 0 {
        let tail = r.u8()?;
        return Ok((tag, (((b >> 5) as u64) << 8) | tail as u64, 1));
    }
    let count = if (b >> 5) < 7 {
        (b >> 5) as u64 + 2
    } else {
        let (sub_tag, sub_value, _) = read_tagged(r)?;
        if sub_tag != 0 {
            return Err(crate::error::bad_file("tagged sub-length is not unsigned"));
        }
        sub_value + 9
    };
    if count > 8 {
        return Err(beam_common::BeamError::SystemLimit);
    }
    let bytes = r.bytes(count as usize)?;
    let mut value: u64 = 0;
    for &byte in bytes {
        value = (value << 8) | byte as u64;
    }
    Ok((tag, value, count as usize))
}

/// One decoded instruction: opcode, operands and its byte offset in the stream.
///
/// The offset lets [`crate::lower`] attach line/backtrace information and lets the
/// engine report a source location for an exception.
#[derive(Clone, Debug)]
pub struct RawInstr {
    /// The instruction's opcode.
    pub opcode: Opcode,
    /// The decoded operands, in the order the opcode declares them.
    pub operands: Vec<Operand>,
    /// The byte offset of the opcode in the `Code` instruction stream.
    pub offset: u32,
}

/// Decodes a `Code` instruction stream into [`RawInstr`]s.
///
/// `atoms` maps a *wire* atom index — the magnitude of an `a` operand — to the
/// interned [`Atom`] (`atoms[0]` is the NIL placeholder and is never looked up,
/// because `a,0` decodes to [`Operand::Nil`]). `types` is the module's
/// [`TypeTable`]: it decides whether a `z,5` `tr` operand keeps its type index or
/// collapses to a plain register. `version` supplies `max_opcode`, the valid
/// opcode-byte bound.
///
/// The stream ends at the `int_code_end/0` instruction, **not** at size exhaustion:
/// a stream that ends before `int_code_end` is malformed (the `Code` chunk's size
/// includes trailing padding, which the reader must never try to interpret). A zero
/// opcode byte, an opcode byte above `max_opcode`, an opcode with no roster entry, a
/// truncated instruction, an out-of-range atom index or an unknown/obsolete `z`
/// sub-tag yields [`beam_common::BeamError::BadArg`]. Never panics.
pub fn decode_stream(
    stream: &[u8],
    atoms: &[Atom],
    version: &CodeVersion,
    types: &TypeTable,
) -> Result<Vec<RawInstr>> {
    let mut r = beam_codec::Reader::new(stream);
    let mut instrs = Vec::new();

    loop {
        // The stream must be terminated by `int_code_end`; running out of bytes
        // first means the opcode/operand framing did not line up with the chunk.
        if r.is_empty() {
            return Err(bad_file("instruction stream ended before int_code_end"));
        }

        // Recorded before the opcode byte is consumed, so it is the instruction's
        // own offset in the `Code` stream (what line/backtrace mapping needs).
        let offset = r.position() as u32;

        let op_byte = r.u8()?;
        if op_byte == 0 {
            return Err(bad_file("zero opcode byte"));
        }
        if u32::from(op_byte) > version.max_opcode {
            return Err(bad_file("opcode above the chunk's max_opcode"));
        }
        let op = Opcode::from_u8(op_byte).ok_or_else(|| bad_file("opcode is not in the roster"))?;

        let arity = usize::from(op.arity());
        let mut operands = Vec::with_capacity(arity);
        for _ in 0..arity {
            operands.push(decode_operand(&mut r, atoms, types)?);
        }

        instrs.push(RawInstr {
            opcode: op,
            operands,
            offset,
        });

        if op == Opcode::int_code_end {
            return Ok(instrs);
        }
    }
}

/// Decodes one operand from the instruction stream.
///
/// Reads the tag/length byte and its payload from `r`, returning the typed
/// [`Operand`]. `atoms` maps a wire atom index to the interned [`Atom`] (used by the
/// `a` tag; an index outside `atoms` is a malformed image). `types` is the module's
/// [`TypeTable`]: when it is a fallback the `z,5` `tr` operand collapses to a plain
/// X/Y register (the type index is discarded), and a `z,1` argument list recurses
/// into [`decode_operand`].
///
/// A short read, an out-of-range atom index or an unknown/obsolete `z` sub-tag
/// (notably sub-tag 0, the obsolete float form) yields
/// [`beam_common::BeamError::BadArg`]. Never panics.
pub fn decode_operand(
    r: &mut beam_codec::Reader<'_>,
    atoms: &[Atom],
    types: &TypeTable,
) -> Result<Operand> {
    // One tag/length byte plus its magnitude, decoded by the shared primitive.
    let (tag, magnitude, byte_count) = read_tagged(r)?;

    match tag {
        // `u` — untagged integer. The file's encoding is unsigned; the value is
        // carried as the IR's `i64` (the compiler only emits tagged integers that
        // fit, and `i` is the tag for a negative value).
        0 => Ok(Operand::Integer(magnitude as i64)),
        // `i` — signed integer. A negative value is *always* packed in the
        // explicit-count form (2–8 magnitude bytes, so `byte_count >= 2`): the
        // compiler's writer routes every `N < 0` through `encode1/2`
        // (`../../lib/compiler/src/beam_asm.erl:856`), and `beamreader_read_tagged`
        // sign-extends only that branch, from `count * 8` bits
        // (`beam_file.c:205-215`). The in-nibble form (0–15) and the 1-byte-tail
        // form (16–0x7FF) are read as positive values, because that is what a
        // *positive* operand of either width is written as (`beam_asm.erl:858-860`).
        1 if byte_count >= 2 => Ok(Operand::Integer(sign_extend(magnitude, byte_count))),
        1 => Ok(Operand::Integer(magnitude as i64)),
        // `a` — atom index (0 ⇒ NIL).
        2 => {
            if magnitude == 0 {
                return Ok(Operand::Nil);
            }
            let atom = atoms
                .get(magnitude as usize)
                .copied()
                .ok_or_else(|| bad_file("atom index out of range"))?;
            Ok(Operand::Atom(atom))
        }
        // `x` — X register.
        3 => Ok(Operand::X(magnitude as u32)),
        // `y` — Y register (stack slot).
        4 => Ok(Operand::Y(magnitude as u32)),
        // `f` — label; `f,0` is the failure sentinel `NO_FAIL`, kept verbatim.
        5 => Ok(Operand::Label(Label(magnitude as u32))),
        // `h` — character (defined but never emitted by the compiler).
        6 => Ok(Operand::Character(magnitude as u32)),
        // `z` — extended form; the magnitude is the sub-tag.
        7 => decode_extended(r, magnitude, atoms, types),
        // Tags 8–14 exist only inside the loader and must never appear in a file.
        _ => Err(bad_file("unrecognized operand tag")),
    }
}

/// Sign-extends an `i`-tagged magnitude packed in `byte_count` magnitude bytes.
///
/// Only the explicit-count form (`2..=8` bytes — see [`decode_operand`]'s `i` arm)
/// reaches here, so the value's sign bit is the top bit of that width and the left
/// shift is `16..=48`, which never overflows (`beam_file.c:205-215`).
fn sign_extend(magnitude: u64, byte_count: usize) -> i64 {
    let shift = 64 - 8 * byte_count;
    ((magnitude << shift) as i64) >> shift
}

/// Decodes the payload of a `z` (extended) operand, whose magnitude is the sub-tag
/// (`beam_file.c:2339-2447`).
///
/// Sub-tag `0` (the pre-R16B float form) and any unassigned sub-tag are rejected:
/// only 1–5 are ever emitted.
fn decode_extended(
    r: &mut beam_codec::Reader<'_>,
    sub_tag: u64,
    atoms: &[Atom],
    types: &TypeTable,
) -> Result<Operand> {
    match sub_tag {
        // 1 — argument list: a `u` count then that many sub-operands.
        //
        // The count is file-supplied, so the vector is *not* pre-allocated with it:
        // a bogus count fails on the first short read instead of trying to reserve
        // an attacker-chosen amount of memory.
        1 => {
            let count = read_unsigned(r, "argument-list length is not unsigned")?;
            let mut args = Vec::new();
            for _ in 0..count {
                args.push(decode_operand(r, atoms, types)?);
            }
            Ok(Operand::List(args))
        }
        // 2 — float register index (`beam_file.c:2388-2395`).
        2 => {
            let index = read_unsigned(r, "float-register index is not unsigned")?;
            Ok(Operand::FloatReg(index as u32))
        }
        // 3 — allocation list: a `u` count then that many `(kind, number)` pairs.
        3 => {
            let count = read_unsigned(r, "allocation-list length is not unsigned")?;
            let mut entries = Vec::new();
            for _ in 0..count {
                let kind = read_unsigned(r, "allocation kind is not unsigned")?;
                let number = read_unsigned(r, "allocation count is not unsigned")?;
                entries.push(allocation_entry(kind, number)?);
            }
            Ok(Operand::Allocation(entries))
        }
        // 4 — static literal index into `Module.literals` (`beam_file.c:2400-2417`).
        4 => {
            let index = read_unsigned(r, "literal index is not unsigned")?;
            Ok(Operand::Literal(index as u32))
        }
        // 5 — `tr`, a register carrying a `Type` chunk index (`beam_file.c:2419-2443`).
        5 => {
            let reg = match decode_operand(r, atoms, types)? {
                Operand::X(n) => Reg::X(n),
                Operand::Y(n) => Reg::Y(n),
                // `tr` annotates a register, nothing else.
                _ => return Err(bad_file("`tr` does not annotate an x/y register")),
            };
            let index = read_unsigned(r, "type index is not unsigned")?;
            if types.fallback {
                // The `Type` chunk was stripped after compilation (or is
                // incompatible): the annotation is dropped and `tr` becomes an
                // ordinary register, exactly as the C loader does.
                Ok(match reg {
                    Reg::X(n) => Operand::X(n),
                    Reg::Y(n) => Operand::Y(n),
                })
            } else {
                Ok(Operand::Typed {
                    reg,
                    ty: TypeIndex(index as u32),
                })
            }
        }
        // 0 — the pre-R16B float byte form; anything else is unassigned.
        _ => Err(bad_file("unrecognized `z` sub-tag")),
    }
}

/// Reads one `u`-tagged (unsigned) sub-operand, rejecting every other tag.
///
/// Every `z` sub-tag payload is an unsigned count or index (`beam_file.c` asserts
/// `TAG_u` at each of them), so any other tag means the stream is malformed.
fn read_unsigned(r: &mut beam_codec::Reader<'_>, msg: &str) -> Result<u64> {
    let (tag, value, _) = read_tagged(r)?;
    if tag != 0 {
        return Err(bad_file(msg));
    }
    Ok(value)
}

/// One `z,3` allocation-list entry, with its heap-word contribution.
///
/// Mirrors `marshal_allocation_list` (`beam_file.c:2234-2284`): kind `0` counts raw
/// heap words and kind `1` counts floats, each of which takes `FLOAT_SIZE_OBJECT`
/// (2) words on the 64-bit emulator. Kinds `2` (funs) and `3` (native records) take
/// `ERL_FUN_SIZE`/`RECORD_INST_SIZE` words per element in the C loader; those object
/// layouts are not modeled by this crate yet, so `number` words is used as a
/// documented Milestone-1 placeholder. Any other kind is malformed.
fn allocation_entry(kind: u64, number: u64) -> Result<AllocKind> {
    let words = match kind {
        0 => number,
        1 => number.saturating_mul(2),
        2 | 3 => number,
        _ => return Err(bad_file("invalid allocation-list kind")),
    };
    // The IR stores 32-bit counts; a wider one is a system limit, not a `badarg`.
    if number > u64::from(u32::MAX) || words > u64::from(u32::MAX) {
        return Err(beam_common::BeamError::SystemLimit);
    }
    Ok(AllocKind {
        kind: kind as u32,
        number: number as u32,
        words: words as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{TestKind, Type};
    use crate::operand::NO_FAIL;
    use beam_term::AtomTable;

    /// The `Code` header of the committed `slice.beam` fixture: `max_opcode 181`,
    /// `label_count 15`, `function_count 6` (`tests/fixtures/README.md`).
    const VERSION: CodeVersion = CodeVersion {
        format_number: 0,
        max_opcode: 181,
        label_count: 15,
        function_count: 6,
    };

    /// Opcode numbers used by the stream tests (from `genop.tab` via
    /// [`crate::opcode::OPCODE_TABLE`]).
    const OP_MOVE: u8 = 64;
    const OP_IS_EQ_EXACT: u8 = 43;
    const OP_GC_BIF2: u8 = 125;
    const OP_INT_CODE_END: u8 = 3;

    /// Packs one `(tag, value)` operand into the file's tag/length byte plus
    /// magnitude bytes (`beam_file.c:167-240`), using the same three encodings the
    /// compiler emits:
    /// * value 0–15 ⇒ `value << 4 | tag` (in-nibble form);
    /// * value 16–0x7FF ⇒ `0x08 | ((value >> 8) << 5) | tag` + one magnitude byte;
    /// * wider ⇒ `0x18 | ((count - 2) << 5) | tag` + `count` big-endian bytes
    ///   (`0x18` = `0x08 | 0x10`: both length bits set means "explicit count").
    fn tagged(tag: u8, value: u64) -> Vec<u8> {
        assert!(tag <= 7, "a `.beam` file only ever carries tags 0-7");
        if value <= 15 {
            vec![((value as u8) << 4) | tag]
        } else if value < 0x800 {
            vec![0x08 | (((value >> 8) as u8) << 5) | tag, value as u8]
        } else {
            let count = ((64 - value.leading_zeros()) as usize + 7) / 8; // 2..=8
            let mut out = vec![0x18 | (((count - 2) as u8) << 5) | tag];
            out.extend_from_slice(&value.to_be_bytes()[8 - count..]);
            out
        }
    }

    /// The bytes of a synthetic instruction stream.
    fn stream(parts: &[&[u8]]) -> Vec<u8> {
        parts.iter().flat_map(|p| p.iter().copied()).collect()
    }

    /// Asserts that a decode step failed with the loader's `badarg`.
    ///
    /// [`beam_common::BeamError`] deliberately does not implement `PartialEq`, so
    /// the variant is matched rather than compared.
    #[track_caller]
    fn assert_bad_arg<T>(result: Result<T>) {
        match result {
            Err(beam_common::BeamError::BadArg) => {}
            Err(other) => panic!("expected BadArg, got {other:?}"),
            Ok(_) => panic!("expected BadArg, got a successful decode"),
        }
    }

    /// The fallback `TypeTable` (no usable `Type` chunk): `tr` collapses to a plain
    /// register.
    fn fallback_types() -> TypeTable {
        TypeTable::default()
    }

    /// A real (non-fallback) `TypeTable` with `count` entries.
    fn types_with(count: usize) -> TypeTable {
        TypeTable {
            entries: vec![Type::default(); count],
            fallback: false,
        }
    }

    // ---- decode_stream ----------------------------------------------------

    #[test]
    fn int_code_end_terminates_an_empty_stream() {
        let instrs = decode_stream(&[OP_INT_CODE_END], &[], &VERSION, &fallback_types()).unwrap();
        assert_eq!(instrs.len(), 1);
        assert_eq!(instrs[0].opcode, Opcode::int_code_end);
        assert!(instrs[0].operands.is_empty());
        assert_eq!(instrs[0].offset, 0);
    }

    #[test]
    fn decodes_move_x0_to_x1() {
        // move/2 = opcode 64, operands `x0 x1`: x0 = tag 3 value 0 = 0x03,
        // x1 = tag 3 value 1 = 0x13; then int_code_end (3).
        let bytes = stream(&[&[OP_MOVE], &tagged(3, 0), &tagged(3, 1), &[OP_INT_CODE_END]]);
        let instrs = decode_stream(&bytes, &[], &VERSION, &fallback_types()).unwrap();

        assert_eq!(instrs.len(), 2);
        assert_eq!(instrs[0].opcode, Opcode::r#move);
        assert_eq!(instrs[0].operands, vec![Operand::X(0), Operand::X(1)]);
        assert_eq!(instrs[0].offset, 0);
        assert_eq!(instrs[1].opcode, Opcode::int_code_end);
        assert_eq!(instrs[1].offset, 3);
    }

    #[test]
    fn decodes_gc_bif2_operands_in_order() {
        // gc_bif2/6 = opcode 125: `fail live bif arg1 arg2 dst`, i.e.
        // `f,0` (0x05) `u,2` (0x20) `u,1` (0x10) `x1` (0x13) `x2` (0x23) `x1`.
        let bytes = stream(&[
            &[OP_GC_BIF2],
            &tagged(5, 0),
            &tagged(0, 2),
            &tagged(0, 1),
            &tagged(3, 1),
            &tagged(3, 2),
            &tagged(3, 1),
            &[OP_INT_CODE_END],
        ]);
        let instrs = decode_stream(&bytes, &[], &VERSION, &fallback_types()).unwrap();

        assert_eq!(instrs[0].opcode, Opcode::gc_bif2);
        assert_eq!(
            instrs[0].operands,
            vec![
                Operand::Label(Label(0)),
                Operand::Integer(2),
                Operand::Integer(1),
                Operand::X(1),
                Operand::X(2),
                Operand::X(1),
            ]
        );
    }

    #[test]
    fn decodes_is_eq_exact_and_classifies_it() {
        // is_eq_exact/3 = opcode 43: `f,7` (0x75) `x0` (0x03) `a,1` (0x12).
        let mut atoms = AtomTable::new();
        let atom = atoms.intern("slice");
        let bytes = stream(&[
            &[OP_IS_EQ_EXACT],
            &tagged(5, 7),
            &tagged(3, 0),
            &tagged(2, 1),
            &[OP_INT_CODE_END],
        ]);
        let instrs =
            decode_stream(&bytes, &[Atom::MAX, atom], &VERSION, &fallback_types()).unwrap();

        assert_eq!(instrs[0].opcode, Opcode::is_eq_exact);
        assert_eq!(
            instrs[0].operands,
            vec![Operand::Label(Label(7)), Operand::X(0), Operand::Atom(atom),]
        );
        assert_eq!(
            TestKind::from_opcode(instrs[0].opcode),
            Some(TestKind::IsEqExact)
        );
    }

    #[test]
    fn rejects_a_stream_that_never_reaches_int_code_end() {
        // A complete `move` followed by EOF: the stream is unterminated.
        let bytes = stream(&[&[OP_MOVE], &tagged(3, 0), &tagged(3, 1)]);
        assert_bad_arg(decode_stream(&bytes, &[], &VERSION, &fallback_types()));

        // And an empty stream is unterminated too.
        assert_bad_arg(decode_stream(&[], &[], &VERSION, &fallback_types()));
    }

    #[test]
    fn rejects_a_zero_opcode_byte() {
        let bytes = &[0u8, OP_INT_CODE_END];
        assert_bad_arg(decode_stream(bytes, &[], &VERSION, &fallback_types()));
    }

    #[test]
    fn rejects_an_opcode_above_max_opcode() {
        // 182 > the chunk's `max_opcode` of 181.
        let bytes = &[182u8, OP_INT_CODE_END];
        assert_bad_arg(decode_stream(bytes, &[], &VERSION, &fallback_types()));
    }

    #[test]
    fn rejects_an_opcode_with_no_roster_entry() {
        // The roster is contiguous over 1..=193, so 194 has no `Opcode`; widen
        // `max_opcode` so the failure is the roster lookup, not the bound.
        let version = CodeVersion {
            max_opcode: 255,
            ..VERSION
        };
        let bytes = &[194u8, OP_INT_CODE_END];
        assert_bad_arg(decode_stream(bytes, &[], &version, &fallback_types()));
    }

    #[test]
    fn rejects_a_truncated_operand() {
        // `move` with only the first operand byte present.
        let bytes = stream(&[&[OP_MOVE], &tagged(3, 0)]);
        assert_bad_arg(decode_stream(&bytes, &[], &VERSION, &fallback_types()));
    }

    // ---- scalar operand tags ---------------------------------------------

    #[test]
    fn decodes_untagged_and_signed_integers() {
        let cases: &[(&[u8], i64)] = &[
            (&tagged(0, 0), 0),         // in-nibble `u,0`
            (&tagged(0, 15), 15),       // in-nibble `u,15`
            (&tagged(0, 16), 16),       // 1-byte tail `u,16`
            (&tagged(0, 0x7FF), 0x7FF), // 1-byte tail maximum
            (&tagged(0, 0x800), 0x800), // explicit 2-byte magnitude
            (&tagged(1, 0), 0),         // in-nibble `i,0`
            (&tagged(1, 15), 15),       // in-nibble `i,15`
            // The 1-byte-tail `i` form holds 16–0x7FF and is *positive*: the
            // compiler writes positive literals of that size this way
            // (`beam_asm.erl:858-860`) and `beamreader_read_tagged` sign-extends
            // only the explicit-count form (`beam_file.c:205-215`).
            (&tagged(1, 200), 200),
            (&tagged(1, 0x7FF), 0x7FF),
        ];
        for &(bytes, expected) in cases {
            assert_eq!(
                decode_operand(&mut beam_codec::Reader::new(bytes), &[], &fallback_types())
                    .unwrap(),
                Operand::Integer(expected),
                "bytes {bytes:02x?}"
            );
        }

        // Negatives are packed in the explicit-count form (`0x19` = tag `i`, two
        // magnitude bytes; `0x39` = three) and sign-extended from that width.
        let negative_cases: &[(&[u8], i64)] = &[
            (&[0x19, 0xFF, 0xFF], -1),         // 0xFFFF as i16
            (&[0x19, 0xFF, 0xFE], -2),         // 0xFFFE as i16
            (&[0x19, 0xFF, 0x38], -200),       // 0xFF38 as i16
            (&[0x19, 0x80, 0x00], -32_768),    // i16::MIN
            (&[0x39, 0xFF, 0xFF, 0x38], -200), // 0xFFFF38 as i24
        ];
        for &(bytes, expected) in negative_cases {
            assert_eq!(
                decode_operand(&mut beam_codec::Reader::new(bytes), &[], &fallback_types())
                    .unwrap(),
                Operand::Integer(expected),
                "bytes {bytes:02x?}"
            );
        }

        // The widest form: eight magnitude bytes, sign-extended from all 64 bits.
        let widest = tagged(1, u64::MAX);
        assert_eq!(widest.len(), 9, "tag byte + eight magnitude bytes");
        assert_eq!(
            decode_operand(
                &mut beam_codec::Reader::new(&widest),
                &[],
                &fallback_types()
            )
            .unwrap(),
            Operand::Integer(-1)
        );
    }

    #[test]
    fn decodes_atoms_and_the_nil_placeholder() {
        let mut table = AtomTable::new();
        let slice = table.intern("slice");
        let add = table.intern("add");
        let atoms = [Atom::MAX, slice, add];

        // `a,0` is NIL, not `atoms[0]`.
        assert_eq!(
            decode_operand(
                &mut beam_codec::Reader::new(&tagged(2, 0)),
                &atoms,
                &fallback_types()
            )
            .unwrap(),
            Operand::Nil
        );
        assert_eq!(
            decode_operand(
                &mut beam_codec::Reader::new(&tagged(2, 2)),
                &atoms,
                &fallback_types()
            )
            .unwrap(),
            Operand::Atom(add)
        );
    }

    #[test]
    fn rejects_an_out_of_range_atom_index() {
        // `a,1` with an empty atom table.
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&tagged(2, 1)),
            &[],
            &fallback_types(),
        ));
    }

    #[test]
    fn decodes_register_label_and_character_tags() {
        let cases: &[(&[u8], Operand)] = &[
            (&tagged(3, 0), Operand::X(0)),
            (&tagged(3, 12), Operand::X(12)),
            (&tagged(4, 1), Operand::Y(1)),
            (&tagged(5, 0), Operand::Label(NO_FAIL)),
            (&tagged(5, 8), Operand::Label(Label(8))),
            (&tagged(6, 0), Operand::Character(0)),
            (&tagged(6, 65), Operand::Character(65)),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                &decode_operand(&mut beam_codec::Reader::new(bytes), &[], &fallback_types())
                    .unwrap(),
                expected,
                "bytes {bytes:02x?}"
            );
        }
    }

    // ---- `z` extended sub-tags -------------------------------------------

    #[test]
    fn decodes_an_argument_list() {
        // `z,1` (0x17) `u,2` (0x20) `x0` (0x03) `y1` (0x14).
        let bytes = stream(&[&tagged(7, 1), &tagged(0, 2), &tagged(3, 0), &tagged(4, 1)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::List(vec![Operand::X(0), Operand::Y(1)])
        );

        // An empty list is legal framing-wise.
        let empty = stream(&[&tagged(7, 1), &tagged(0, 0)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&empty), &[], &fallback_types()).unwrap(),
            Operand::List(Vec::new())
        );
    }

    #[test]
    fn rejects_a_truncated_argument_list() {
        // `z,1` announcing 2 arguments but carrying one: the nested decode runs
        // out of bytes.
        let bytes = stream(&[&tagged(7, 1), &tagged(0, 2), &tagged(3, 0)]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&bytes),
            &[],
            &fallback_types(),
        ));
    }

    #[test]
    fn decodes_a_float_register() {
        // `z,2` (0x27) `u,1` (0x10).
        let bytes = stream(&[&tagged(7, 2), &tagged(0, 1)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::FloatReg(1)
        );
    }

    #[test]
    fn decodes_an_allocation_list_with_heap_words() {
        // `z,3` (0x37), count `u,4` (0x40 = in-nibble form), then the
        // `(kind, number)` pairs (0,3) (1,2) (2,5) (3,7).
        let bytes = stream(&[
            &tagged(7, 3),
            &tagged(0, 4),
            &tagged(0, 0),
            &tagged(0, 3),
            &tagged(0, 1),
            &tagged(0, 2),
            &tagged(0, 2),
            &tagged(0, 5),
            &tagged(0, 3),
            &tagged(0, 7),
        ]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::Allocation(vec![
                // kind 0: raw heap words.
                AllocKind {
                    kind: 0,
                    number: 3,
                    words: 3
                },
                // kind 1: floats, 2 words each.
                AllocKind {
                    kind: 1,
                    number: 2,
                    words: 4
                },
                // kinds 2/3: M1 placeholder = `number` words each.
                AllocKind {
                    kind: 2,
                    number: 5,
                    words: 5
                },
                AllocKind {
                    kind: 3,
                    number: 7,
                    words: 7
                },
            ])
        );
    }

    #[test]
    fn rejects_an_invalid_allocation_kind() {
        // `z,3` count 1, then kind 4 (unknown).
        let bytes = stream(&[&tagged(7, 3), &tagged(0, 1), &tagged(0, 4), &tagged(0, 1)]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&bytes),
            &[],
            &fallback_types(),
        ));
    }

    #[test]
    fn decodes_a_static_literal() {
        // `z,4` (0x47) `u,7` (0x70).
        let bytes = stream(&[&tagged(7, 4), &tagged(0, 7)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::Literal(7)
        );
    }

    #[test]
    fn collapses_a_typed_register_when_types_are_a_fallback() {
        // `z,5` (0x57) `x1` (0x13) type index `u,1` (0x10).
        let x = stream(&[&tagged(7, 5), &tagged(3, 1), &tagged(0, 1)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&x), &[], &fallback_types()).unwrap(),
            Operand::X(1)
        );

        let y = stream(&[&tagged(7, 5), &tagged(4, 2), &tagged(0, 0)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&y), &[], &fallback_types()).unwrap(),
            Operand::Y(2)
        );
    }

    #[test]
    fn keeps_the_type_index_of_a_typed_register() {
        let types = types_with(3);
        // `z,5` `x1` type index `u,2`.
        let x = stream(&[&tagged(7, 5), &tagged(3, 1), &tagged(0, 2)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&x), &[], &types).unwrap(),
            Operand::Typed {
                reg: Reg::X(1),
                ty: TypeIndex(2)
            }
        );

        // The "any type" index 0.
        let any = stream(&[&tagged(7, 5), &tagged(4, 0), &tagged(0, 0)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&any), &[], &types).unwrap(),
            Operand::Typed {
                reg: Reg::Y(0),
                ty: TypeIndex(0)
            }
        );
    }

    #[test]
    fn rejects_a_typed_register_that_is_not_a_register() {
        // `z,5` `a,1` type index `u,0` — `tr` may only annotate x/y.
        let mut table = AtomTable::new();
        let atom = table.intern("slice");
        let bytes = stream(&[&tagged(7, 5), &tagged(2, 1), &tagged(0, 0)]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&bytes),
            &[Atom::MAX, atom],
            &fallback_types(),
        ));
    }

    #[test]
    fn rejects_the_obsolete_and_unknown_z_sub_tags() {
        // Sub-tag 0: the pre-R16B float byte form.
        let obsolete = tagged(7, 0);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&obsolete),
            &[],
            &fallback_types(),
        ));

        // Sub-tag 6 and beyond are unassigned.
        for sub_tag in 6..=8u64 {
            let bytes = tagged(7, sub_tag);
            assert_bad_arg(decode_operand(
                &mut beam_codec::Reader::new(&bytes),
                &[],
                &fallback_types(),
            ));
        }
    }

    // ---- TestKind::from_opcode -------------------------------------------

    #[test]
    fn test_kind_maps_the_is_family() {
        let cases: &[(Opcode, TestKind)] = &[
            (Opcode::is_lt, TestKind::IsLt),
            (Opcode::is_ge, TestKind::IsGe),
            (Opcode::is_eq, TestKind::IsEq),
            (Opcode::is_ne, TestKind::IsNe),
            (Opcode::is_eq_exact, TestKind::IsEqExact),
            (Opcode::is_ne_exact, TestKind::IsNeExact),
            (Opcode::is_integer, TestKind::IsInteger),
            (Opcode::is_float, TestKind::IsFloat),
            (Opcode::is_number, TestKind::IsNumber),
            (Opcode::is_atom, TestKind::IsAtom),
            (Opcode::is_pid, TestKind::IsPid),
            (Opcode::is_reference, TestKind::IsReference),
            (Opcode::is_port, TestKind::IsPort),
            (Opcode::is_nil, TestKind::IsNil),
            (Opcode::is_binary, TestKind::IsBinary),
            (Opcode::is_bitstr, TestKind::IsBitstr),
            (Opcode::is_list, TestKind::IsList),
            (Opcode::is_nonempty_list, TestKind::IsNonemptyList),
            (Opcode::is_tuple, TestKind::IsTuple),
            (Opcode::is_map, TestKind::IsMap),
            (Opcode::is_boolean, TestKind::IsBoolean),
            (Opcode::is_function, TestKind::IsFunction),
            (Opcode::is_function2, TestKind::IsFunction2),
            (Opcode::is_tagged_tuple, TestKind::IsTaggedTuple),
            (Opcode::test_arity, TestKind::TestArity),
        ];
        for (op, kind) in cases {
            assert_eq!(&TestKind::from_opcode(*op), &Some(*kind), "{}", op.name());
        }
    }

    #[test]
    fn test_kind_is_none_for_every_other_opcode() {
        // A handful of neighbours and non-test opcodes must not be classified.
        for op in [
            Opcode::r#move,
            Opcode::label,
            Opcode::int_code_end,
            Opcode::func_info,
            Opcode::is_constant,
            Opcode::is_record_accessible,
            Opcode::gc_bif2,
            Opcode::select_val,
        ] {
            assert_eq!(TestKind::from_opcode(op), None, "{}", op.name());
        }

        // Exhaustively: `from_opcode` is `Some` for exactly the 25 test opcodes.
        let classified = crate::opcode::OPCODE_TABLE
            .iter()
            .filter(|(_, op, _, _)| TestKind::from_opcode(*op).is_some())
            .count();
        assert_eq!(classified, 25);
    }
}
