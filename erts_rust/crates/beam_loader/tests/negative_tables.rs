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

//! Negative coverage of the **chunk-table** and **container** validations — the
//! structural checks the legacy C reader applies before any instruction is decoded.
//!
//! Every case here is accept/reject-only: the oracle's sole Erlang-observable rejection
//! is the bare atom `badfile` (`code:load_binary/3`), so a structurally invalid table is
//! `BeamError::BadArg` (via `crate::error::bad_file`/`truncated`) and the crate's
//! documented scaling convention for arity/size bounds is `BeamError::SystemLimit`.
//!
//! Each test drives the **public** `load()` over a mutated copy of the committed oracle
//! fixture `tests/fixtures/slice.beam` and asserts, in the same test, that the
//! *unmutated* (or equivalently rebuilt) image still loads — so a rejection can never be
//! an artefact of the mutation harness. The mutations either patch a fixed-width `i32`
//! field in place (same length) or rebuild the chunk directory from the fixture's own
//! chunks with one payload replaced/removed.
//!
//! Ground truth (read-only): `erts/emulator/beam/beam_file.c` (the chunk readers and the
//! 32-bit-bounded chunk walk), `erts/emulator/beam/beam_load.c:141-187` (the mandatory
//! chunks), `erts/emulator/beam/atom.c:283-335` (the atom name caps) and
//! `erts/emulator/beam/beam_types.c:44-82` (the type-word rejections).

use beam_common::BeamError;
use beam_loader::container;
use beam_loader::load;

/// Which error class a mutated image must produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expected {
    /// A structurally invalid image: the BEAM `badarg` atom, the oracle's `badfile`.
    BadArg,
    /// A hard arity/size bound: this crate's `system_limit` convention.
    SystemLimit,
}

/// The committed oracle fixture (a real `.beam` produced by the system `erlc`).
fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/slice.beam"
    ))
    .expect("read tests/fixtures/slice.beam")
}

/// The fixture's chunk directory as owned `(id, payload)` pairs, in file order.
fn fixture_chunks() -> Vec<([u8; 4], Vec<u8>)> {
    let bytes = fixture();
    let container = container::parse(&bytes).expect("the fixture's container parses");
    container
        .chunks
        .iter()
        .map(|chunk| (chunk.id.name(), chunk.data.to_vec()))
        .collect()
}

/// Serializes a chunk directory into a `FOR1`/`BEAM` image, padding payloads to the
/// 4-byte boundary the wire format requires.
fn build(chunks: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, payload) in chunks {
        body.extend_from_slice(id);
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(payload);
        body.extend(std::iter::repeat_n(0u8, (4 - payload.len() % 4) % 4));
    }
    let mut image = Vec::new();
    image.extend_from_slice(b"FOR1");
    image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    image.extend_from_slice(b"BEAM");
    image.extend_from_slice(&body);
    image
}

/// The fixture image with `id`'s payload replaced (or appended when absent).
fn with_chunk(chunks: &[([u8; 4], Vec<u8>)], id: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut out = chunks.to_vec();
    match out.iter_mut().find(|(cid, _)| cid == id) {
        Some(slot) => slot.1 = payload,
        None => out.push((*id, payload)),
    }
    build(&out)
}

/// The fixture image with `id`'s chunk removed entirely.
fn without_chunk(chunks: &[([u8; 4], Vec<u8>)], id: &[u8; 4]) -> Vec<u8> {
    let kept: Vec<([u8; 4], Vec<u8>)> = chunks
        .iter()
        .filter(|(cid, _)| cid != id)
        .cloned()
        .collect();
    assert_eq!(kept.len(), chunks.len() - 1, "the chunk to remove is present");
    build(&kept)
}

