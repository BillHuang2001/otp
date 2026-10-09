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

//! The **`LitT` literal-pool** end-to-end tests.
//!
//! The `LitT` chunk holds a module's static literals (the `Operand::Literal(idx)`
//! pool) as ETf-encoded terms, in one of two framings: **verbatim** (OTP 28+,
//! `uncompressed_size == 0`) or **zlib-compressed** (OTP 27 and earlier,
//! `uncompressed_size != 0`). [`crate::chunk::literal`] owns the framing and
//! [`crate::literal::LiteralArea`] decodes each payload into a term reachable
//! through [`Module::literal`]. This file proves that round-trip against the real
//! OTP install and the real oracle.
//!
//! # What is asserted, and how
//!
//! * **Both framings** — the committed fixture pair
//!   (`tests/fixtures/literals_verbatim.beam` / `literals_compressed.beam`, the
//!   same source compiled by the system `erlc` with and without
//!   `+compressed_literals`) is loaded, and the two pools must carry the *same*
//!   payloads and decode to identically re-encoded terms (each framing's re-encode
//!   equals the other's).
//! * **Map-free literals round-trip byte-exactly** — for a payload that carries no
//!   map, `encode_term(decode_term(payload))` is asserted to reproduce the payload
//!   the compiler wrote, byte for byte.
//! * **Map-bearing literals are asserted against the oracle, not the payload** —
//!   the fixture is compiled with `+deterministic`, so a flat map's keys are written
//!   in **name-lexicographic** order (the `term_to_binary/2` `[deterministic]`
//!   option), whereas ERTS — and therefore this loader — stores a flat map's keys in
//!   `erts_cmp_flatmap_keys` order, which orders two atom keys by **atom-table
//!   index** (`../../erts/emulator/beam/erl_utils.h`). Such a payload is *not*
//!   reproducible from the decoded term, so
//!   [`both_litt_forms_decode_to_identically_encoded_terms`] asserts the real
//!   contract for it — the Rust re-encode equals **ERTS's own**
//!   `term_to_binary(binary_to_term(Payload))` (obtained from the oracle script) —
//!   and
//!   [`map_bearing_fixture_literal_is_normalized_to_erts_key_order_on_load`] pins
//!   both the compiler's *payload* key order and the loader's *stored* key order of
//!   the `nested/0` map.
//! * **Nested/composite literals** — the fixture covers a tuple containing a
//!   map/float/list, a binary literal, a non-byte-aligned bitstring and a bignum.
//!   [`the_fixture_covers_every_required_literal_kind`] asserts each literal's
//!   *structure* (arity / list length / map pairs / float value / bitstring bits /
//!   fun MFA / bignum sign).
//! * **The whole installed OTP, against the oracle** —
//!   [`installed_literal_pools_agree_with_the_oracle`] walks every `.beam` under
//!   the running system's `lib` root, decodes every raw `LitT` payload through the
//!   crate's public pipeline, and checks two independent properties per literal:
//!   1. the decoded term re-encodes **byte-exactly** to the payload the compiler
//!      wrote (a pure Rust decode+encode round-trip) — except for a literal that
//!      contains a map, whose payload carries the compiler's flat-map key order
//!      that ERTS normalizes; those are counted separately and rest on check (2); and
//!   2. the decoded term is **semantically equal** to the term the oracle gets from
//!      the same payload (`binary_to_term/1` -> `term_to_binary/1`, compared with
//!      `beam_term::compare`, i.e. insensitive to a flat map's stored key order,
//!      which OTP's `binary_to_term/1` normalizes and our decoder preserves).
//!
//!   The oracle runs `tests/fixtures/literals_oracle.escript`, which parses the
//!   `LitT` chunks with `beam_lib`/`zlib` — an implementation **independent** of
//!   this crate's framing, so a framing bug cannot invalidate its own check.
//!
//! # Skip-vs-assert policy (explicit)
//!
//! A missing OTP install (no `erl`, or `stdlib`/`kernel` not locatable) **skips
//! LOUDLY** with a `SKIP:` line and returns — it never fails in that case, mirroring
//! `tests/real_modules.rs`. It never silently passes without OTP. The committed
//! fixtures cover the compressed framing deterministically; the install sweep adds
//! breadth and is skipped (loudly) when the OTP `lib` root is absent.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader
//! ```
//!
//! With `-- --nocapture` the `LITERALS:` summary (module/literal counts and the
//! verbatim-vs-compressed split) is printed.

mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_alloc::Heap;
use beam_codec::term::{
    big_parts, bitstring_bytes, cons_head, cons_tail, flatmap_parts, float_value, tuple_arity,
    tuple_element,
};
use beam_codec::{
    decode_term, encode_term, fun_arity_term, fun_function, fun_kind_term, fun_module, FunKind,
};
use beam_loader::container::{parse as parse_container, ChunkId};
use beam_loader::Module;
use beam_term::prelude::{atom_val, cmp, is_atom, is_nil, signed_value, Eterm};

use crate::common::raw_map_key_names;

// ---------------------------------------------------------------------------
// The committed fixtures (both framings of the same source)
// ---------------------------------------------------------------------------

/// `literals_fixture.erl` compiled with `erlc +deterministic` (the verbatim `LitT`
/// framing, OTP 28+); see `tests/fixtures/README.md` for the regeneration commands.
const VERBATIM_BEAM: &[u8] = include_bytes!("fixtures/literals_verbatim.beam");
/// The same source compiled with `erlc +deterministic +compressed_literals` (the
/// OTP 27 zlib `LitT` framing).
const COMPRESSED_BEAM: &[u8] = include_bytes!("fixtures/literals_compressed.beam");

/// Which `LitT` framing a chunk uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LitForm {
    /// The record table follows verbatim (`uncompressed_size == 0`, OTP 28+).
    Verbatim,
    /// The chunk body is a zlib stream inflating to `uncompressed_size` bytes.
    Compressed,
}

/// The raw `LitT` chunk payload of a `.beam` image, if the module has one.
fn litt_chunk(bytes: &[u8]) -> Option<&[u8]> {
    parse_container(bytes)
        .ok()?
        .find(ChunkId::LITT)
        .map(|chunk| chunk.data)
}

/// Classifies a `LitT` chunk payload by its leading `u32 uncompressed_size`.
///
/// A chunk shorter than its own 4-byte header is reported as [`LitForm::Verbatim`];
/// [`beam_loader::chunk::literal::parse`] then rejects it, so the malformed chunk is
/// still surfaced as a failure rather than being silently accepted.
fn lit_form(chunk: &[u8]) -> LitForm {
    match chunk.get(..4) {
        Some(header) if u32::from_be_bytes(header.try_into().unwrap()) != 0 => LitForm::Compressed,
        _ => LitForm::Verbatim,
    }
}

/// The raw literal payloads (ETF bytes) of a `.beam` image, in `Operand::Literal(idx)`
/// index order, through the crate's *public* framing parser.
fn raw_literals(bytes: &[u8]) -> Vec<Vec<u8>> {
    match litt_chunk(bytes) {
        Some(chunk) => {
            beam_loader::chunk::literal::parse(chunk).expect("the LitT framing must parse")
        }
        None => Vec::new(),
    }
}

/// Loads a `.beam` image through the public pipeline (fails loudly on a defect).
fn load(bytes: &[u8]) -> Module {
    beam_loader::load(bytes).expect("the fixture must load")
}

/// The literal at `index`, resolved through the module's [`LiteralArea`].
fn literal(module: &Module, index: usize) -> Eterm {
    module
        .literal(index as u32)
        .unwrap_or_else(|| panic!("literal {index} must be decoded"))
}

/// The elements of a proper list, in order (a non-list/improper tail is a failure).
fn list_elements(term: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    let mut cursor = term;
    while !is_nil(cursor) {
        out.push(cons_head(cursor).expect("a proper list element"));
        cursor = cons_tail(cursor).expect("a proper list tail");
    }
    out
}

/// The name of an `Atom` in `module`'s atom table.
fn name_of<'a>(module: &'a Module, atom: beam_term::Atom) -> &'a str {
    module.atom_name(atom).expect("an interned atom name")
}

/// The name of an atom term, resolved through `module`'s atom table.
fn atom_name<'a>(module: &'a Module, term: Eterm) -> &'a str {
    name_of(module, atom_val(term))
}

/// The exact bytes and bit count of a binary/bitstring literal.
fn bits(term: Eterm) -> (Vec<u8>, usize) {
    let (bytes, offset, size) = bitstring_bytes(term).expect("a bitstring literal");
    assert_eq!(offset, 0, "a decoded literal bitstring starts at a byte boundary");
    (bytes.to_vec(), size)
}

// ---------------------------------------------------------------------------
// Part 1: the committed fixtures -- both framings, every literal kind
// ---------------------------------------------------------------------------

