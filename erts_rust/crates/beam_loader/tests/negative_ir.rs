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
//! Negative integration tests for the **instruction-stream / IR-lowering** checks.
//!
//! The legacy loader validates the assembled code, not just the chunk framing
//! (`erts/emulator/beam/jit/asm_load.c`): a defined `label` must lie inside the
//! `Code` header's label space and be defined once, the `func_info` count must equal
//! the header's `function_count`, a `func_info` arity must fit `MAX_ARG`, every
//! import index must resolve in `ImpT`, and every `Line`/`executable_line`/
//! `debug_line` location must index the line item table. Those checks are
//! accept/reject-only — the oracle's only Erlang-observable rejection is the bare
//! atom `badfile` — so each case here asserts a typed [`BeamError`], never a panic.
//!
//! Every case is a **byte mutation of a committed, `erlc`-produced fixture**
//! (`tests/fixtures/`, OTP 29 / `erts-17.1`) run through the public
//! [`beam_loader::load`] pipeline, paired with the *unmutated* image as the control
//! in the same test: only the field under test differs, so a rejection can never come
//! from a structural flaw in the image itself. The mutated field is located by
//! decoding the fixture's instruction stream (`decode::decode_stream`, which reports
//! each instruction's byte offset) rather than by hard-coding offsets, and every
//! patch asserts the bytes it replaces so a regenerated fixture fails loudly instead
//! of silently patching something else.
use beam_common::BeamError;
use beam_loader::chunk::Chunks;
use beam_loader::decode::{decode_stream, RawInstr};
use beam_loader::{load, Atom, Opcode};

/// `slice.beam`: 6 functions, labels 1..14, `label_count` 15, `function_count` 6,
/// a 4-entry `ImpT`, a 6-item `Line` table (the undefined location plus 5 lines) and
/// no `FunT`.
const SLICE: &[u8] = include_bytes!("fixtures/slice.beam");
/// `coverage.beam`: 12 functions, labels 1..37, `label_count` 38, `function_count` 12,
/// a 6-entry `ImpT`, a 24-item `Line` table, a one-entry `FunT` (label 37) and three
/// `LitT` literals. It is the fixture that carries `call_ext_only`, `bif` and
/// `gc_bif` — every external-call shape the import bound must cover.
const COVERAGE: &[u8] = include_bytes!("fixtures/coverage.beam");
/// `linecov.beam` (`+line_coverage`): 4 functions, labels 1..9, `label_count` 10, a
/// `Line` table of 7 items and `executable_line` (183) instructions.
const LINECOV: &[u8] = include_bytes!("fixtures/linecov.beam");

/// The `Code` chunk payload's header size: `head_size` (4) plus the four `u32` header
/// words it counts (`beam_file.c:1527-1530`).
const CODE_HEADER: usize = 20;

/// Asserts the load is rejected with `BeamError::BadArg` (and, implicitly, that
/// producing it did not panic).
fn assert_bad_arg<T: std::fmt::Debug>(result: Result<T, BeamError>, what: &str) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected BeamError::BadArg, got {other:?}"),
    }
}

/// Asserts `mutated` is rejected while the unmutated `control` still loads.
fn assert_rejected(control: &[u8], mutated: impl FnOnce() -> Vec<u8>, what: &str) {
    load(control).unwrap_or_else(|error| panic!("{what}: the control image must load: {error:?}"));
    let mutated = mutated();
    assert_ne!(
        mutated.as_slice(),
        control,
        "{what}: the mutation changed nothing"
    );
    assert_bad_arg(load(&mutated), what);
}

/// The container's chunks as `(id, payload)` pairs, in file order.
///
/// Walks the `FOR1`/`BEAM` chunk directory the way the loader's container parser
/// does, so the offsets stay correct if a fixture is regenerated.
fn chunks_of(image: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
    assert_eq!(&image[0..4], b"FOR1", "not a FOR1 image");
    assert_eq!(&image[8..12], b"BEAM", "not a BEAM form");
    let mut pos = 12usize;
    let mut chunks = Vec::new();
    while pos + 8 <= image.len() {
        let id: [u8; 4] = image[pos..pos + 4].try_into().expect("4-byte chunk id");
        let size = u32::from_be_bytes(
            image[pos + 4..pos + 8]
                .try_into()
                .expect("4-byte chunk size"),
        ) as usize;
        let payload = image[pos + 8..pos + 8 + size].to_vec();
        chunks.push((id, payload));
        pos += 8 + size + (4 - (size % 4)) % 4;
    }
    chunks
}

