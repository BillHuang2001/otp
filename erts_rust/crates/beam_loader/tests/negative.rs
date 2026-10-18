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
//! [`beam_loader::decode::decode_stream`] with crafted byte streams, and the
//! structural-rejection cases at the end run the whole pipeline over a hand-built
//! image paired with a well-formed control.
use beam_common::BeamError;
use beam_loader::decode::decode_stream;
use beam_loader::{load, Atom, CodeVersion, Opcode, TypeTable};

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
///
/// `label_count` bounds every non-zero `f` operand (`beam_file.c:2324-2331`), so it is
/// set well above the handful of labels the crafted prologues use (1, 2, …) — a stream
/// is then rejected for the *intended* reason, never merely for its label index. The
/// dedicated label-bound case builds its own tighter header.
fn version() -> CodeVersion {
    CodeVersion {
        format_number: 0,
        max_opcode: 181,
        label_count: 16,
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
    // This drives the *decoder* with a hand-built version directly: `max_opcode` 200
    // admits the byte, so the rejection can only come from the opcode roster (the
    // roster stops at 193, and 194 is not a `genop.tab` opcode). A real file can no
    // longer declare such a header at all — `chunk::code::parse_header` refuses
    // anything outside `[169, 193]` (see the whole-image cases below).
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
/// reads. Tags: `0`=`u` (integer), `2`=`a` (atom), `3`=`x`, `4`=`y`, `5`=`f`,
/// `7`=`z` (whose payload is the extended sub-tag).
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

// ---- Structural rejection: whole-image cases ----------------------------------
//
// Every case below runs the *whole* `load` pipeline over a hand-built `FOR1`/`BEAM`
// image and is paired with a well-formed **control** image built by the very same
// helper — only the bytes under test differ — so a rejection can never come from a
// structural flaw in the image itself, and the control proves the harness works.

/// Appends one `FOR1`/`BEAM` chunk (id, size, payload, zero padding).
fn push_chunk(body: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    body.extend_from_slice(id);
    body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    body.extend_from_slice(payload);
    body.extend(std::iter::repeat(0u8).take((4 - payload.len() % 4) % 4));
}

/// Wraps `body` in a `FOR1`/`BEAM` container.
fn wrap(body: Vec<u8>) -> Vec<u8> {
    let mut image = Vec::new();
    image.extend_from_slice(b"FOR1");
    image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    image.extend_from_slice(b"BEAM");
    image.extend_from_slice(&body);
    image
}

/// A minimal, otherwise-valid module image whose `Code` chunk carries `body`.
///
/// The instruction stream is `label 1; func_info m f 0; label 2; <body>; return;
/// int_code_end`, so `lower::group_functions` finds exactly one `m:f/0` function and
/// `AtU8` holds the two atoms it needs (`m` at wire index 1, `f` at index 2). The
/// `Code` header takes `max_opcode` / `label_count`; `extras` appends further chunks
/// (`Type`, `LitT`, …) verbatim. Only the bytes a case varies differ between it and
/// its control, so each rejection comes from the operand (or header field) under test.
fn image_with_body(
    body: &[u8],
    max_opcode: u32,
    label_count: u32,
    extras: &[(&[u8; 4], &[u8])],
) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&16u32.to_be_bytes()); // head_size
    code.extend_from_slice(&0u32.to_be_bytes()); // format_number
    code.extend_from_slice(&max_opcode.to_be_bytes());
    code.extend_from_slice(&label_count.to_be_bytes());
    code.extend_from_slice(&1u32.to_be_bytes()); // function_count
    code.extend_from_slice(&[0x01, 0x10]); // `label 1`
    code.extend_from_slice(&[0x02, 0x12, 0x22, 0x00]); // `func_info m f 0`
    code.extend_from_slice(&[0x01, 0x20]); // `label 2` (the entry label)
    code.extend_from_slice(body);
    code.extend_from_slice(&[0x13, 0x03]); // `return`, `int_code_end`

    let mut atoms = Vec::new();
    atoms.extend_from_slice(&2u32.to_be_bytes()); // count = 2
    for name in ["m", "f"] {
        atoms.push(name.len() as u8);
        atoms.extend_from_slice(name.as_bytes());
    }

    let mut chunks = Vec::new();
    push_chunk(&mut chunks, b"AtU8", &atoms);
    push_chunk(&mut chunks, b"Code", &code);
    // `ImpT`/`ExpT` are mandatory chunks (`beam_load.c:155-169`); empty tables keep the
    // image structurally complete so each case fails for the operand under test.
    push_chunk(&mut chunks, b"ImpT", &0u32.to_be_bytes());
    push_chunk(&mut chunks, b"ExpT", &0u32.to_be_bytes());
    for (id, payload) in extras {
        push_chunk(&mut chunks, id, payload);
    }
    wrap(chunks)
}

