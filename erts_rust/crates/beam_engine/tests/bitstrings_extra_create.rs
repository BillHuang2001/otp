//! Bitstring `bs_create_bin` **construction** integration tests for the segment
//! forms added after the original integer/binary/string coverage: the `float`
//! segment (16/32/64 bits, big- and little-endian) and the `utf8`/`utf16`/`utf32`
//! segments.
//!
//! Loads the committed oracle fixture `tests/fixtures/bscreate.beam` (produced by
//! the system `erlc`, OTP 29 / erts-17.1) and runs its exports end to end through
//! [`Machine::run_export`]. **Every expected byte sequence below was derived from
//! the same system `erl` oracle** — e.g.
//! `erl -noshell -eval 'io:format("~w~n",[binary_to_list(bscreate:f32(1.5))])'`
//! — and the segment each export actually emits was confirmed with
//! `beam_disasm:file/1` (see `tests/fixtures/README.md`). Nothing here is guessed.
//!
//! Every failure is a typed [`BeamError`]: a rejected codepoint or an invalid float
//! segment size never panics and never yields a wrong binary.

use beam_common::BeamError;
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{make_small, Atom, Eterm};

/// The committed construction fixture (`erlc bscreate.erl`).
const BSCREATE_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bscreate.beam");

fn load(path: &str) -> Module {
    load_file(path).unwrap_or_else(|e| panic!("load {path}: {e:?}"))
}

fn export_atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned"))
}

/// A float argument term (the fixture's unary exports take a float or a small
/// integer value).
fn float(ctx: &mut MinimalContext, value: f64) -> Eterm {
    terms::make_float(ctx, value).expect("float term")
}

/// Run a unary export and return the exact bytes of the result binary.
fn build_bytes(module: &Module, name: &str, arg: Eterm, ctx: &mut MinimalContext) -> Vec<u8> {
    let mut machine = Machine::new();
    let result = machine
        .run_export(module, export_atom(module, name), 1, &[arg], ctx)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    terms::binary_bytes(result)
        .unwrap_or_else(|e| panic!("{name}: not a binary: {e:?}"))
        .to_vec()
}

/// Assert a unary export is rejected as a typed error (never a panic, never a
/// wrong value). The oracle raises `badarg` here; the engine surfaces a typed
/// [`BeamError`] (a documented Milestone-1 gap — see `CONTEXT.md`), which is what
/// this crate-level test can observe.
fn assert_rejects(module: &Module, name: &str, arg: Eterm, ctx: &mut MinimalContext) {
    let mut machine = Machine::new();
    let result = machine.run_export(module, export_atom(module, name), 1, &[arg], ctx);
    let err: BeamError = result.expect_err(&format!("{name}: expected a typed rejection"));
    // A typed rejection (the `Raise` family, or the engine's `BadMatch` sentinel
    // when the module's atom table has no `badmatch` atom to name the reason).
    assert!(
        matches!(err, BeamError::Raise(_) | BeamError::BadMatch),
        "{name}: expected a typed rejection, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// `float` segments — 16-bit (IEEE binary16)
// ---------------------------------------------------------------------------

#[test]
fn f16_encodes_binary16_big_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(f64, &[u8])] = &[
        (0.0, &[0x00, 0x00]),
        (-0.0, &[0x80, 0x00]),
        (1.0, &[0x3C, 0x00]),
        (-1.0, &[0xBC, 0x00]),
        (1.5, &[0x3E, 0x00]),
        (-1.5, &[0xBE, 0x00]),
        (0.1, &[0x2E, 0x66]),
        (65504.0, &[0x7B, 0xFF]),   // largest finite binary16
        (65519.0, &[0x7B, 0xFF]),   // just below the overflow tie
        (65520.0, &[0x7C, 0x00]),   // overflow tie rounds up to infinity
        (100000.0, &[0x7C, 0x00]),  // saturates to infinity
        (6.103515625e-5, &[0x04, 0x00]), // 2^-14, smallest normal
        (5.960464477539063e-8, &[0x00, 0x01]), // 2^-24, smallest subnormal
        (2.9802322387695312e-8, &[0x00, 0x00]), // 2^-25: RNE tie -> even -> 0
        (-2.9802322387695312e-8, &[0x80, 0x00]), // -2^-25 -> -0
        (8.940696716308594e-8, &[0x00, 0x02]), // 3*2^-25: RNE tie -> even
        (1.00048828125, &[0x3C, 0x00]),  // 1 + 2^-11: RNE tie -> even
        (1.00146484375, &[0x3C, 0x02]),  // 1 + 3*2^-11: RNE tie -> even
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "f16", float(&mut ctx, value), &mut ctx);
        assert_eq!(got, expected, "f16({value})");
    }
}