/// The byte offset of `id`'s payload inside `image` (the first occurrence), for in-place
/// patches that keep the image's length unchanged.
fn payload_offset(image: &[u8], id: &[u8; 4]) -> usize {
    let mut pos = 12usize; // FOR1 + form_length + BEAM
    while pos + 8 <= image.len() {
        let size = u32::from_be_bytes(image[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if &image[pos..pos + 4] == id {
            return pos + 8;
        }
        pos += 8 + size + (4 - size % 4) % 4;
    }
    panic!("chunk {id:?} not found in the fixture");
}

/// Asserts `control` loads and the `mutated` image fails with `expected`.
fn assert_rejects(control: &[u8], mutated: &[u8], expected: Expected, what: &str) {
    load(control).unwrap_or_else(|error| {
        panic!("{what}: the control image must load, got {error:?}");
    });
    match load(mutated) {
        Err(BeamError::BadArg) if expected == Expected::BadArg => {}
        Err(BeamError::SystemLimit) if expected == Expected::SystemLimit => {}
        other => panic!("{what}: expected {expected:?}, got {other:?}"),
    }
}

/// Packs a value as the `u`-tagged number the `AtU8` long form uses
/// (`beamreader_read_tagged`, `beam_file.c:167-240`).
fn tagged_length(value: u64) -> Vec<u8> {
    if value <= 15 {
        return vec![(value as u8) << 4];
    }
    if value < 0x800 {
        return vec![0x08 | (((value >> 8) as u8) << 5), value as u8];
    }
    let count = ((64 - value.leading_zeros()) as usize + 7) / 8;
    let mut out = vec![0x18 | (((count - 2) as u8) << 5)];
    out.extend_from_slice(&value.to_be_bytes()[8 - count..]);
    out
}

#[test]
fn the_reassembled_fixture_is_byte_identical() {
    // The mutation harness must be faithful: rebuilding the fixture from its own chunks
    // reproduces the file exactly, so every mutation below differs from the control only
    // in the field under test.
    assert_eq!(build(&fixture_chunks()), fixture());
    load(&fixture()).expect("the fixture loads");
}

// ---- The mandatory chunks (`beam_load.c:141-187`) -----------------------------

#[test]
fn a_missing_import_table_is_rejected() {
    // `MISSING_IMPORT_TABLE` (`beam_load.c:155-169`): `ImpT` is mandatory even for a
    // module that imports nothing (the chunk is then present with a zero count).
    //
    // The control is the untouched fixture — equivalently its faithful rebuild, pinned by
    // `the_reassembled_fixture_is_byte_identical`. A zero-count `ImpT` cannot stand in for
    // the real one: the loader resolves every `ImportRef` in the lowered code against
    // `Module::imports` (`jit/asm_load.c:532-548`) and `slice.beam`'s code does reference
    // imports, so an emptied table is itself rejected.
    let chunks = fixture_chunks();
    assert_rejects(
        &build(&chunks),
        &without_chunk(&chunks, b"ImpT"),
        Expected::BadArg,
        "missing ImpT",
    );
}

#[test]
fn a_missing_export_table_is_rejected() {
    // `MISSING_EXPORT_TABLE` (`beam_load.c:155-169`): `ExpT` is what makes a module
    // loadable, so its absence is refused rather than treated as "no exports".
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"ExpT", 0u32.to_be_bytes().to_vec());
    assert_rejects(
        &control,
        &without_chunk(&chunks, b"ExpT"),
        Expected::BadArg,
        "missing ExpT",
    );
}

// ---- Arity and label bounds (`beam_file.c:335,373-374`) -----------------------

#[test]
fn an_export_arity_above_max_arg_is_rejected() {
    // `LoadAssert(arity >= 0 && arity <= MAX_ARG)` (`beam_file.c:373`), `MAX_ARG` 255
    // (`erl_vm.h:48`). The `ExpT` entry is `{function, arity, label}`.
    let image = fixture();
    let payload = payload_offset(&image, b"ExpT");
    let mut mutated = image.clone();
    mutated[payload + 8..payload + 12].copy_from_slice(&256u32.to_be_bytes());
    assert_rejects(
        &image,
        &mutated,
        Expected::SystemLimit,
        "export arity 256",
    );

    // The control bound itself is legal: arity 255 loads (only the table entry changes;
    // nothing references this export).
    let mut at_limit = image.clone();
    at_limit[payload + 8..payload + 12].copy_from_slice(&255u32.to_be_bytes());
    load(&at_limit).expect("an export arity of MAX_ARG is accepted");
}

#[test]
fn a_negative_export_arity_is_rejected() {
    // The wire field is a signed `i32`, so a negative arity is representable and must be
    // refused (`arity >= 0`, `beam_file.c:373`).
    let image = fixture();
    let payload = payload_offset(&image, b"ExpT");
    let mut mutated = image.clone();
    mutated[payload + 8..payload + 12].copy_from_slice(&(-1i32).to_be_bytes());
    assert_rejects(&image, &mutated, Expected::SystemLimit, "export arity -1");
}

#[test]
fn a_negative_export_label_is_rejected() {
    // `LoadAssert(label >= 0)` (`beam_file.c:374`): the wire `0xFFFFFFFF` must never
    // become `Export { label: Label(4294967295) }`.
    let image = fixture();
    let payload = payload_offset(&image, b"ExpT");
    let mut mutated = image.clone();
    mutated[payload + 12..payload + 16].copy_from_slice(&(-1i32).to_be_bytes());
    assert_rejects(&image, &mutated, Expected::BadArg, "export label -1");
}

#[test]
fn an_import_arity_above_max_arg_is_rejected() {
    // The same bound on `ImpT` (`beam_file.c:335`); the entry is
    // `{module_atom, function_atom, arity}`.
    let image = fixture();
    let payload = payload_offset(&image, b"ImpT");
    let mut mutated = image.clone();
    mutated[payload + 12..payload + 16].copy_from_slice(&256u32.to_be_bytes());
    assert_rejects(
        &image,
        &mutated,
        Expected::SystemLimit,
        "import arity 256",
    );

    let mut negative = fixture();
    let payload = payload_offset(&negative, b"ImpT");
    negative[payload + 12..payload + 16].copy_from_slice(&(-1i32).to_be_bytes());
    assert_rejects(&image, &negative, Expected::SystemLimit, "import arity -1");
}

#[test]
fn a_local_table_arity_is_bounded_like_an_export() {
    // `LocT` reuses the export-table reader (`parse_export_table`,
    // `beam_file.c:391-395`), so its arity carries the same bound. The fixture's single
    // local entry `{fib, 3, 6}` sits at payload + 4.
    let image = fixture();
    let payload = payload_offset(&image, b"LocT");
    let mut mutated = image.clone();
    mutated[payload + 8..payload + 12].copy_from_slice(&256u32.to_be_bytes());
    assert_rejects(&image, &mutated, Expected::SystemLimit, "LocT arity 256");
}

// ---- The line table (`beam_file.c:519-522`) ----------------------------------

/// A `Line` chunk payload: `version, flags, instr_count, item_count, name_count`, then
/// the raw item stream (no file-name table).
fn line_chunk(version: u32, instr_count: u32, item_count: u32, items: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    for value in [version, 0, instr_count, item_count, 0] {
        data.extend_from_slice(&value.to_be_bytes());
    }
    data.extend_from_slice(items);
    data
}

#[test]
fn a_negative_line_number_is_rejected() {
    // `LoadAssert(val.size == 0 && val.word_value >= 0)` (`beam_file.c:519-522`): `0x19`
    // is a two-byte `i` magnitude, so `0xFF 0xFF` sign-extends to `-1` — a corrupt line
    // table, never the wrapped line `4294967295`.
    //
    // The table MUST be fixture-sized: every `Line`/`executable_line`/`debug_line`
    // `LineRef` is bound-checked against `Module.line.items` (`jit/asm_load.c:324-341`), so
    // a smaller table leaves the fixture's own line references out of range and the
    // *control* would be rejected. The parser synthesizes `items[0]` (the undefined
    // location), so the chunk's `item_count` is one less than `items.len()`; `0x41` is an
    // `i`-tagged inline line number, i.e. a valid entry.
    let chunks = fixture_chunks();
    let items = load(&fixture())
        .expect("the fixture loads")
        .line
        .items
        .len()
        - 1;
    let control = with_chunk(
        &chunks,
        b"Line",
        line_chunk(0, 7, items as u32, &vec![0x41u8; items]),
    );
    let mut mutated_items = vec![0x41u8; items - 1];
    mutated_items.extend_from_slice(&[0x19, 0xFF, 0xFF]); // the last line is -1
    assert_rejects(
        &control,
        &with_chunk(
            &chunks,
            b"Line",
            line_chunk(0, 7, items as u32, &mutated_items),
        ),
        Expected::BadArg,
        "negative line number",
    );
}

// ---- The atom table (`beam_file.c:260`, `atom.c:283-335`) --------------------

/// The fixture's `AtU8` payload (long/varint form: `count = -10`) with one extra name
/// appended, read back from the container so the encoding stays the compiler's.
fn atoms_with_extra(name: &str) -> Vec<u8> {
    let chunks = fixture_chunks();
    let original = &chunks
        .iter()
        .find(|(id, _)| id == b"AtU8")
        .expect("the fixture carries AtU8")
        .1;
    let count = i32::from_be_bytes(original[..4].try_into().unwrap());
    assert!(count < 0, "the fixture uses the long (varint) length form");

    let mut payload = Vec::new();
    payload.extend_from_slice(&(count - 1).to_be_bytes()); // one more atom
    payload.extend_from_slice(&original[4..]); // the fixture's names, unchanged
    payload.extend_from_slice(&tagged_length(name.len() as u64));
    payload.extend_from_slice(name.as_bytes());
    payload
}

#[test]
fn an_empty_atom_table_is_rejected() {
    // `CHECK_ITEM_COUNT(count, 1, …)` (`beam_file.c:260`): a module always carries its
    // own name, so a zero-count table is corrupt rather than an empty one.
    let chunks = fixture_chunks();
    let mut empty = Vec::new();
    empty.extend_from_slice(&0u32.to_be_bytes());
    assert_rejects(
        &build(&chunks),
        &with_chunk(&chunks, b"AtU8", empty),
        Expected::BadArg,
        "zero-count AtU8",
    );
}

#[test]
fn an_atom_name_longer_than_max_atom_characters_is_rejected() {
    // `MAX_ATOM_CHARACTERS` = 255 characters (`atom.h:29`, `atom.c:313-323`): 300 ASCII
    // characters are well inside the 1020-*byte* limit but 45 characters too many.
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"AtU8", atoms_with_extra(&"a".repeat(20)));
    assert_rejects(
        &control,
        &with_chunk(&chunks, b"AtU8", atoms_with_extra(&"a".repeat(300))),
        Expected::SystemLimit,
        "300-character atom name",
    );

    // Exactly 255 characters is the largest legal name and loads.
    let at_limit = with_chunk(&chunks, b"AtU8", atoms_with_extra(&"a".repeat(255)));
    load(&at_limit).expect("a 255-character atom name is accepted");
}