/// Asserts the whole pipeline rejects `body` (inside [`image_with_body`]) for the
/// intended reason, and that the *same image* carrying `control` genuinely loads.
fn assert_image_rejected(
    body: &[u8],
    control: &[u8],
    max_opcode: u32,
    label_count: u32,
    extras: &[(&[u8; 4], &[u8])],
    what: &str,
) {
    load(&image_with_body(control, max_opcode, label_count, extras)).unwrap_or_else(
        |error| panic!("{what}: the control image must load, got {error:?}"),
    );
    assert_bad_arg(
        load(&image_with_body(body, max_opcode, label_count, extras)),
        what,
    );
}

/// A `z,2` float-register operand.
fn float_reg(index: u64) -> Vec<u8> {
    let mut out = tagged(7, 2);
    out.extend(tagged(0, index));
    out
}

/// A `z,5` `tr` operand: an `x` register plus its `Type` chunk index.
fn typed_reg(reg: u64, ty: u64) -> Vec<u8> {
    let mut out = tagged(7, 5);
    out.extend(tagged(3, reg));
    out.extend(tagged(0, ty));
    out
}

/// A `z,3` allocation list of `(kind, number)` entries.
fn alloc_list(entries: &[(u64, u64)]) -> Vec<u8> {
    let mut out = tagged(7, 3);
    out.extend(tagged(0, entries.len() as u64));
    for (kind, number) in entries {
        out.extend(tagged(0, *kind));
        out.extend(tagged(0, *number));
    }
    out
}

/// A `z,1` argument list with the given sub-operand byte strings.
fn arg_list(items: &[&[u8]]) -> Vec<u8> {
    let mut out = tagged(7, 1);
    out.extend(tagged(0, items.len() as u64));
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// A version-4 `Type` chunk holding one `"any type"` entry (count 1).
fn single_type_table() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&4u32.to_be_bytes()); // version 4
    out.extend_from_slice(&1u32.to_be_bytes()); // count 1
    out.extend_from_slice(&0x1FFFu16.to_be_bytes()); // "any type"
    out
}

/// A verbatim `LitT` chunk (`uncompressed_size 0`) holding `literals` records.
fn litt(literals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u32.to_be_bytes()); // verbatim records follow
    out.extend_from_slice(&(literals.len() as i32).to_be_bytes());
    for literal in literals {
        out.extend_from_slice(&(literal.len() as i32).to_be_bytes());
        out.extend_from_slice(literal);
    }
    out
}