/// Re-emits a `FOR1`/`BEAM` image from `(id, payload)` pairs, recomputing every size,
/// the zero padding and the form length.
fn rewrap(chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, payload) in chunks {
        body.extend_from_slice(id);
        body.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("chunk fits")
                .to_be_bytes(),
        );
        body.extend_from_slice(payload);
        body.extend(std::iter::repeat_n(0u8, (4 - (payload.len() % 4)) % 4));
    }
    let mut image = Vec::with_capacity(body.len() + 12);
    image.extend_from_slice(b"FOR1");
    image.extend_from_slice(
        &u32::try_from(body.len() + 4)
            .expect("form fits")
            .to_be_bytes(),
    );
    image.extend_from_slice(b"BEAM");
    image.extend_from_slice(&body);
    image
}

/// Rebuilds `image` with its `Code` chunk payload rewritten by `patch`.
fn with_code(image: &[u8], patch: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut chunks = chunks_of(image);
    let code = chunks
        .iter_mut()
        .find(|(id, _)| id == b"Code")
        .expect("a Code chunk");
    patch(&mut code.1);
    rewrap(&chunks)
}

/// Rebuilds `image` with the chunk `id` replaced by `payload` (or appended when the
/// image carries none).
fn with_chunk(image: &[u8], id: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut chunks = chunks_of(image);
    match chunks.iter_mut().find(|(cid, _)| cid == id) {
        Some(chunk) => chunk.1 = payload,
        None => chunks.push((*id, payload)),
    }
    rewrap(&chunks)
}

/// The number of bytes one tagged operand occupies, from its tag/length byte.
///
/// The file's variable-length rule (`beam_file.c`'s `beamreader_read_tagged`): the low
/// three bits are the tag; `byte & 0x08 == 0` means the value is packed in the high
/// nibble (one byte), `byte & 0x10 == 0` adds a single big-endian tail byte (two), and
/// otherwise `(byte >> 5) + 2` magnitude bytes follow. The extended (multi-byte length)
/// form `beam >> 5 == 7` is not used by the operands patched here, so it panics rather
/// than returning a wrong length.
fn operand_len(byte: u8) -> usize {
    if byte & 0x08 == 0 {
        1
    } else if byte & 0x10 == 0 {
        2
    } else {
        let count = usize::from(byte >> 5) + 2;
        assert!(
            byte >> 5 < 7,
            "the extended operand length form is not supported by this helper"
        );
        count + 1
    }
}

/// The byte offsets of `instr`'s operands inside `stream` (relative to `stream`, which
/// must start at the `Code` chunk's instruction stream).
fn operand_offsets(stream: &[u8], instr: &RawInstr) -> Vec<usize> {
    let mut at = instr.offset as usize + 1; // the opcode byte
    let mut offsets = Vec::new();
    for _ in 0..instr.operands.len() {
        offsets.push(at);
        at += operand_len(stream[at]);
    }
    offsets
}

/// The decoded instruction stream of `image` plus the tables the checks range over.
struct Decoded {
    instrs: Vec<RawInstr>,
    label_count: u32,
    function_count: u32,
    import_count: usize,
    line_items: usize,
}

/// Decodes `image`'s `Code` stream the way [`beam_loader::load`] does.
fn decoded(image: &[u8]) -> Decoded {
    let container = beam_loader::container::parse(image).expect("a parsable container");
    let chunks: Chunks<'_> = beam_loader::chunk::parse(&container).expect("parsable chunks");
    let mut table = beam_term::AtomTable::new();
    let mut atoms: Vec<Atom> = vec![Atom::MAX];
    for name in &chunks.atoms[1..] {
        atoms.push(table.intern(name));
    }
    let instrs = decode_stream(
        chunks.code_stream,
        &atoms,
        &chunks.code_version,
        &chunks.types,
    )
    .expect("the fixture decodes");
    Decoded {
        instrs,
        label_count: chunks.code_version.label_count,
        function_count: chunks.code_version.function_count,
        import_count: chunks.imports.len(),
        line_items: chunks.line.map_or(0, |line| line.items.len()),
    }
}

/// The instruction stream bytes of `image`.
fn stream_of(image: &[u8]) -> Vec<u8> {
    let chunks = chunks_of(image);
    let code = &chunks
        .iter()
        .find(|(id, _)| id == b"Code")
        .expect("a Code chunk")
        .1;
    code[CODE_HEADER..].to_vec()
}