#[test]
fn an_atom_name_longer_than_max_atom_sz_limit_is_rejected() {
    // `MAX_ATOM_SZ_LIMIT` = 4 * 255 = 1020 **bytes** (`atom.h:31`, `atom.c:284-286`):
    // 256 four-byte characters are 1024 bytes — one character inside the character cap
    // but four bytes past the byte cap.
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"AtU8", atoms_with_extra(&"😀".repeat(255)));
    assert_rejects(
        &control,
        &with_chunk(&chunks, b"AtU8", atoms_with_extra(&"😀".repeat(256))),
        Expected::SystemLimit,
        "1024-byte atom name",
    );
}

// ---- The type table (`beam_types.c:44-82`, `beam_file.c:625-698`) ------------

/// A version-4 `Type` chunk: `count` followed by `entries.len()` raw words, `None`
/// standing for a word that carries no extra fields.
fn type_chunk(entries: &[(u16, Option<(Option<i64>, Option<i64>, Option<u8>)>)]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&4u32.to_be_bytes()); // version 4
    data.extend_from_slice(&(entries.len() as i32).to_be_bytes());
    for (raw, extra) in entries {
        data.extend_from_slice(&raw.to_be_bytes());
        if let Some((lower, upper, unit)) = extra {
            if let Some(lower) = lower {
                data.extend_from_slice(&lower.to_be_bytes());
            }
            if let Some(upper) = upper {
                data.extend_from_slice(&upper.to_be_bytes());
            }
            if let Some(unit) = unit {
                data.push(*unit);
            }
        }
    }
    data
}