#[test]
fn rejects_a_max_opcode_above_the_roster_in_a_whole_image() {
    // `beam_load.c:189-196`: a header above the roster's last opcode means the file was
    // built for a later runtime than this one. The control is the committed fixture
    // (its header declares 181), which loads.
    load(FIXTURE).expect("the fixture itself is a well-formed image");
    for max_opcode in [194u32, 200, 255, u32::MAX] {
        let mut image = FIXTURE.to_vec();
        // `Code` payload: head_size, format_number, max_opcode, …
        let payload = chunk_payload_offset(&image, b"Code");
        image[payload + 8..payload + 12].copy_from_slice(&max_opcode.to_be_bytes());
        assert_bad_arg(load(&image), "max_opcode above the roster");
    }
    // Control: the roster's own last opcode (193) is inside the window, and the
    // fixture's stream only carries bytes well below it.
    let mut image = FIXTURE.to_vec();
    let payload = chunk_payload_offset(&image, b"Code");
    image[payload + 8..payload + 12].copy_from_slice(&193u32.to_be_bytes());
    load(&image).expect("max_opcode 193 is the highest accepted value");
}

#[test]
fn rejects_a_max_opcode_below_swap_in_a_whole_image() {
    // `beam_load.c:197-225`: OTP 23–26 set `max_opcode` to exactly `swap/2`, so a
    // smaller value is an OTP 22 (or earlier) file the current loader refuses.
    let body = [0x13u8, 0x03]; // `return`, `int_code_end`
    // Control: the same minimal image declaring `swap/2` (169) — the lowest accepted
    // value — loads; its own instructions are all far below it.
    load(&image_with_body(&body, 169, 2, &[])).expect("max_opcode 169 must load");
    for max_opcode in [0u32, 1, 168] {
        assert_bad_arg(
            load(&image_with_body(&body, max_opcode, 2, &[])),
            "max_opcode below swap/2",
        );
    }
}

#[test]
fn rejects_an_out_of_range_label_index() {
    // `is_eq_exact/3` (43) `Fail Arg1 Arg2`: `LoadAssert(raw_arg.word_value <
    // code->label_count)` for every non-zero `f` operand (`beam_file.c:2324-2331`).
    let op = Opcode::is_eq_exact.as_u8();
    let mut body = vec![op];
    body.extend(tagged(5, 2)); // Fail = `f,2` — NOT below `label_count` 2
    body.extend(tagged(3, 0));
    body.extend(tagged(3, 0));
    let mut control = vec![op];
    control.extend(tagged(5, 1)); // Fail = `f,1` — a label the image defines
    control.extend(tagged(3, 0));
    control.extend(tagged(3, 0));
    assert_image_rejected(&body, &control, 181, 2, &[], "label index out of range");
}

#[test]
fn rejects_an_out_of_range_x_register_index() {
    // `move/2` (64) `Src Dst`: `x,1024` is exactly `MAX_REG` (`erl_vm.h:49`) and
    // cannot exist (`beam_file.c:2335-2338`).
    let op = Opcode::r#move.as_u8();
    let mut body = vec![op];
    body.extend(tagged(3, 1024));
    body.extend(tagged(3, 0));
    let mut control = vec![op];
    control.extend(tagged(3, 1023)); // the highest legal register index
    control.extend(tagged(3, 0));
    assert_image_rejected(&body, &control, 181, 2, &[], "x register out of range");
}

#[test]
fn rejects_an_out_of_range_y_register_index() {
    // A `y` operand carries the same bound as `x` (`beam_file.c:2335-2338`).
    let op = Opcode::r#move.as_u8();
    let mut body = vec![op];
    body.extend(tagged(3, 0));
    body.extend(tagged(4, 1024));
    let mut control = vec![op];
    control.extend(tagged(3, 0));
    control.extend(tagged(4, 1023));
    assert_image_rejected(&body, &control, 181, 2, &[], "y register out of range");
}

#[test]
fn rejects_an_out_of_range_float_register_index() {
    // `fmove/2` (96) `Src Dst` with `z,2` float registers, bounded by the same
    // `MAX_REG` (`beam_file.c:2381`).
    let op = Opcode::fmove.as_u8();
    let mut body = vec![op];
    body.extend(float_reg(1024));
    body.extend(float_reg(0));
    let mut control = vec![op];
    control.extend(float_reg(1023));
    control.extend(float_reg(0));
    assert_image_rejected(
        &body,
        &control,
        181,
        2,
        &[],
        "float register out of range",
    );
}

