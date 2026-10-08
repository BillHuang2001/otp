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

//! Negative integration tests: every malformed image must yield a typed
//! `BeamError`, never a panic.
//!
//! Container-level cases mutate the committed oracle fixture
//! (`tests/fixtures/slice.beam`) in memory and run the *whole* pipeline through
//! [`beam_loader::load`]; instruction-level cases drive the public
//! [`beam_loader::decode::decode_stream`] with crafted byte streams.
use beam_common::BeamError;
use beam_loader::decode::decode_stream;
use beam_loader::{load, Atom, CodeVersion, TypeTable};

/// The committed oracle fixture, as bytes.
const FIXTURE: &[u8] = include_bytes!("fixtures/slice.beam");

/// Asserts the result is `Err(BeamError::BadArg)` (and, implicitly, that producing
/// it did not panic).
fn assert_bad_arg<T: std::fmt::Debug>(result: Result<T, BeamError>, what: &str) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected BeamError::BadArg, got {other:?}"),
    }
}

/// The byte offset of `id`'s payload inside a `FOR1`/`BEAM` image.
///
/// Walks the chunk directory exactly as the loader's container parser does so the
/// patched offsets stay correct if the fixture is regenerated.
fn chunk_payload_offset(image: &[u8], id: &[u8; 4]) -> usize {
    let mut pos = 12usize; // FOR1 + form_length + BEAM
    while pos + 8 <= image.len() {
        let cid = &image[pos..pos + 4];
        let size = u32::from_be_bytes(image[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if cid == id {
            return pos + 8;
        }
        pos += 8 + size + (4 - (size % 4)) % 4;
    }
    panic!("chunk {id:?} not found in the fixture");
}

/// The offset of the first instruction byte in the `Code` chunk.
fn code_stream_offset(image: &[u8]) -> usize {
    let payload = chunk_payload_offset(image, b"Code");
    let head_size = u32::from_be_bytes(image[payload..payload + 4].try_into().unwrap()) as usize;
    payload + 4 + head_size
}

/// The `CodeVersion` used for the crafted instruction streams.
fn version() -> CodeVersion {
    CodeVersion {
        format_number: 0,
        max_opcode: 181,
        label_count: 1,
        function_count: 1,
    }
}

#[test]
fn rejects_a_truncated_header() {
    // Eight zero bytes: shorter than the 12-byte preamble (and not a `FOR1` image).
    assert_bad_arg(load(&[0u8; 8]), "8 zero bytes");
    assert_bad_arg(load(&[]), "empty image");
}

#[test]
fn rejects_a_bad_for1_magic() {
    let mut image = FIXTURE.to_vec();
    image[0..4].copy_from_slice(b"XXXX");
    assert_bad_arg(load(&image), "bad FOR1 magic");
}

#[test]
fn rejects_a_bad_form_type() {
    let mut image = FIXTURE.to_vec();
    image[8..12].copy_from_slice(b"NOPE");
    assert_bad_arg(load(&image), "bad BEAM form type");
}

#[test]
fn rejects_a_nonzero_code_format_number() {
    let mut image = FIXTURE.to_vec();
    // The `Code` chunk payload is `u32 head_size` then `u32 format_number`.
    let payload = chunk_payload_offset(&image, b"Code");
    image[payload + 4..payload + 8].copy_from_slice(&1u32.to_be_bytes());
    assert_bad_arg(load(&image), "Code format_number != 0");
}

#[test]
fn rejects_a_chunk_size_that_overruns_the_file() {
    let mut image = FIXTURE.to_vec();
    // The first chunk's size field is at preamble(12) + id(4).
    image[16..20].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    assert_bad_arg(load(&image), "chunk size overruns the file");
}

#[test]
fn rejects_a_truncated_image() {
    for len in [0usize, 4, 8, 11, 12, 40, FIXTURE.len() / 2, FIXTURE.len() - 1] {
        assert_bad_arg(load(&FIXTURE[..len]), &format!("truncated to {len} bytes"));
    }
}

#[test]
fn rejects_a_zero_opcode_byte() {
    let mut image = FIXTURE.to_vec();
    let stream = code_stream_offset(&image);
    image[stream] = 0;
    assert_bad_arg(load(&image), "zero opcode byte");
}

#[test]
fn rejects_an_opcode_above_max_opcode() {
    let mut image = FIXTURE.to_vec();
    let stream = code_stream_offset(&image);
    // `max_opcode` is 181, so 0xFF is out of the file's opcode space.
    image[stream] = 0xFF;
    assert_bad_arg(load(&image), "opcode above max_opcode");
}

// ---- Instruction-level cases (crafted streams) --------------------------------

#[test]
fn rejects_a_zero_opcode_byte_in_a_crafted_stream() {
    let atoms = [Atom::MAX];
    assert_bad_arg(
        decode_stream(&[0x00], &atoms, &version(), &TypeTable::default()),
        "zero opcode",
    );
}

#[test]
fn rejects_an_out_of_roster_opcode_in_a_crafted_stream() {
    let atoms = [Atom::MAX];
    // `max_opcode` 200 admits the byte (the roster stops at 193), but 194 is not a
    // `genop.tab` opcode.
    let version = CodeVersion {
        format_number: 0,
        max_opcode: 200,
        label_count: 1,
        function_count: 1,
    };
    assert_bad_arg(
        decode_stream(&[0xC2u8], &atoms, &version, &TypeTable::default()),
        "byte outside the roster",
    );
}

#[test]
fn rejects_an_out_of_range_atom_index() {
    // `func_info` (opcode 2) whose module operand is `a,200`: the tag byte `0x0A`
    // is the 1-byte-tail form of the `a` tag and `0xC8` is 200, far past the
    // one-entry atom map.
    let stream = [0x02u8, 0x0A, 0xC8, 0x12, 0x20, 0x03];
    let atoms = [Atom::MAX];
    assert_bad_arg(
        decode_stream(&stream, &atoms, &version(), &TypeTable::default()),
        "atom index out of range",
    );
}

#[test]
fn rejects_a_truncated_operand() {
    // `func_info` followed by a lone `a` tag byte whose 1-byte tail is missing.
    let stream = [0x02u8, 0x0A];
    let atoms = [Atom::MAX];
    assert_bad_arg(
        decode_stream(&stream, &atoms, &version(), &TypeTable::default()),
        "truncated operand",
    );
}

#[test]
fn rejects_a_stream_that_never_reaches_int_code_end() {
    // A well-formed `label 1` instruction, then end of bytes: the stream must be
    // terminated by `int_code_end`.
    let stream = [0x01u8, 0x15];
    let atoms = [Atom::MAX];
    assert_bad_arg(
        decode_stream(&stream, &atoms, &version(), &TypeTable::default()),
        "no int_code_end",
    );
}

#[test]
fn rejects_a_missing_atom_or_code_chunk() {
    // A `FOR1`/`BEAM` container with no chunks at all: `AtU8` and `Code` are both
    // structurally required.
    let mut image = Vec::new();
    image.extend_from_slice(b"FOR1");
    image.extend_from_slice(&4u32.to_be_bytes());
    image.extend_from_slice(b"BEAM");
    assert_bad_arg(load(&image), "empty chunk directory");
}

#[test]
fn rejects_a_stream_with_no_func_info() {
    // `group_functions` is public and must reject a stream that defines no function.
    assert_bad_arg(
        beam_loader::lower::group_functions(&[]),
        "empty instruction stream",
    );

    // A lone `int_code_end` (opcode 3) also has no `func_info`.
    let atoms = [Atom::MAX];
    let raw = decode_stream(&[0x03u8], &atoms, &version(), &TypeTable::default()).unwrap();
    assert_bad_arg(
        beam_loader::lower::group_functions(&raw),
        "stream without func_info",
    );
}

// ---- Malformed operands of the *widened* typed opcodes -------------------------
//
// The widened IR lowers several opcodes into typed variants whose operands have a
// structural shape (`z,1` argument lists, `z,3` allocation lists, integer counts).
// A stream whose operand carries a *valid tag* but the *wrong shape* must still be
// rejected by the lowering with a typed `BeamError::BadArg` — never a panic and
// never a wrong result. These cases drive the public decode → lower boundary: the
// decoder accepts the operand (the tag is well-formed), and the lowering rejects it.

/// Packs one `(tag, value)` operand into the file's tag/length byte plus magnitude
/// bytes (`beam_file.c:167-240`), mirroring the three encodings `src/decode.rs`
/// reads. Tags: `0`=`u` (integer), `2`=`a` (atom), `3`=`x`, `4`=`y`, `5`=`f`.
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

/// A well-formed single-function prologue: `label 1 | func_info 1 2 0 | label 2`.
///
/// The module uses the atom map `[NIL, atom 1, atom 2]`; a stream that continues
/// with a valid body and the [`epilogue`] is a loadable module, so a rejection can
/// only come from the instruction that follows the prologue.
fn prologue() -> Vec<u8> {
    let mut stream = Vec::new();
    stream.push(0x01); // `label`
    stream.extend(tagged(0, 1)); // `u`: label 1
    stream.push(0x02); // `func_info`
    stream.extend(tagged(2, 1)); // module atom index 1
    stream.extend(tagged(2, 2)); // function atom index 2
    stream.extend(tagged(0, 0)); // arity 0
    stream.push(0x01); // `label`
    stream.extend(tagged(0, 2)); // entry label 2
    stream
}

/// The stream terminator: `return` (opcode 19) then `int_code_end` (opcode 3).
fn epilogue() -> Vec<u8> {
    vec![0x13, 0x03]
}

/// Decodes `stream` and lowers it, asserting the *lowering* rejects it with
/// `BeamError::BadArg`.
///
/// The decoder itself must accept the stream (the operand's tag is well-formed);
/// the rejection comes from the operand's *shape* at lowering time.
fn assert_lowering_rejects(stream: &[u8], what: &str) {
    let atoms = [Atom::MAX, Atom::MAX, Atom::MAX];
    let raw = decode_stream(stream, &atoms, &version(), &TypeTable::default())
        .unwrap_or_else(|e| panic!("{what}: the decoder must accept the stream, got {e:?}"));
    assert_bad_arg(beam_loader::lower::group_functions(&raw), what);
}

#[test]
fn rejects_a_put_tuple2_whose_elements_are_not_a_list() {
    // `put_tuple2 Dst Elements` (opcode 164): the second operand must be a `z,1`
    // argument list; here it is the plain register `x1`.
    let mut stream = prologue();
    stream.push(0xA4); // `put_tuple2` (opcode 164)
    stream.extend(tagged(3, 0)); // Dst = x0
    stream.extend(tagged(3, 1)); // Elements = x1 — NOT a `z,1` list
    stream.extend(epilogue());
    assert_lowering_rejects(
        &stream,
        "put_tuple2 with a non-list element operand",
    );
}

#[test]
fn rejects_a_test_heap_whose_need_is_neither_integer_nor_allocation() {
    // `test_heap Need Live` (opcode 16): the need operand must be a `u` integer or
    // a `z,3` allocation list; here it is the plain register `x0`.
    let mut stream = prologue();
    stream.push(0x10); // `test_heap` (opcode 16)
    stream.extend(tagged(3, 0)); // Need = x0 — neither `u` nor `z,3`
    stream.extend(tagged(0, 4)); // Live = 4
    stream.extend(epilogue());
    assert_lowering_rejects(
        &stream,
        "test_heap with a non-integer, non-allocation need",
    );
}

#[test]
fn rejects_a_select_tuple_arity_whose_destinations_are_not_a_list() {
    // `select_tuple_arity Tuple Fail Destinations` (opcode 60): the destinations
    // must be a `z,1` argument list of `Arity,Label` pairs; here it is the plain
    // integer `1`.
    let mut stream = prologue();
    stream.push(0x3C); // `select_tuple_arity` (opcode 60)
    stream.extend(tagged(3, 0)); // Tuple = x0
    stream.extend(tagged(5, 1)); // Fail = f1
    stream.extend(tagged(0, 1)); // Destinations = 1 — NOT a `z,1` list
    stream.extend(epilogue());
    assert_lowering_rejects(
        &stream,
        "select_tuple_arity with non-list destinations",
    );
}