#[test]
fn f16le_encodes_binary16_little_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(f64, &[u8])] = &[
        (0.0, &[0x00, 0x00]),
        (-0.0, &[0x00, 0x80]),
        (1.0, &[0x00, 0x3C]),
        (1.5, &[0x00, 0x3E]),
        (-1.5, &[0x00, 0xBE]),
        (0.1, &[0x66, 0x2E]),
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "f16le", float(&mut ctx, value), &mut ctx);
        assert_eq!(got, expected, "f16le({value})");
    }
}

// ---------------------------------------------------------------------------
// `float` segments — 32-bit (IEEE binary32)
// ---------------------------------------------------------------------------

#[test]
fn f32_encodes_binary32_big_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(f64, &[u8])] = &[
        (0.0, &[0x00, 0x00, 0x00, 0x00]),
        (1.0, &[0x3F, 0x80, 0x00, 0x00]),
        (-1.0, &[0xBF, 0x80, 0x00, 0x00]),
        (1.5, &[0x3F, 0xC0, 0x00, 0x00]),
        (0.1, &[0x3D, 0xCC, 0xCC, 0xCD]),
        (3.4028234663852886e38, &[0x7F, 0x7F, 0xFF, 0xFF]), // f32::MAX
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "f32", float(&mut ctx, value), &mut ctx);
        assert_eq!(got, expected, "f32({value})");
    }
}

#[test]
fn f32le_encodes_binary32_little_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(f64, &[u8])] = &[
        (1.0, &[0x00, 0x00, 0x80, 0x3F]),
        (1.5, &[0x00, 0x00, 0xC0, 0x3F]),
        (-1.5, &[0x00, 0x00, 0xC0, 0xBF]),
        (0.1, &[0xCD, 0xCC, 0xCC, 0x3D]),
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "f32le", float(&mut ctx, value), &mut ctx);
        assert_eq!(got, expected, "f32le({value})");
    }
}

// ---------------------------------------------------------------------------
// `float` segments — 64-bit (IEEE binary64)
// ---------------------------------------------------------------------------

#[test]
fn f64_encodes_binary64_big_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(f64, &[u8])] = &[
        (0.0, &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
        (-0.0, &[0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
        (1.0, &[0x3F, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
        (-1.0, &[0xBF, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
        (1.5, &[0x3F, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
        (
            0.1,
            &[0x3F, 0xB9, 0x99, 0x99, 0x99, 0x99, 0x99, 0x9A],
        ),
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "f64", float(&mut ctx, value), &mut ctx);
        assert_eq!(got, expected, "f64({value})");
    }
}

#[test]
fn f64le_encodes_binary64_little_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(f64, &[u8])] = &[
        (1.0, &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF0, 0x3F]),
        (1.5, &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF8, 0x3F]),
        (-1.5, &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF8, 0xBF]),
        (
            0.1,
            &[0x9A, 0x99, 0x99, 0x99, 0x99, 0x99, 0xB9, 0x3F],
        ),
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "f64le", float(&mut ctx, value), &mut ctx);
        assert_eq!(got, expected, "f64le({value})");
    }
}

#[test]
fn a_float_segment_accepts_a_small_integer_value() {
    // `<<1:64/float>>` / `<<1:32/float>>` — a small integer is promoted to a float
    // (`float_source`), exactly as the oracle does.
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    assert_eq!(
        build_bytes(&module, "f64", make_small(1), &mut ctx),
        vec![0x3F, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        build_bytes(&module, "f32", make_small(1), &mut ctx),
        vec![0x3F, 0x80, 0x00, 0x00]
    );
}

#[test]
fn an_invalid_float_segment_size_is_rejected() {
    // f8(F) -> <<F:8/float>>. The oracle raises `badarg`; the engine rejects the
    // segment size (only 16/32/64 are valid float widths) as a typed error.
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    assert_rejects(&module, "f8", float(&mut ctx, 1.0), &mut ctx);
}

// ---------------------------------------------------------------------------
// `utf8` segments
// ---------------------------------------------------------------------------

#[test]
fn u8_encodes_utf8_scalars() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(i64, &[u8])] = &[
        (0, &[0x00]),
        (1, &[0x01]),
        (0x7F, &[0x7F]),                    // single byte
        (0x80, &[0xC2, 0x80]),              // two bytes
        (0x7FF, &[0xDF, 0xBF]),             // two-byte maximum
        (0x800, &[0xE0, 0xA0, 0x80]),       // three bytes
        (0xFFFF, &[0xEF, 0xBF, 0xBF]),      // three-byte maximum
        (0x10000, &[0xF0, 0x90, 0x80, 0x80]), // four bytes
        (0x10FFFF, &[0xF4, 0x8F, 0xBF, 0xBF]), // maximum scalar value
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "u8", make_small(value), &mut ctx);
        assert_eq!(got, expected, "u8({value})");
    }
}