#[test]
fn rejects_an_out_of_range_type_index() {
    // `z,5` `tr` names a `Type` chunk entry: with the chunk present (no fallback) the
    // index must be `< count` (`beam_file.c:2436-2437`). Both images carry the same
    // one-entry `Type` table, so only the index differs.
    let types = single_type_table();
    let op = Opcode::r#move.as_u8();
    let mut body = vec![op];
    body.extend(typed_reg(0, 1)); // type index 1 — past the single entry
    body.extend(tagged(3, 1));
    let mut control = vec![op];
    control.extend(typed_reg(0, 0)); // type index 0 — the "any type" entry
    control.extend(tagged(3, 1));
    assert_image_rejected(
        &body,
        &control,
        181,
        2,
        &[(b"Type", types.as_slice())],
        "type index out of range",
    );
}

#[test]
fn rejects_an_allocation_list_longer_than_the_four_kinds() {
    // `LoadAssert(count.word_value <= 4)` (`beam_file.c:2241`): a `z,3` list carries at
    // most one entry per allocation kind. `test_heap/2` (16) `Need Live`.
    let op = Opcode::test_heap.as_u8();
    let mut body = vec![op];
    body.extend(alloc_list(&[(0, 1), (0, 1), (0, 1), (0, 1), (0, 1)])); // five entries
    body.extend(tagged(0, 1)); // Live
    let mut control = vec![op];
    control.extend(alloc_list(&[(0, 1), (1, 1), (2, 1), (3, 1)])); // exactly the four kinds
    control.extend(tagged(0, 1));
    assert_image_rejected(&body, &control, 181, 2, &[], "allocation list too long");
}

#[test]
fn rejects_an_argument_list_that_is_not_the_last_operand() {
    // `LoadAssert((i + 1) == arity)` (`beam_file.c:2352`): a `z,1` argument list is
    // always the instruction's *last* operand. `put_tuple2/2` (164) `Dst Elements` —
    // the control puts the list in the `Elements` slot, the negative puts it in `Dst`.
    let op = Opcode::put_tuple2.as_u8();
    let mut body = vec![op];
    body.extend(arg_list(&[&tagged(3, 0)])); // the list in the FIRST of two slots
    body.extend(tagged(3, 1));
    let mut control = vec![op];
    control.extend(tagged(3, 0)); // `Dst = x0`
    control.extend(arg_list(&[&tagged(3, 1)])); // `Elements` — the last operand
    assert_image_rejected(&body, &control, 181, 2, &[], "argument list not last");
}

#[test]
fn rejects_a_static_literal_index_the_file_does_not_define() {
    // The `z,4` index is `LoadAssert(index.word_value < literals->count)` against the
    // file's own `LitT` table (`beam_file.c:2400-2417`): a reference the file does not
    // define "must not have been created at runtime". The bound is enforced by
    // `lower::build_module`, which owns the literal table. Both images carry the same
    // one-entry `LitT`, so only the index differs — and the literal is materialized by
    // `LiteralArea`, so the control genuinely resolves it.
    let literals = litt(&[&[131, 97, 0]]); // `SMALL_INTEGER_EXT 0`
    let op = Opcode::r#move.as_u8();
    let mut body = vec![op];
    body.extend(tagged(7, 4));
    body.extend(tagged(0, 1)); // literal index 1 — only index 0 exists
    body.extend(tagged(3, 1));
    let mut control = vec![op];
    control.extend(tagged(7, 4));
    control.extend(tagged(0, 0)); // literal index 0
    control.extend(tagged(3, 1));
    assert_image_rejected(
        &body,
        &control,
        181,
        2,
        &[(b"LitT", literals.as_slice())],
        "static literal index out of range",
    );
}
