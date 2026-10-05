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