/// The fixture pair really is one compressed and one verbatim `LitT` chunk, and the
/// verbatim one is the larger image (compression is the whole point).
#[test]
fn the_committed_fixtures_cover_both_litt_forms() {
    let verbatim = litt_chunk(VERBATIM_BEAM).expect("the verbatim fixture has a LitT chunk");
    let compressed = litt_chunk(COMPRESSED_BEAM).expect("the compressed fixture has a LitT chunk");

    assert_eq!(lit_form(verbatim), LitForm::Verbatim);
    assert_eq!(lit_form(compressed), LitForm::Compressed);

    // The compressed body is a zlib (RFC 1950) stream: `0x78` is its CMF byte, and
    // it is strictly smaller than the size it inflates to.
    let uncompressed_size = u32::from_be_bytes(compressed[..4].try_into().unwrap()) as usize;
    assert_eq!(compressed[4], 0x78, "the compressed body must be a zlib stream");
    assert!(
        compressed.len() - 4 < uncompressed_size,
        "the zlib body ({} bytes) must be smaller than the {uncompressed_size} bytes it inflates to",
        compressed.len() - 4
    );
    assert!(COMPRESSED_BEAM.len() < VERBATIM_BEAM.len());
}

/// Both framings carry the *same* payloads and decode to identically re-encoded
/// terms, and every literal re-encodes to what the real ERTS produces from the same
/// payload — byte-exactly for a map-free literal (where ERTS's image *is* the
/// compiler's payload), and "equal to the oracle's own re-encode" for a map-bearing
/// one (whose payload carries the compiler's `+deterministic` flat-map key order
/// that ERTS normalizes; see the module docs).
#[test]
fn both_litt_forms_decode_to_identically_encoded_terms() {
    let verbatim = load(VERBATIM_BEAM);
    let compressed = load(COMPRESSED_BEAM);
    let verbatim_payloads = raw_literals(VERBATIM_BEAM);
    let compressed_payloads = raw_literals(COMPRESSED_BEAM);

    assert_eq!(verbatim_payloads.len(), 9, "the fixture declares nine literals");
    assert_eq!(
        compressed_payloads, verbatim_payloads,
        "the two framings must carry identical literal payloads"
    );
    assert_eq!(verbatim.literals().len(), 9);
    assert_eq!(compressed.literals().len(), 9);

    // ERTS's own re-encode of each payload is the reference for the map-bearing
    // literals. Without an OTP install the comparison is skipped *loudly* (the rest
    // of the assertions below are deterministic and still run).
    let oracle = fixture_oracle_reencode();

    let mut map_literals = 0usize;
    let mut oracle_checked = 0usize;
    for (index, payload) in verbatim_payloads.iter().enumerate() {
        // Both framings must decode to the same term, seen as identical bytes.
        let v = encode_term(literal(&verbatim, index), &verbatim.atom_table)
            .unwrap_or_else(|error| panic!("verbatim literal {index} must encode: {error:?}"));
        let c = encode_term(literal(&compressed, index), &compressed.atom_table)
            .unwrap_or_else(|error| panic!("compressed literal {index} must encode: {error:?}"));
        assert_eq!(
            c, v,
            "the verbatim and compressed framings must decode literal {index} to identically \
             encoded terms"
        );

        if !is_map_literal(literal(&verbatim, index), &verbatim.atom_table) {
            // No map anywhere in the term: the decoded term re-encodes to the payload
            // the compiler wrote, byte for byte.
            assert_eq!(&v, payload, "verbatim literal {index} must round-trip byte-exactly");
            continue;
        }

        // The payload carries a map, so it holds the compiler's `+deterministic`
        // (name-lexicographic) key order, which ERTS normalizes on load: the assertion
        // is Rust == ERTS's own re-encode of the same payload, never Rust == payload.
        map_literals += 1;
        if let Some(canonical) = &oracle {
            let expected = canonical.get(&index).unwrap_or_else(|| {
                panic!("the oracle reported no literal {index} of the verbatim fixture")
            });
            assert_eq!(
                &v, expected,
                "verbatim literal {index} (map-bearing) must re-encode exactly as the oracle's \
                 `term_to_binary(binary_to_term(Payload))`"
            );
            oracle_checked += 1;
        }
    }
    assert!(
        map_literals > 0,
        "the fixture must contain a map-bearing literal, otherwise the oracle-backed branch \
         above never runs"
    );
    if oracle.is_some() {
        assert_eq!(
            oracle_checked, map_literals,
            "every map-bearing literal must have been compared against the oracle"
        );
    }
}