/// Patches the instruction stream: the bytes at `at` must be `expected` (a loud
/// failure if the fixture changed shape) and become `replacement`.
fn patch_stream(image: &[u8], at: usize, expected: &[u8], replacement: &[u8]) -> Vec<u8> {
    with_code(image, |code| {
        let start = CODE_HEADER + at;
        assert_eq!(
            &code[start..start + expected.len()],
            expected,
            "the fixture's bytes at stream offset {at} are not what this case patches"
        );
        code.splice(start..start + expected.len(), replacement.iter().copied());
    })
}

/// Patches one `u32` word of the `Code` header (`at` is relative to the payload).
fn patch_header_word(image: &[u8], at: usize, expected: u32, replacement: u32) -> Vec<u8> {
    with_code(image, |code| {
        let word = u32::from_be_bytes(code[at..at + 4].try_into().expect("4-byte word"));
        assert_eq!(
            word, expected,
            "the fixture's Code header at payload offset {at} is not what this case patches"
        );
        code[at..at + 4].copy_from_slice(&replacement.to_be_bytes());
    })
}

/// The byte offset of the `Code` header's `label_count` word.
const LABEL_COUNT_AT: usize = 12;
/// The byte offset of the `Code` header's `function_count` word.
const FUNCTION_COUNT_AT: usize = 16;

// ---- control ---------------------------------------------------------------

#[test]
fn the_unmutated_fixtures_load() {
    // The control every case below is paired with: all three fixtures are well-formed
    // and load through the public pipeline, so a rejection below can only come from
    // the mutated field.
    for (name, image) in [
        ("slice", SLICE),
        ("coverage", COVERAGE),
        ("linecov", LINECOV),
    ] {
        let module = load(image).unwrap_or_else(|error| panic!("{name} must load: {error:?}"));
        assert!(!module.functions.is_empty(), "{name} has functions");
    }
}

// ---- 1. label definitions --------------------------------------------------

#[test]
fn rejects_a_label_definition_of_zero() {
    // `asm_load.c:512-521` requires `0 < label < label_count`; label 0 is the "no
    // label" sentinel and can never be defined. `slice.beam`'s first instruction is
    // `label 1` (`u,1` = `0x10`), so its operand byte is stream offset 1.
    let stream = stream_of(SLICE);
    assert_eq!(
        stream[0],
        Opcode::label.as_u8(),
        "the first instruction is `label`"
    );
    assert_eq!(stream[1], 0x10, "the first label is 1 (the `u,1` form)");
    assert_eq!(decoded(SLICE).instrs[0].operands.len(), 1);
    assert_rejected(
        SLICE,
        || patch_stream(SLICE, 1, &[0x10], &[0x00]),
        "a label definition of 0",
    );
}

#[test]
fn rejects_a_label_definition_at_the_label_count() {
    // `asm_load.c:515-521` ("invalid label num %u (0 < label < %u)"): a definition must
    // be inside the declared label space. `slice.beam` declares `label_count` 15 and
    // defines labels 1..14, so shrinking the header to 11 makes label 11 — the first
    // out-of-range definition, `label == label_count` — invalid. The largest `f`
    // operand in the fixture is `f,10`, so the decoder's own `f`-operand bound
    // (`beam_file.c:2324-2331`) cannot fire first: this rejection is the definition
    // check.
    let info = decoded(SLICE);
    assert_eq!(info.label_count, 15);
    assert_rejected(
        SLICE,
        || patch_header_word(SLICE, LABEL_COUNT_AT, 15, 11),
        "a label definition at label_count",
    );
}

#[test]
fn rejects_a_duplicate_label_definition() {
    // `asm_load.c:523-528` ("label %d defined more than once"): one label array per
    // module. `slice.beam`'s third instruction is `label 2` (`u,2` = `0x20`) at stream
    // offset 8; redefining it as `label 1` collides with the module's first label.
    let info = decoded(SLICE);
    let second_label = info
        .instrs
        .iter()
        .filter(|instr| instr.opcode == Opcode::label)
        .nth(1)
        .expect("slice.beam defines more than one label");
    let at = second_label.offset as usize;
    let stream = stream_of(SLICE);
    assert_eq!((stream[at], stream[at + 1]), (Opcode::label.as_u8(), 0x20));
    assert_rejected(
        SLICE,
        || patch_stream(SLICE, at + 1, &[0x20], &[0x10]),
        "a duplicate label definition",
    );
}