#[test]
fn u8_rejects_non_scalars() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    for value in [-1, 0xD800, 0xDFFF, 0x110000] {
        assert_rejects(&module, "u8", make_small(value), &mut ctx);
    }
}

// ---------------------------------------------------------------------------
// `utf16` segments (big- and little-endian)
// ---------------------------------------------------------------------------

#[test]
fn u16_encodes_utf16_big_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(i64, &[u8])] = &[
        (0, &[0x00, 0x00]),
        (1, &[0x00, 0x01]),
        (0x7F, &[0x00, 0x7F]),
        (0xFFFF, &[0xFF, 0xFF]),            // BMP maximum, one code unit
        (0x10000, &[0xD8, 0x00, 0xDC, 0x00]), // first non-BMP: surrogate pair
        (0x10FFFF, &[0xDB, 0xFF, 0xDF, 0xFF]), // maximum scalar value
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "u16", make_small(value), &mut ctx);
        assert_eq!(got, expected, "u16({value})");
    }
}

#[test]
fn u16le_encodes_utf16_little_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(i64, &[u8])] = &[
        (0, &[0x00, 0x00]),
        (0x7F, &[0x7F, 0x00]),
        (0xFFFF, &[0xFF, 0xFF]),
        (0x10000, &[0x00, 0xD8, 0x00, 0xDC]),
        (0x10FFFF, &[0xFF, 0xDB, 0xFF, 0xDF]),
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "u16le", make_small(value), &mut ctx);
        assert_eq!(got, expected, "u16le({value})");
    }
}

#[test]
fn u16_rejects_non_scalars() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    for value in [-1, 0xD800, 0xDFFF, 0x110000] {
        assert_rejects(&module, "u16", make_small(value), &mut ctx);
    }
}

// ---------------------------------------------------------------------------
// `utf32` segments
// ---------------------------------------------------------------------------

#[test]
fn u32_encodes_utf32_big_endian() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    let cases: &[(i64, &[u8])] = &[
        (0, &[0x00, 0x00, 0x00, 0x00]),
        (1, &[0x00, 0x00, 0x00, 0x01]),
        (0x7F, &[0x00, 0x00, 0x00, 0x7F]),
        (0xFFFF, &[0x00, 0x00, 0xFF, 0xFF]),
        (0x10000, &[0x00, 0x01, 0x00, 0x00]),
        (0x10FFFF, &[0x00, 0x10, 0xFF, 0xFF]),
    ];
    for &(value, expected) in cases {
        let got = build_bytes(&module, "u32", make_small(value), &mut ctx);
        assert_eq!(got, expected, "u32({value})");
    }
}

#[test]
fn u32_rejects_non_scalars() {
    let module = load(BSCREATE_BEAM);
    let mut ctx = MinimalContext::new();
    for value in [-1, 0xD800, 0xDFFF, 0x110000] {
        assert_rejects(&module, "u32", make_small(value), &mut ctx);
    }
}

// ---------------------------------------------------------------------------
// Sanity: the fixture's exports are the ones the disassembly claims
// ---------------------------------------------------------------------------

#[test]
fn every_construction_export_is_present() {
    // A guard against a stale/rebuilt fixture: each export the tests rely on must
    // resolve to an atom in the module (a missing one fails the lookup).
    let module = load(BSCREATE_BEAM);
    for name in [
        "f16", "f16le", "f32", "f32le", "f64", "f64le", "f8", "u8", "u16", "u16le", "u32",
    ] {
        let _: Atom = export_atom(&module, name);
    }
}