/// The `nested/0` literal is `{#{list => [a, b], float => 1.5, bin => <<7, 8>>}, …}`.
///
/// Its inner flat map pins the loader's map **normalization**: the fixture's payload
/// carries the compiler's `+deterministic` (name-lexicographic) key order
/// (`bin`, `float`, `list`), while the loader — like ERTS's `binary_to_term/1` —
/// must *store* it in `erts_cmp_flatmap_keys` order, which for atom keys is the
/// **atom-table index** order (here `list`, `float`, `bin`). Those two orders differ,
/// which is exactly why a map-bearing literal payload is not byte-preservable (see
/// [`both_litt_forms_decode_to_identically_encoded_terms`]).
#[test]
fn map_bearing_fixture_literal_is_normalized_to_erts_key_order_on_load() {
    let verbatim = load(VERBATIM_BEAM);
    let compressed = load(COMPRESSED_BEAM);
    let verbatim_payloads = raw_literals(VERBATIM_BEAM);
    let compressed_payloads = raw_literals(COMPRESSED_BEAM);

    // The raw payload bytes, read WITHOUT decoding (a decode normalizes the order
    // away): the compiler's `+deterministic` image is name-lexicographic.
    let compiler_order = vec!["bin".to_string(), "float".to_string(), "list".to_string()];
    assert_eq!(
        raw_map_key_names(&verbatim_payloads[7]),
        compiler_order,
        "the verbatim payload must carry the compiler's `+deterministic` key order"
    );
    assert_eq!(
        raw_map_key_names(&compressed_payloads[7]),
        compiler_order,
        "the two framings must carry the same payload bytes"
    );

    // The stored order is `erts_cmp_flatmap_keys` order, in both framings.
    let erts_order = vec!["list".to_string(), "float".to_string(), "bin".to_string()];
    for (form, module, payloads) in [
        ("verbatim", &verbatim, &verbatim_payloads),
        ("compressed", &compressed, &compressed_payloads),
    ] {
        let tuple = literal(module, 7);
        let inner_map = tuple_element(tuple, 0).expect("element 0 of `nested/0` is the map");
        let (keys, values) = flatmap_parts(inner_map).expect("a flat map literal");
        assert_eq!(keys.len(), 3);
        assert_eq!(values.len(), 3);
        let stored: Vec<String> =
            keys.iter().map(|&key| atom_name(module, key).to_string()).collect();
        assert_eq!(
            stored, erts_order,
            "the {form} literal's stored flat map must be in `erts_cmp_flatmap_keys` order"
        );
        // ... which for atom keys means ascending ATOM-TABLE INDEX order.
        let indices: Vec<u32> = keys.iter().map(|&key| atom_val(key).raw()).collect();
        assert!(
            indices.windows(2).all(|pair| pair[0] < pair[1]),
            "the {form} literal's stored keys must ascend by atom index, got {indices:?}"
        );

        // The two orders really do differ, so this literal is a genuine
        // not-byte-preservable case: re-encoding the decoded term cannot reproduce the
        // compiler's payload. (This is what makes the byte-exact assertion in
        // `both_litt_forms_decode_to_identically_encoded_terms` impossible here.)
        let re_encoded = encode_term(tuple, &module.atom_table)
            .unwrap_or_else(|error| panic!("the {form} literal must encode: {error:?}"));
        assert_ne!(
            re_encoded, payloads[7],
            "the {form} literal's re-encode must differ from the `+deterministic` payload"
        );
    }
}