/// The fixture's own three entries (`0x1FFF`, `0x2020` + lower 0, `0x2020` + lower 1)
/// plus one further well-formed entry, so a rebuilt table still decodes the module's
/// `z,5` `tr` operands (indices 0..=2) and remains a *positive* control.
fn valid_type_chunk() -> Vec<u8> {
    type_chunk(&[
        (0x1FFF, None),
        (0x2020, Some((Some(0), None, None))),
        (0x2020, Some((Some(1), None, None))),
        (0x2020, Some((Some(2), None, None))),
    ])
}

/// A `Type` chunk with a single entry, for the words the validator rejects before the
/// extra fields they announce are read.
fn single_entry_type_chunk(raw: u16) -> Vec<u8> {
    type_chunk(&[(raw, None)])
}

#[test]
fn the_control_type_table_loads() {
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"Type", valid_type_chunk());
    load(&control).expect("the four-entry control type table loads");
}

#[test]
fn an_empty_type_word_is_rejected() {
    // `beam_types_decode_type` returns -1 for `flags == BEAM_TYPE_NONE`
    // (`beam_types.c:46-48`).
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"Type", valid_type_chunk());
    assert_rejects(
        &control,
        &with_chunk(&chunks, b"Type", single_entry_type_chunk(0x0000)),
        Expected::BadArg,
        "type word 0x0000",
    );
}