// ---- 2. function_count -----------------------------------------------------

#[test]
fn rejects_a_function_count_below_the_func_info_count() {
    // `asm_load.c:625-629` ("too many functions in module (header said %u)"): a
    // `func_info` past `function_count`. `slice.beam` has 6 functions; a header of 5
    // makes the sixth one the overrun.
    let info = decoded(SLICE);
    assert_eq!(info.function_count, 6);
    assert_rejected(
        SLICE,
        || patch_header_word(SLICE, FUNCTION_COUNT_AT, 6, 5),
        "function_count below the func_info count",
    );
}

#[test]
fn rejects_a_function_count_above_the_func_info_count() {
    // `asm_load.c:737-743` ("too few functions (%u) in module (header said %u)"): the
    // count is checked again at `int_code_end`.
    let info = decoded(SLICE);
    assert_eq!(info.function_count, 6);
    assert_rejected(
        SLICE,
        || patch_header_word(SLICE, FUNCTION_COUNT_AT, 6, 7),
        "function_count above the func_info count",
    );
}

// ---- 3. func_info arity ----------------------------------------------------

#[test]
fn rejects_a_func_info_arity_above_the_runtime_maximum() {
    // `asm_load.c:636-639` ("too many arguments: %d"): `arity <= MAX_ARG` (255,
    // `erl_vm.h:48`). `slice.beam`'s first `func_info` declares arity 2 as the one-byte
    // `u,2` form (`0x20`); 300 needs the two-byte form (`0x29 0x2c`), so the stream
    // grows by one byte and the container is re-emitted with the new sizes.
    let info = decoded(SLICE);
    let func_info = info
        .instrs
        .iter()
        .find(|instr| instr.opcode == Opcode::func_info)
        .expect("a func_info");
    let arity_at = *operand_offsets(&stream_of(SLICE), func_info)
        .last()
        .expect("func_info has an arity operand");
    assert_eq!(info.label_count, 15, "the rest of the image is untouched");
    // An arity bound is a size bound, so this crate reports it as `SystemLimit` (the
    // `BeamError::BadArg` of the oracle's `badfile` is the class for structural damage).
    assert!(
        matches!(
            load(&patch_stream(SLICE, arity_at, &[0x20], &[0x29, 0x2C])),
            Err(BeamError::SystemLimit)
        ),
        "a func_info arity of 300 must be rejected as SystemLimit"
    );
    // The same two-byte encoding with a legal value loads: the rejection above is the
    // arity bound (300), not the wider operand form — 16 is `0x08 0x10` (a zero high
    // byte and the tail), 300 is `0x28 0x2c` (`(300 >> 8) << 5 | 0x08`, tail `300 & 0xff`).
    let arity_16 = patch_stream(SLICE, arity_at, &[0x20], &[0x08, 0x10]);
    load(&arity_16).expect("an arity of 16 must load");
}

// ---- 4. import-table indices ----------------------------------------------

#[test]
fn rejects_an_import_index_past_the_import_table() {
    // `asm_load.c:532-548` ("invalid import table index %d") for the `e`/`b` operand
    // types. `slice.beam`'s `gc_bif2` names import 0 of a 4-entry `ImpT`; naming entry
    // 4 — the first index past the table — must be a load error rather than a later
    // execution error in `beam_engine` (`Module::imports.get` ⇒ `None`).
    let info = decoded(SLICE);
    assert_eq!(info.import_count, 4);
    let gc_bif = info
        .instrs
        .iter()
        .find(|instr| instr.opcode == Opcode::gc_bif2)
        .expect("a gc_bif2");
    let stream = stream_of(SLICE);
    // `gc_bif2 Fail Live Bif Arg… Dst` — the import index is operand 2.
    let bif_at = operand_offsets(&stream, gc_bif)[2];
    assert_eq!(stream[bif_at], 0x00, "the fixture's first BIF is import 0");
    assert_rejected(
        SLICE,
        || patch_stream(SLICE, bif_at, &[0x00], &[0x40]),
        "an import index at the import table's length",
    );
}

#[test]
fn rejects_every_external_call_when_the_import_table_is_empty() {
    // The same bound seen from the other side: `coverage.beam` calls through
    // `call_ext_only`, `bif` and `gc_bif2`, so an empty `ImpT` makes every one of those
    // operands — and only those — out of range. The rest of the image stays untouched.
    let info = decoded(COVERAGE);
    assert_eq!(info.import_count, 6);
    assert!(info
        .instrs
        .iter()
        .any(|instr| instr.opcode == Opcode::call_ext_only));
    assert_bad_arg(
        load(&with_chunk(COVERAGE, b"ImpT", 0u32.to_be_bytes().to_vec())),
        "an empty import table",
    );
}