/// Each fixture literal has the *structure* its source term has: a tuple carrying a
/// map/float/list, a list, a map with a binary key and a float value, floats, positive
/// and negative bignums, binaries including a non-byte-aligned bitstring, and external
/// funs.
#[test]
fn the_fixture_covers_every_required_literal_kind() {
    let module = load(VERBATIM_BEAM);
    let l: Vec<Eterm> = (0..9).map(|index| literal(&module, index)).collect();

    // lit0: `{a, 1, {b, 2}, []}` -- a tuple with a nested tuple and the empty list.
    assert_eq!(tuple_arity(l[0]).unwrap(), 4);
    assert_eq!(atom_name(&module, tuple_element(l[0], 0).unwrap()), "a");
    assert_eq!(signed_value(tuple_element(l[0], 1).unwrap()), 1);
    let nested_tuple = tuple_element(l[0], 2).unwrap();
    assert_eq!(tuple_arity(nested_tuple).unwrap(), 2);
    assert_eq!(atom_name(&module, tuple_element(nested_tuple, 0).unwrap()), "b");
    assert_eq!(signed_value(tuple_element(nested_tuple, 1).unwrap()), 2);
    assert!(is_nil(tuple_element(l[0], 3).unwrap()));

    // lit1: `[1, 2, [3, 4], {ok, 5}, "abc"]` -- a list with a nested list, a tuple and
    // a string (a list of integers).
    let elements = list_elements(l[1]);
    assert_eq!(elements.len(), 5);
    assert_eq!(signed_value(elements[0]), 1);
    assert_eq!(signed_value(elements[1]), 2);
    assert_eq!(
        list_elements(elements[2]).iter().map(|&e| signed_value(e)).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(tuple_arity(elements[3]).unwrap(), 2);
    assert_eq!(atom_name(&module, tuple_element(elements[3], 0).unwrap()), "ok");
    assert_eq!(signed_value(tuple_element(elements[3], 1).unwrap()), 5);
    assert_eq!(
        list_elements(elements[4]).iter().map(|&e| signed_value(e)).collect::<Vec<_>>(),
        vec![97, 98, 99],
        "\"abc\" decodes to the integer list [97, 98, 99]"
    );

    // lit2: `#{a => 1, {x, y} => [1, 2], <<"k">> => 2.0}` -- a flat map with an atom
    // key, a tuple key and a *binary* key whose value is a float. The key order is
    // deliberately not asserted: the raw payload's order is the compiler's, and a
    // flat map's stored order is not observable in Erlang.
    let (keys, values) = flatmap_parts(l[2]).expect("a flat map literal");
    assert_eq!(keys.len(), 3);
    let (mut atom_key, mut tuple_key, mut binary_key) = (false, false, false);
    for (&key, &value) in keys.iter().zip(values) {
        if is_atom(key) {
            assert_eq!(atom_name(&module, key), "a");
            assert_eq!(signed_value(value), 1);
            atom_key = true;
        } else if let Ok(arity) = tuple_arity(key) {
            assert_eq!(arity, 2);
            assert_eq!(
                list_elements(value).iter().map(|&e| signed_value(e)).collect::<Vec<_>>(),
                vec![1, 2]
            );
            tuple_key = true;
        } else {
            assert_eq!(bits(key), (b"k".to_vec(), 8), "the third key is the binary <<\"k\">>");
            assert_eq!(float_value(value).unwrap(), 2.0);
            binary_key = true;
        }
    }
    assert!(atom_key && tuple_key && binary_key, "all three map keys must be present");

    // lit3: `[1.5, -2.25, 1.0e300, 0.0]` -- floats.
    assert_eq!(
        list_elements(l[3]).iter().map(|&e| float_value(e).unwrap()).collect::<Vec<_>>(),
        vec![1.5, -2.25, 1.0e300, 0.0]
    );

    // lit4: `[123456789012345678901234567890, -99999999999999999999999999]` -- positive
    // and negative bignums.
    let bignums = list_elements(l[4]);
    assert_eq!(bignums.len(), 2);
    assert!(!big_parts(bignums[0]).unwrap().0, "the first bignum is positive");
    assert!(big_parts(bignums[1]).unwrap().0, "the second bignum is negative");
    for bignum in bignums {
        assert!(!big_parts(bignum).unwrap().1.is_empty());
    }

    // lit5: `[<<1,2,3>>, <<"hello">>, <<1:3>>]` -- binaries, the last bit-aligned.
    let binaries = list_elements(l[5]);
    assert_eq!(binaries.len(), 3);
    assert_eq!(bits(binaries[0]), (vec![1, 2, 3], 24));
    assert_eq!(bits(binaries[1]), (b"hello".to_vec(), 40));
    assert_eq!(bits(binaries[2]), (vec![0x20], 3), "`<<1:3>>` is three bits, value 1");

    // lit6: `[fun lists:reverse/1, fun maps:get/2]` -- external (`EXPORT_EXT`) funs.
    let funs = list_elements(l[6]);
    assert_eq!(funs.len(), 2);
    let mfas: Vec<(String, String, usize)> = funs
        .iter()
        .map(|&fun| {
            assert_eq!(fun_kind_term(fun).unwrap(), FunKind::External);
            (
                name_of(&module, fun_module(fun).unwrap()).to_string(),
                name_of(&module, fun_function(fun).unwrap()).to_string(),
                fun_arity_term(fun).unwrap(),
            )
        })
        .collect();
    assert_eq!(
        mfas,
        vec![
            ("lists".to_string(), "reverse".to_string(), 1),
            ("maps".to_string(), "get".to_string(), 2),
        ]
    );

    // lit7: `{#{list => [a, b], float => 1.5, bin => <<7, 8>>}, 10^25, [<<"x">>]}` --
    // a tuple carrying a map (with a float and a binary value), a bignum and a list.
    assert_eq!(tuple_arity(l[7]).unwrap(), 3);
    let inner_map = tuple_element(l[7], 0).unwrap();
    let (inner_keys, inner_values) = flatmap_parts(inner_map).expect("a flat map literal");
    assert_eq!(inner_keys.len(), 3);
    let float_value_in_map = inner_keys
        .iter()
        .zip(inner_values)
        .find_map(|(&key, &value)| {
            (module.atom_name(atom_val(key)) == Some("float")).then(|| float_value(value).unwrap())
        })
        .expect("the inner map has a `float` key");
    assert_eq!(float_value_in_map, 1.5);
    assert_eq!(big_parts(tuple_element(l[7], 1).unwrap()).unwrap().0, false);
    assert_eq!(bits(cons_head(tuple_element(l[7], 2).unwrap()).unwrap()).0, b"x".to_vec());

    // lit8: `{ok, [1, 2, 3], {a, 1.5}, #{k => v}}`.
    assert_eq!(tuple_arity(l[8]).unwrap(), 4);
    assert_eq!(atom_name(&module, tuple_element(l[8], 0).unwrap()), "ok");
    assert_eq!(list_elements(tuple_element(l[8], 1).unwrap()).len(), 3);
    assert_eq!(float_value(tuple_element(tuple_element(l[8], 2).unwrap(), 1).unwrap()).unwrap(), 1.5);
    assert_eq!(flatmap_parts(tuple_element(l[8], 3).unwrap()).unwrap().0.len(), 1);
}

// ---------------------------------------------------------------------------
// Part 2: the whole installed OTP, against the real oracle
// ---------------------------------------------------------------------------

/// The running Erlang's `lib` root (the directory holding the versioned application
/// directories), or `None` when it cannot be located: `$ERTS_LIB_DIR` when set, else
/// OTP's own `code:lib_dir/0`.
fn otp_lib_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ERTS_LIB_DIR") {
        return Some(PathBuf::from(dir));
    }
    let output = Command::new("erl")
        .args(["-noshell", "-eval", "io:format(\"~s\",[code:lib_dir()]),halt()."])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// The `ebin` directory of the running Erlang's `app` application, or `None`.