#[test]
fn a_type_bound_without_a_numeric_union_is_rejected() {
    // `beam_types.c:52-62`: a lower/upper bound is only meaningful on
    // `INTEGER`/`FLOAT`. `0x2000`/`0x4000` carry the flag on an empty union.
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"Type", valid_type_chunk());
    for raw in [0x2000u16, 0x4000] {
        assert_rejects(
            &control,
            &with_chunk(&chunks, b"Type", single_entry_type_chunk(raw)),
            Expected::BadArg,
            &format!("type word {raw:#06x}"),
        );
    }
}

#[test]
fn a_type_unit_without_a_bitstring_union_is_rejected() {
    // `beam_types.c:63-69`: the unit is only meaningful on `BITSTRING`; `0x8020` is a
    // unit on the `INTEGER` union.
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"Type", valid_type_chunk());
    assert_rejects(
        &control,
        &with_chunk(&chunks, b"Type", single_entry_type_chunk(0x8020)),
        Expected::BadArg,
        "type unit on INTEGER",
    );
}

#[test]
fn an_empty_type_table_is_rejected() {
    // `LoadAssert(count >= 1)` (`beam_file.c:636`): the fallback table is synthesized
    // only for an *absent* or version-incompatible chunk (`beam_file.c:700-710`).
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"Type", valid_type_chunk());
    let mut empty = Vec::new();
    empty.extend_from_slice(&4u32.to_be_bytes());
    empty.extend_from_slice(&0u32.to_be_bytes());
    assert_rejects(
        &control,
        &with_chunk(&chunks, b"Type", empty),
        Expected::BadArg,
        "zero-count Type",
    );
}