// ---- 5. line-table item indices -------------------------------------------

#[test]
fn rejects_a_line_index_past_the_line_table() {
    // `add_line_entry` (`asm_load.c:324-341`, "line instruction index overflow
    // (%u/%u)"): a `line` operand must index the line item table. `slice.beam`'s first
    // `line` instruction (stream offset 2) names item 1 of 6; 15 is past the table.
    let info = decoded(SLICE);
    assert_eq!(info.line_items, 6);
    let line = info
        .instrs
        .iter()
        .find(|instr| instr.opcode == Opcode::line)
        .expect("a line instruction");
    let at = line.offset as usize + 1;
    assert_eq!(
        stream_of(SLICE)[at],
        0x10,
        "the fixture's first line is item 1"
    );
    assert_rejected(
        SLICE,
        || patch_stream(SLICE, at, &[0x10], &[0xF0]),
        "a line index past the line table",
    );
}

#[test]
fn rejects_an_executable_line_index_past_the_line_table() {
    // The same bound through the `executable_line` case (`asm_load.c:666-733` reaches
    // `add_line_entry` for both `line` and `debug_line`; `+line_coverage` emits 183).
    // `linecov.beam`'s line table has 7 items, so item 7 is the first invalid one.
    let info = decoded(LINECOV);
    assert_eq!(info.line_items, 7);
    let executable = info
        .instrs
        .iter()
        .find(|instr| instr.opcode == Opcode::executable_line)
        .expect("an executable_line");
    let stream = stream_of(LINECOV);
    let location_at = operand_offsets(&stream, executable)[0];
    assert_eq!(
        stream[location_at], 0x10,
        "the fixture's first location is item 1"
    );
    assert_rejected(
        LINECOV,
        || patch_stream(LINECOV, location_at, &[0x10], &[0x70]),
        "an executable_line index at the line table's length",
    );
}

// ---- 6. FunT lambda table --------------------------------------------------

/// The committed `coverage.beam` `FunT` chunk payload, as `(count, entries)`.
fn funt_parts() -> (u32, Vec<u8>) {
    let chunks = chunks_of(COVERAGE);
    let payload = &chunks
        .iter()
        .find(|(id, _)| id == b"FunT")
        .expect("a FunT chunk")
        .1;
    let count = u32::from_be_bytes(payload[0..4].try_into().expect("4-byte count"));
    (count, payload[4..].to_vec())
}

#[test]
fn rejects_two_fun_descriptors_naming_the_same_label() {
    // `asm_load.c:150-165` ("lambda already defined for label %i"): the C keeps one
    // `lambda_index` slot per label, so a second `FunT` entry for the same label is
    // malformed. `coverage.beam`'s single entry (label 37) is duplicated.
    let (count, entries) = funt_parts();
    assert_eq!(count, 1);
    assert_eq!(entries.len(), 24, "one 24-byte FunT entry");
    let mut payload = 2u32.to_be_bytes().to_vec();
    payload.extend_from_slice(&entries);
    payload.extend_from_slice(&entries);
    assert_bad_arg(
        load(&with_chunk(COVERAGE, b"FunT", payload)),
        "two FunT entries sharing a label",
    );
}

#[test]
fn rejects_a_fun_descriptor_with_a_zero_label() {
    // `beam_file.c:429`'s `LoadAssert(label > 0 && label < label_count)`: label 0 is
    // the "no label" sentinel, never a lambda body. The `FunT` entry's `label` field is
    // the third word (`function`, `arity`, `label`, `index`, `num_free`, `old_uniq`).
    let (count, entries) = funt_parts();
    assert_eq!(count, 1);
    assert_eq!(
        u32::from_be_bytes(entries[8..12].try_into().expect("4-byte label")),
        37,
        "the fixture's lambda body is label 37"
    );
    let mut broken = entries.clone();
    broken[8..12].copy_from_slice(&0u32.to_be_bytes());
    let mut payload = count.to_be_bytes().to_vec();
    payload.extend_from_slice(&broken);
    assert_rejected(
        COVERAGE,
        || with_chunk(COVERAGE, b"FunT", payload),
        "a FunT entry labelling label 0",
    );
}