fn app_ebin(app: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(format!("ERTS_{}_DIR", app.to_uppercase())) {
        let dir = PathBuf::from(dir);
        if has_beams(&dir) {
            return Some(dir);
        }
        let ebin = dir.join("ebin");
        return has_beams(&ebin).then_some(ebin);
    }
    let eval = format!("io:format(\"~s\",[code:lib_dir({app})]),halt().");
    let output = Command::new("erl").args(["-noshell", "-eval", &eval]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let app_dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    if has_beams(&app_dir) {
        return Some(app_dir);
    }
    let ebin = app_dir.join("ebin");
    has_beams(&ebin).then_some(ebin)
}

/// True when `dir` is a directory containing at least one `*.beam` file.
fn has_beams(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry.path().extension().map(|extension| extension == "beam").unwrap_or(false)
    })
}

/// The reason to skip loudly, or `None` when `stdlib`/`kernel` are available.
fn skip_reason() -> Option<String> {
    if app_ebin("stdlib").is_none() {
        return Some(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the installed-literal sweep requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    if app_ebin("kernel").is_none() {
        return Some(
            "SKIP: system Erlang/kernel not found (need `erl` on PATH or $ERTS_KERNEL_DIR) — \
             the installed-literal sweep requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    None
}

/// Recursively collects every `*.beam` file beneath `dir` into `out`, sorted.
fn collect_beams(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_beams(&path, out);
        } else if path.extension().map(|extension| extension == "beam").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// A short, stable label for reporting: `path` relative to `root` when possible.
fn rel_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// Runs the oracle script over `root` and returns `(module, index) -> canonical bytes`,
/// where the canonical form is `term_to_binary(binary_to_term(Payload))`.
///
/// The script (`tests/fixtures/literals_oracle.escript`) parses the `LitT` chunks
/// with OTP's own `beam_lib`/`zlib`, so it is an implementation independent of this
/// crate's framing. A failure to run it (or an undecodable payload) fails the test.
fn oracle_literals(root: &Path) -> BTreeMap<(String, usize), Vec<u8>> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/literals_oracle.escript");
    let output = Command::new("escript")
        .arg(&script)
        .arg(root)
        .output()
        .unwrap_or_else(|error| panic!("could not run {}: {error}", script.display()));
    assert!(
        output.status.success(),
        "the literal oracle failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("the oracle emits UTF-8");

    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        let mut fields = line.split('\t');
        let rel = fields.next().expect("a path field").to_string();
        let index: usize = fields.next().expect("an index field").parse().expect("a decimal index");
        let canonical = from_hex(fields.next().expect("a hex field"));
        assert!(fields.next().is_none(), "unexpected field in the oracle line {line:?}");
        assert!(
            map.insert((rel.clone(), index), canonical).is_none(),
            "the oracle reported {rel} literal {index} twice"
        );
    }
    map
}

/// Runs the oracle script's `--reencode` mode over `beam_file` and returns
/// `index -> term_to_binary(binary_to_term(Payload))`, the oracle's own decode/re-encode
/// of every raw `LitT` payload of that one module.
///
/// The script (`tests/fixtures/literals_oracle.escript`) parses the `LitT` chunk with
/// OTP's own `beam_lib`/`zlib`, so it is an implementation independent of this crate's
/// framing. A failure to run it (or an undecodable payload) fails the test.
fn oracle_reencode(beam_file: &Path) -> BTreeMap<usize, Vec<u8>> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/literals_oracle.escript");
    let output = Command::new("escript")
        .arg(&script)
        .arg("--reencode")
        .arg(beam_file)
        .output()
        .unwrap_or_else(|error| panic!("could not run {}: {error}", script.display()));
    assert!(
        output.status.success(),
        "the literal oracle (--reencode {}) failed ({}): {}",
        beam_file.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("the oracle emits UTF-8");

    let mut map = BTreeMap::new();
    for line in stdout.lines() {
        let mut fields = line.split('\t');
        let index: usize = fields.next().expect("an index field").parse().expect("a decimal index");
        let canonical = from_hex(fields.next().expect("a hex field"));
        assert!(fields.next().is_none(), "unexpected field in the oracle line {line:?}");
        assert!(
            map.insert(index, canonical).is_none(),
            "the oracle reported literal {index} twice"
        );
    }
    map
}

/// The oracle's per-literal re-encode of the committed verbatim fixture, or `None`
/// after a loud `SKIP:` line when no OTP install is available (the install checks
/// reuse [`skip_reason`]); it never fails and never passes silently.
fn fixture_oracle_reencode() -> Option<BTreeMap<usize, Vec<u8>>> {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            eprintln!(
                "SKIP: the map-bearing literal comparison needs the OTP oracle — the committed \
                 fixtures' deterministic checks (payload counts, identical framings, byte-exact \
                 map-FREE literals) still ran; only the oracle-side assertion was skipped"
            );
            None
        }
        None => {
            let fixture =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/literals_verbatim.beam");
            Some(oracle_reencode(&fixture))
        }
    }
}

/// Whether `term` is a map, or a tuple/list that contains one (transitively).
///
/// A literal whose payload contains a map is not asserted to re-encode
/// **byte-exactly** to the compiler's bytes: ERTS normalizes a flat map's key
/// order on load, and the fixture is compiled with `+deterministic` (which writes a
/// different order), so `encode_term(decode_term(payload)) != payload` as soon as the
/// two orders differ. Such literals are compared against the **oracle's** own
/// re-encode of the same payload instead (and counted/reported by the install sweep,
/// whose semantic comparison is insensitive to a flat map's stored key order).
fn is_map_literal(term: Eterm, atoms: &beam_term::AtomTable) -> bool {
    if beam_term::map::map_size(term, atoms).is_ok() {
        return true;
    }
    if let Ok(arity) = tuple_arity(term) {
        return (0..arity)
            .filter_map(|i| tuple_element(term, i).ok())
            .any(|element| is_map_literal(element, atoms));
    }
    if let Ok(head) = cons_head(term) {
        if is_map_literal(head, atoms) {
            return true;
        }
        if let Ok(tail) = cons_tail(term) {
            return is_map_literal(tail, atoms);
        }
    }
    false
}

/// Decodes a hex string (lowercase, even length) into bytes.
fn from_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0, "odd-length hex: {hex:?}");
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex digits"))
        .collect()
}