#[test]
fn a_type_table_not_starting_with_the_any_type_is_rejected() {
    // `LoadAssert(types->entries[0].type_union == BEAM_TYPE_ANY)` (`beam_file.c:689`):
    // every `z,5` `tr` index 0 means "any".
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"Type", valid_type_chunk());
    let swapped = type_chunk(&[
        (0x0020, None), // INTEGER instead of ANY
        (0x1FFF, None),
        (0x2020, Some((Some(0), None, None))),
    ]);
    assert_rejects(
        &control,
        &with_chunk(&chunks, b"Type", swapped),
        Expected::BadArg,
        "type table whose entry 0 is INTEGER",
    );
}

#[test]
fn a_bounded_any_entry_is_rejected() {
    // `LoadAssert(types->entries[0].min > types->entries[0].max)` (`beam_file.c:690`):
    // the any entry must stay unbounded, which a lower *and* upper bound inverts.
    let chunks = fixture_chunks();
    let control = with_chunk(&chunks, b"Type", valid_type_chunk());
    let bounded = type_chunk(&[(
        0x7FFF, // ANY | HAS_LOWER_BOUND | HAS_UPPER_BOUND
        Some((Some(0), Some(10), None)),
    )]);
    assert_rejects(
        &control,
        &with_chunk(&chunks, b"Type", bounded),
        Expected::BadArg,
        "bounded any entry",
    );
}

// ---- The form-bounded chunk walk (`beam_file.c:2040-2058`) -------------------

#[test]
fn a_form_shorter_than_its_last_chunk_is_rejected() {
    // `iff_init` hands the walk a view of exactly `form_length` bytes, and a chunk whose
    // padded payload does not fit in what is left of it is corrupt
    // (`beamreader_test(&reader, storage_size)`, `beam_file.c:1554`). The fixture's last
    // chunk is `Type` (30 bytes + 2 pad), so four bytes less truncates it.
    let image = fixture();
    let mut mutated = image.clone();
    let form_length = u32::from_be_bytes(image[4..8].try_into().unwrap());
    mutated[4..8].copy_from_slice(&(form_length - 4).to_be_bytes());
    assert_rejects(&image, &mutated, Expected::BadArg, "form cut inside Type");
}

#[test]
fn trailing_bytes_past_the_form_are_ignored() {
    // The C walk stops at the end of the form view, so bytes after it are never examined
    // (`beam_file.c:1548-1575`) — the image still loads and its chunk directory is
    // unchanged. The declared `form_length` is untouched, so the trailing bytes are
    // outside the form.
    let image = fixture();
    let mut extended = image.clone();
    extended.extend_from_slice(b"XXXXtrailing junk past the form");
    let container = container::parse(&extended).expect("the form still parses");
    assert_eq!(container.chunks.len(), container::parse(&image).unwrap().chunks.len());
    load(&extended).expect("trailing bytes past the form do not affect loading");
}

#[test]
fn a_form_that_does_not_hold_the_form_type_is_rejected() {
    // A `form_length` below the 4-byte form type cannot describe a form at all
    // (`read_beam_chunks` reads the form type out of the form view, `beam_file.c:1548`).
    let image = fixture();
    for form_length in [0u32, 3] {
        let mut mutated = image.clone();
        mutated[4..8].copy_from_slice(&form_length.to_be_bytes());
        assert_rejects(
            &image,
            &mutated,
            Expected::BadArg,
            &format!("form_length {form_length}"),
        );
    }
}

#[test]
fn a_chunk_that_overruns_the_form_is_rejected() {
    // A chunk `size` past the end of the form view (`beamreader_test(&reader,
    // storage_size)`, `beam_file.c:1554`) — not merely past the end of the file.
    let image = fixture();
    let mut mutated = image.clone();
    let size_offset = payload_offset(&image, b"AtU8") - 4;
    let form_end = 8 + u32::from_be_bytes(image[4..8].try_into().unwrap()) as usize;
    mutated[size_offset..size_offset + 4]
        .copy_from_slice(&((form_end - size_offset) as u32).to_be_bytes());
    assert_rejects(&image, &mutated, Expected::BadArg, "chunk size overruns form");
}