/// Every literal of every installed OTP module: the decoded term must (1) re-encode
/// byte-exactly to the payload the compiler wrote and (2) be semantically equal to
/// the term the real `erl` oracle decodes from that same payload.
#[test]
fn installed_literal_pools_agree_with_the_oracle() {
    if let Some(reason) = skip_reason() {
        eprintln!("{reason}");
        return;
    }
    let Some(root) = otp_lib_dir() else {
        eprintln!(
            "SKIP: could not resolve the OTP `lib` root (need `erl` on PATH or $ERTS_LIB_DIR) — \
             the installed-literal sweep requires the installed OTP `.beam` files"
        );
        return;
    };
    let mut beams = Vec::new();
    collect_beams(&root, &mut beams);
    beams.sort();
    assert!(!beams.is_empty(), "no `.beam` files found under {}", root.display());

    let oracle = oracle_literals(&root);

    let mut verbatim_modules = 0usize;
    let mut compressed_modules = 0usize;
    let mut literal_modules = 0usize;
    let mut compared = 0usize;
    let mut synthesized = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for path in &beams {
        let label = rel_label(&root, path);
        let Ok(bytes) = std::fs::read(path) else {
            failures.push(format!("{label}: unreadable"));
            continue;
        };
        let Some(chunk) = litt_chunk(&bytes) else {
            continue;
        };
        match lit_form(chunk) {
            LitForm::Verbatim => verbatim_modules += 1,
            LitForm::Compressed => compressed_modules += 1,
        }
        let payloads = beam_loader::chunk::literal::parse(chunk)
            .unwrap_or_else(|error| panic!("{label}: the LitT framing must parse: {error:?}"));
        let module = beam_loader::load(&bytes)
            .unwrap_or_else(|error| panic!("{label}: the module must load: {error:?}"));
        literal_modules += 1;
        assert!(
            module.literals().len() >= payloads.len(),
            "{label}: {} decoded literals for {} LitT payloads",
            module.literals().len(),
            payloads.len()
        );

        // The module's own atom table, extended with whatever the oracle's canonical
        // image names (the same atoms, so the indices stay aligned with the module's).
        let mut atoms = module.atom_table.clone();
        for (index, payload) in payloads.iter().enumerate() {
            let term = literal(&module, index);

            // (1) The Rust decode -> encode round-trip reproduces the payload exactly,
            //     except for a map-containing literal, whose payload carries the
            //     compiler's (non-canonical) flat-map key order that ERTS normalizes.
            match encode_term(term, &module.atom_table) {
                Ok(encoded) if &encoded == payload => {}
                Ok(_) if is_map_literal(term, &module.atom_table) => {
                    synthesized += 1;
                }
                Ok(encoded) => failures.push(format!(
                    "{label} literal {index}: re-encoded to {} bytes, the payload is {} bytes",
                    encoded.len(),
                    payload.len()
                )),
                Err(error) => {
                    failures.push(format!("{label} literal {index}: encode failed: {error:?}"))
                }
            }

            // (2) The decoded term is semantically the oracle's term for that payload.
            let key = (label.clone(), index);
            let Some(canonical) = oracle.get(&key) else {
                failures.push(format!("{label} literal {index}: missing from the oracle dump"));
                continue;
            };
            let mut heap = match Heap::new(canonical.len().saturating_mul(4).saturating_add(1024))
            {
                Ok(heap) => heap,
                Err(error) => {
                    failures.push(format!(
                        "{label} literal {index}: could not allocate an oracle heap: {error:?}"
                    ));
                    continue;
                }
            };
            match decode_term(canonical, &mut heap, &mut atoms) {
                Ok(oracle_term) => {
                    if cmp(term, oracle_term, &module.atom_table) != core::cmp::Ordering::Equal {
                        failures.push(format!(
                            "{label} literal {index}: the decoded term disagrees with the oracle's"
                        ));
                    }
                }
                Err(error) => failures.push(format!(
                    "{label} literal {index}: the oracle image did not decode: {error:?}"
                )),
            }
            compared += 1;
        }
    }

    let mut summary = String::new();
    write!(
        summary,
        "LITERALS: {} module(s) with a LitT chunk ({} verbatim, {} compressed), \
         {} literal(s) decoded and compared against the oracle; \
         {} literal(s) whose payload bytes are the compiler's non-canonical map order",
        literal_modules, verbatim_modules, compressed_modules, compared, synthesized
    )
    .expect("writing to a String cannot fail");
    println!("{summary}");
    if compressed_modules == 0 {
        eprintln!(
            "NOTE: this OTP install carries no compressed-`LitT` module; the committed \
             `fixtures/literals_compressed.beam` covers the compressed framing deterministically"
        );
    }

    assert_eq!(
        compared,
        oracle.len(),
        "the sweep decoded {compared} literals but the oracle reported {} — the two walks \
         must agree on the discovered modules",
        oracle.len()
    );
    assert!(compared > 0, "the sweep must find at least one literal to compare");
    assert!(verbatim_modules > 0, "the install must have at least one verbatim-LitT module");
    assert!(
        failures.is_empty(),
        "{} literal(s) disagreed with the oracle; first 20:\n{:#?}",
        failures.len(),
        failures.iter().take(20).collect::<Vec<_>>()
    );
}
