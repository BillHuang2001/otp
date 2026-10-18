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

//! `Tag::covered()` mechanically pinned against the decoder — both directions.
//!
//! `Tag::covered()` claims "the M1 term layer supports this tag" (the crate's tag
//! table). This file makes that claim **executable**, in two directions:
//!
//! 1. every tag `covered()` reports `true` for decodes a **minimal valid image** — so
//!    a tag cannot be claimed covered while its `Decoder::decode_inner` arm is a stub
//!    or missing; and
//! 2. every tag `covered()` reports `false` for is a **typed `BadArg`** (never a
//!    panic, never `unsupported`, never a term) — so an "uncovered" tag cannot be
//!    silently half-implemented.
//!
//! Plus, live (skipped loudly without an oracle), direction 3: every tag the *system*
//! `erl`'s own encoder produces for a curated corpus is a covered tag — so nothing the
//! oracle's encoder can emit is missing from the covered set.
//!
//! The odd tags — `COMPRESSED`/`LOCAL_EXT` (top-level wrappers, not terms),
//! `ATOM_CACHE_REF` (needs a distribution cache) — are handled in their own arms with
//! the context they require; the remaining tags use the plain `decode_term` entry.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_term::prelude::{make_atom, make_small, AtomTable, Eterm, NIL};

use beam_codec::compress::decompress;
use beam_codec::term::{
    make_big, make_bitstring, make_external_pid, make_external_port, make_external_ref, make_float,
    make_fun_external, make_fun_imported, make_map, make_record, FunPid,
};
use beam_codec::{
    decode_term, decode_term_with_local_node, encode_term, encode_term_body,
    encode_term_compressed, encode_term_local, encode_term_with_minor_version, AtomCache,
    AtomCacheWriter, AtomEncoding, Decoder, Encoder, MinorVersion, Tag, ATOM_CACHE_REF_EXT,
    ATOM_EXT, ATOM_INTERNAL_REF2_EXT, ATOM_INTERNAL_REF3_EXT, ATOM_UTF8_EXT, BINARY_EXT,
    BIT_BINARY_EXT, COMPRESSED_EXT, DIST_HEADER_EXT, EXPORT_EXT, FLOAT_EXT, FUN_EXT, INTEGER_EXT,
    LARGE_BIG_EXT, LARGE_TUPLE_EXT, LIST_EXT, LOCAL_EXT, MAP_EXT, NEWER_REFERENCE_EXT,
    NEW_CACHE_EXT, NEW_FLOAT_EXT, NEW_FUN_EXT, NEW_PID_EXT, NEW_PORT_EXT, NEW_REFERENCE_EXT,
    NIL_EXT, PID_EXT, PORT_EXT, RECORD_EXT, REFERENCE_EXT, SMALL_ATOM_EXT, SMALL_ATOM_UTF8_EXT,
    SMALL_BIG_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, STRING_EXT, V4_PORT_EXT, VERSION_MAGIC,
};

/// The per-tag minimal images live in a shared submodule so `tag_space_audit.rs`'s
/// 256-byte sweep reuses them instead of re-deriving 37 hand-written wire layouts.
mod tag_images;

use tag_images::minimal_image;

/// Decode a `term_to_binary` image, returning the typed error on failure.
fn decode(image: &[u8]) -> Result<(), BeamError> {
    let mut heap = Heap::new(1 << 14).unwrap();
    let mut atoms = AtomTable::new();
    decode_term(image, &mut heap, &mut atoms).map(|_| ())
}

/// Every `Tag` with `covered() == true` decodes a minimal valid image.
#[test]
fn every_covered_tag_decodes() {
    let mut checked = Vec::new();
    for tag in ALL_TAGS {
        if !tag.covered() {
            continue;
        }
        match tag {
            // `COMPRESSED` is covered **only** as the top-level wrapper: the plain
            // `decode_term` unwraps it before the term walker, so the minimal image is
            // an empty zlib stream wrapping a `[97, 0]` body.
            Tag::Compressed => {
                let body = [97u8, 0];
                let zlib = beam_codec::compress::compress(&body, 6).unwrap();
                let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
                image.extend_from_slice(&(body.len() as u32).to_be_bytes());
                image.extend_from_slice(&zlib);
                decode(&image).unwrap_or_else(|e| panic!("COMPRESSED image rejected: {e:?}"));
            }
            // `LOCAL_EXT` is the `[local]` wrapper: 4-byte hash then the term.
            Tag::Local => {
                let image = [VERSION_MAGIC, LOCAL_EXT, 0, 0, 0, 0, 97, 7];
                decode(&image).unwrap_or_else(|e| panic!("LOCAL_EXT image rejected: {e:?}"));
            }
            // `ATOM_CACHE_REF` needs a distribution cache installed.
            Tag::AtomCacheRef => {
                let mut atoms = AtomTable::new();
                let a = atoms.intern("cached");
                let mut cache = AtomCache::new();
                cache.set(0, a);
                let mut heap = Heap::new(64).unwrap();
                let mut decoder = Decoder::new(&[VERSION_MAGIC, 82, 0])
                    .unwrap()
                    .with_atom_cache(&cache);
                let term = decoder
                    .decode(&mut heap, &mut atoms)
                    .unwrap_or_else(|e| panic!("ATOM_CACHE_REF image rejected: {e:?}"));
                assert_eq!(atoms.name(beam_term::prelude::atom_val(term)), Some("cached"));
            }
            // `ATOM_INTERNAL_REF2` (`'I'`, 73) / `ATOM_INTERNAL_REF3` (`'K'`, 75) name
            // an atom by table **index**, so they only decode against a table that
            // already holds that index. A fresh `erl` answers
            // `binary_to_term(<<131,73,0,5>>)` with the atom at index 5 (`''`) —
            // reproduced here by seeding 6 names and reading index 5 back.
            Tag::AtomInternalRef2 | Tag::AtomInternalRef3 => {
                let mut atoms = AtomTable::new();
                atoms.seed(["z0", "z1", "z2", "z3", "z4", "indexed"]);
                let image = if *tag == Tag::AtomInternalRef2 {
                    vec![VERSION_MAGIC, ATOM_INTERNAL_REF2_EXT, 0, 5]
                } else {
                    vec![VERSION_MAGIC, ATOM_INTERNAL_REF3_EXT, 0, 0, 5]
                };
                let mut heap = Heap::new(64).unwrap();
                let term = decode_term(&image, &mut heap, &mut atoms)
                    .unwrap_or_else(|e| panic!("{tag:?} image rejected: {e:?}"));
                assert_eq!(atoms.name(beam_term::prelude::atom_val(term)), Some("indexed"));
            }
            _ => {
                let image = minimal_image(*tag).expect("a non-contextual covered tag has an image");
                decode(&image).unwrap_or_else(|e| {
                    panic!("{tag:?} claims covered() but its minimal image was rejected: {e:?}")
                });
            }
        }
        checked.push(tag.name());
    }
    assert!(
        checked.len() >= 30,
        "only {} covered tags were exercised — the tag table shrank unexpectedly: {checked:?}",
        checked.len()
    );
    eprintln!(
        "[beam_codec tag_coverage] {} covered tags each decoded a minimal valid image",
        checked.len()
    );
}

/// Every `Tag` with `covered() == false` is a typed `BadArg` — never a term, never
/// `unsupported`, never a panic. `COMPRESSED`/`LOCAL_EXT`/`ATOM_CACHE_REF` are covered
/// (handled above); the three uncovered tags are `FUN_EXT`, `NEW_CACHE`, `DIST_HEADER`.
#[test]
fn every_uncovered_tag_is_a_typed_bad_arg() {
    let mut checked = 0usize;
    for tag in ALL_TAGS {
        if tag.covered() {
            continue;
        }
        // Both a bare tag byte and a tag nested as a 1-tuple's element.
        for image in [
            vec![VERSION_MAGIC, tag.as_byte()],
            vec![VERSION_MAGIC, 104, 1, tag.as_byte()],
        ] {
            match decode(&image) {
                Err(BeamError::BadArg) => {}
                Err(other) => panic!("{tag:?}: expected BadArg, got {other:?}"),
                Ok(()) => panic!("{tag:?} claims uncovered() but decoded a term"),
            }
        }
        checked += 1;
    }
    assert_eq!(
        checked, 3,
        "expected exactly FUN_EXT, NEW_CACHE, DIST_HEADER to be uncovered"
    );
}

/// Every `Tag` variant, so a new variant forces this table to be updated.
const ALL_TAGS: &[Tag] = &[
    Tag::SmallInteger,
    Tag::Integer,
    Tag::Float,
    Tag::Atom,
    Tag::Reference,
    Tag::Port,
    Tag::Pid,
    Tag::SmallTuple,
    Tag::LargeTuple,
    Tag::Nil,
    Tag::String,
    Tag::List,
    Tag::Binary,
    Tag::SmallBig,
    Tag::LargeBig,
    Tag::NewFloat,
    Tag::NewReference,
    Tag::SmallAtom,
    Tag::Map,
    Tag::NewPid,
    Tag::NewPort,
    Tag::NewerReference,
    Tag::AtomUtf8,
    Tag::SmallAtomUtf8,
    Tag::Compressed,
    Tag::BitBinary,
    Tag::NewFun,
    Tag::Export,
    Tag::Fun,
    Tag::V4Port,
    Tag::Local,
    Tag::AtomCacheRef,
    Tag::AtomInternalRef2,
    Tag::AtomInternalRef3,
    Tag::DistHeader,
    Tag::NewCache,
    Tag::Record,
];

// ---------------------------------------------------------------------------
// Direction 3 (live): nothing the oracle's encoder emits is missing
// ---------------------------------------------------------------------------

/// Resolve the oracle: `$ERTS_ORACLE` if it is an existing executable, else `erl` on
/// `$PATH` — the same policy as `tools/differential` and `tests/interop.rs`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec tag_coverage] $ERTS_ORACLE='{}' is not an existing executable — \
             treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(o) => Some(o),
        None => {
            eprintln!(
                "[beam_codec tag_coverage] {test}: SKIPPED — no `erl`/`$ERTS_ORACLE` on this \
                 machine; the live producible-tag cross-check did not run (NOT a pass)."
            );
            None
        }
    }
}

fn run_erl(oracle: &Path, eval: &str) -> String {
    let out = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .expect("spawn the oracle");
    assert!(
        out.status.success(),
        "the oracle exited {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("the oracle prints UTF-8")
}

/// Parse the `[<<131,104,...>>,<<...>>]` list the oracle prints into byte vectors
/// (the same scanner as `tests/interop.rs`).
fn parse_binary_list(out: &str) -> Vec<Vec<u8>> {
    let bytes = out.as_bytes();
    let mut images = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'<' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'>' && bytes[j + 1] == b'>') {
                j += 1;
            }
            assert!(j + 1 < bytes.len(), "unterminated <<...>> in {out:?}");
            let body = &out[start..j];
            let image = if body.trim().is_empty() {
                Vec::new()
            } else {
                body.split(',')
                    .map(|t| {
                        t.trim()
                            .parse::<u8>()
                            .unwrap_or_else(|_| panic!("bad byte {t:?} in {out:?}"))
                    })
                    .collect()
            };
            images.push(image);
            i = j + 2;
        } else {
            i += 1;
        }
    }
    images
}

/// A curated corpus whose **top-level** tags span every tag the system encoder can
/// produce (including the only-minor-version forms). Each image's top-level tag must
/// be a *covered* tag.
#[test]
fn every_tag_the_oracle_emits_is_covered() {
    let Some(oracle) = oracle_or_skip("every_tag_the_oracle_emits_is_covered") else {
        return;
    };
    // `130` copies of `U+03BB` is a legal atom (130 characters) whose UTF-8 name is 260
    // bytes — over the 255-byte `SMALL_ATOM_UTF8` ceiling, so the oracle must use the
    // 16-bit-length `ATOM_UTF8_EXT` (`118`).
    let corpus = r#"[ term_to_binary(5)
         , term_to_binary(200000)
         , term_to_binary(-200000)
         , term_to_binary(12345678901234567890123)
         , term_to_binary(1 bsl 2000)
         , term_to_binary(1.5, [{minor_version,0}])
         , term_to_binary(1.5)
         , term_to_binary('a', [{minor_version,0}])
         , term_to_binary('a')
         , term_to_binary(list_to_atom(lists:duplicate(130, 955)))
         , term_to_binary([])
         , term_to_binary([1,2,3])
         , term_to_binary([1,2,300])
         , term_to_binary(<<"abc">>)
         , term_to_binary(<<1:3>>)
         , term_to_binary(#{1=>2})
         , term_to_binary({a,b})
         , term_to_binary(list_to_tuple(lists:duplicate(300, x)))
         , term_to_binary(self())
         , term_to_binary(hd(erlang:ports()))
         , term_to_binary(make_ref())
         , term_to_binary(fun lists:reverse/1)
         , term_to_binary(begin X=1, fun() -> X end end)
         , term_to_binary(lists:duplicate(500, hello), [compressed])
       ]"#;
    let eval = format!("io:format(\"~w~n\", [ {corpus} ])");
    let printed = run_erl(&oracle, &eval);
    let images = parse_binary_list(&printed);
    assert!(
        images.len() >= 22,
        "the oracle printed {} images: {printed:?}",
        images.len()
    );

    let mut seen: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    for bi in &images {
        assert_eq!(bi[0], VERSION_MAGIC, "an oracle image lacks the version byte: {bi:?}");
        let tag_byte = bi[1];
        let tag = Tag::from_byte(tag_byte)
            .unwrap_or_else(|| panic!("the oracle emitted the unknown tag byte {tag_byte}: {bi:?}"));
        assert!(
            minimal_image(tag).is_some()
                || matches!(
                    tag,
                    Tag::Compressed
                        | Tag::Local
                        | Tag::AtomCacheRef
                        | Tag::AtomInternalRef2
                        | Tag::AtomInternalRef3
                ),
            "the oracle emitted {tag:?}, which has no minimal image — update the coverage table"
        );
        assert!(
            tag.covered(),
            "the oracle emitted {} ({tag:?}), which this decoder reports as NOT covered — \
             a producible tag is missing: {bi:?}",
            tag.name()
        );
        seen.insert(tag.name());
    }
    eprintln!(
        "[beam_codec tag_coverage] the oracle emitted {} distinct top-level tags, all covered: {seen:?}",
        seen.len()
    );
}

// ===========================================================================
// TABLE 1 — per-tag encode + decode + round-trip, pinned to oracle bytes
// ===========================================================================
//
// For every *producible* tag this table pairs a Rust-constructed term (or a
// hand-written decode-only wire image) with our `Encoder` output, then asserts that
// `encode(decode(image))` is **byte-identical** to `image` — or, for the legacy
// decode-only forms our encoder always rewrites, that it is the modern form.
//
// A representative, VM-start-independent subset is additionally cross-checked LIVE
// against the oracle's own `term_to_binary/1` (`representative_tags_match_the_oracle_byte_for_byte`),
// and the three tags the oracle cannot *produce* are pinned offline
// (`wrapper_tags_80_121_82_round_trip` and `record_67_...`).
//
// **Atom-table seeding hazard.** A flat map's atom keys sort by atom-table index,
// which depends on interning order, so the *same* atom-keyed map can produce
// different bytes in two fresh VMs. Every cross-VM byte comparison below therefore
// uses integer-/binary-keyed maps (or atom names carried as bytes on the wire), never
// an atom-keyed map.

/// A term heap of `words` words (the safe public allocator).
fn new_heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

/// The `SMALL_ATOM_UTF8_EXT` `"n1"` node atom used by the legacy identifier forms.
const NODE_N1: [u8; 4] = [119, 2, b'n', b'1'];

/// `[VERSION_MAGIC] ++ tail` — the `term_to_binary` envelope.
fn wi(tail: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(tail.len() + 1);
    v.push(VERSION_MAGIC);
    v.extend_from_slice(tail);
    v
}

/// The `term_to_binary/2` wire flavour an image is (re-)produced under.
#[derive(Clone, Copy, Debug)]
enum Flavour {
    /// Default `MinorVersion::V2` / UTF-8 — exactly what `encode_term` emits.
    Default,
    /// `{minor_version, 0}`: Latin-1 `ATOM_EXT` (`100`) atoms and legacy `FLOAT_EXT` (`99`).
    MinorV0,
    /// The distribution Latin-1 atom flavour (`115`/`100`) under `V2`.
    Latin1,
}

/// Encodes `term` under `flavour` (the per-row re-encode entry point).
fn encode_with(term: Eterm, atoms: &AtomTable, flavour: Flavour) -> Vec<u8> {
    match flavour {
        Flavour::Default => encode_term(term, atoms).expect("encode"),
        Flavour::MinorV0 => {
            encode_term_with_minor_version(term, atoms, MinorVersion::V0).expect("encode")
        }
        Flavour::Latin1 => {
            let mut encoder = Encoder::new(atoms);
            encoder.set_atom_encoding(AtomEncoding::Latin1);
            encoder.encode(term).expect("encode");
            encoder.finish()
        }
    }
}

/// One row of the per-tag table.
struct TagCase {
    label: &'static str,
    /// The tag byte the **input** image carries.
    tag: u8,
    /// Our encoder's canonical image, or a hand-written decode-only image.
    image: Vec<u8>,
    /// The flavour the decoded term is re-encoded under.
    flavour: Flavour,
    /// The tag byte the **re-encoded** image carries (the modern form for a decode-only row).
    out_tag: u8,
    /// Whether `encode(decode(image))` must equal `image` byte-for-byte.
    byte_exact: bool,
    /// Atom names the decoder's table must already hold **before** the image is
    /// decoded. Empty for every ordinary row; non-empty only for the index-carrying
    /// `ATOM_INTERNAL_REF2`/`ATOM_INTERNAL_REF3` forms, which name an existing atom.
    seed: &'static [&'static str],
}

/// A canonical row: `term` → our encoder's image under `flavour`, tagged `tag`.
fn canonical(label: &'static str, term: Eterm, atoms: &AtomTable, flavour: Flavour, tag: u8) -> TagCase {
    let image = encode_with(term, atoms, flavour);
    assert_eq!(image[0], VERSION_MAGIC, "{label}: version byte");
    assert_eq!(
        image[1], tag,
        "{label}: our encoder emitted tag {} but the row pins {tag}",
        image[1]
    );
    TagCase { label, tag, image, flavour, out_tag: tag, byte_exact: true, seed: &[] }
}

/// A decode-only row: a hand-written legacy image whose re-encode is the modern form.
fn decode_only(label: &'static str, tag: u8, image: Vec<u8>, out_tag: u8) -> TagCase {
    decode_only_seeded(label, tag, image, out_tag, &[])
}

/// A decode-only row that needs atoms already present in the decoding table — the
/// index-carrying `ATOM_INTERNAL_REF2` (`73`) / `ATOM_INTERNAL_REF3` (`75`) forms name
/// an atom by index, so the table must be seeded before the image is decoded.
fn decode_only_seeded(
    label: &'static str,
    tag: u8,
    image: Vec<u8>,
    out_tag: u8,
    seed: &'static [&'static str],
) -> TagCase {
    assert_eq!(image[0], VERSION_MAGIC, "{label}: version byte");
    assert_eq!(image[1], tag, "{label}: input tag");
    TagCase { label, tag, image, flavour: Flavour::Default, out_tag, byte_exact: false, seed }
}

/// `[VERSION_MAGIC][101][node][id:4 BE][creation:1]` — a legacy `REFERENCE_EXT`.
fn legacy_reference_image(id: u32, creation: u8) -> Vec<u8> {
    let mut image = wi(&[REFERENCE_EXT]);
    image.extend_from_slice(&NODE_N1);
    image.extend_from_slice(&id.to_be_bytes());
    image.push(creation);
    image
}

/// `[VERSION_MAGIC][102][node][id:4 BE][creation:1]` — a legacy `PORT_EXT`.
fn legacy_port_image(id: u32, creation: u8) -> Vec<u8> {
    let mut image = wi(&[PORT_EXT]);
    image.extend_from_slice(&NODE_N1);
    image.extend_from_slice(&id.to_be_bytes());
    image.push(creation);
    image
}

/// `[VERSION_MAGIC][103][node][number:4 BE][serial:4 BE][creation:1]` — a legacy `PID_EXT`.
fn legacy_pid_image(number: u32, serial: u32, creation: u8) -> Vec<u8> {
    let mut image = wi(&[PID_EXT]);
    image.extend_from_slice(&NODE_N1);
    image.extend_from_slice(&number.to_be_bytes());
    image.extend_from_slice(&serial.to_be_bytes());
    image.push(creation);
    image
}

/// `[VERSION_MAGIC][114][len:2][node][creation:1][ids:len×4]` — a legacy `NEW_REFERENCE_EXT`.
fn new_reference_image(ids: &[u32], creation: u8) -> Vec<u8> {
    let mut image = wi(&[NEW_REFERENCE_EXT]);
    image.extend_from_slice(&(ids.len() as u16).to_be_bytes());
    image.extend_from_slice(&NODE_N1);
    image.push(creation);
    for id in ids {
        image.extend_from_slice(&id.to_be_bytes());
    }
    image
}

/// `[VERSION_MAGIC][89][node][id:4 BE][creation:4 BE]` — a legacy `NEW_PORT_EXT`.
fn new_port_image(id: u32, creation: u32) -> Vec<u8> {
    let mut image = wi(&[NEW_PORT_EXT]);
    image.extend_from_slice(&NODE_N1);
    image.extend_from_slice(&id.to_be_bytes());
    image.extend_from_slice(&creation.to_be_bytes());
    image
}

/// `[VERSION_MAGIC][99][text NUL-padded to 31 bytes]` — a legacy `FLOAT_EXT`.
fn legacy_float_image(text: &[u8]) -> Vec<u8> {
    let mut image = wi(&[FLOAT_EXT]);
    image.extend_from_slice(text);
    image.resize(2 + 31, 0);
    image
}

/// The per-tag table: one row per producible External Term Format tag.
fn tag_cases() -> Vec<TagCase> {
    let mut cases: Vec<TagCase> = Vec::new();

    // ---- scalars, atoms, bignums ----------------------------------------
    {
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let mut h = new_heap(1 << 12);

        cases.push(canonical("small_integer_97", make_small(5), &atoms, Flavour::Default, SMALL_INTEGER_EXT));
        cases.push(canonical("integer_98", make_small(1000), &atoms, Flavour::Default, INTEGER_EXT));
        cases.push(canonical(
            "new_float_70",
            make_float(&mut h, 1.5).unwrap(),
            &atoms,
            Flavour::Default,
            NEW_FLOAT_EXT,
        ));
        // The legacy 31-byte `FLOAT_EXT` is producible only under `{minor_version,0}`.
        cases.push(canonical(
            "float_99_under_v0",
            make_float(&mut h, 1.5).unwrap(),
            &atoms,
            Flavour::MinorV0,
            FLOAT_EXT,
        ));

        // Atoms: `ATOM_EXT` (100) under V0, Latin-1 `SMALL_ATOM_EXT` (115), the UTF-8
        // forms and the 255-character/255-byte boundary.
        cases.push(canonical("atom_100_under_v0", a, &atoms, Flavour::MinorV0, ATOM_EXT));
        cases.push(canonical("atom_115_latin1", a, &atoms, Flavour::Latin1, SMALL_ATOM_EXT));
        cases.push(canonical("atom_119_utf8", a, &atoms, Flavour::Default, SMALL_ATOM_UTF8_EXT));
        // Exactly 255 characters (and 255 bytes): still the 1-byte-length `SMALL_ATOM_UTF8`.
        let a255 = make_atom(atoms.intern(&"a".repeat(255)));
        cases.push(canonical("atom_119_255_chars", a255, &atoms, Flavour::Default, SMALL_ATOM_UTF8_EXT));
        // 130 copies of U+03BB: a legal 130-character name whose UTF-8 image is 260
        // bytes — over the 255-byte `SMALL_ATOM_UTF8` ceiling, so tag 118.
        let wide = make_atom(atoms.intern(&"\u{03bb}".repeat(130)));
        cases.push(canonical("atom_118_wide_utf8", wide, &atoms, Flavour::Default, ATOM_UTF8_EXT));

        // Bignums across the `< 256` / `>= 256` significant-byte switchover.
        let small_big = make_big(&mut h, false, &[1u64 << 63]).unwrap(); // 8 significant bytes ⇒ 110
        cases.push(canonical("small_big_110", small_big, &atoms, Flavour::Default, SMALL_BIG_EXT));
        let neg_big = make_big(&mut h, true, &[1u64 << 63]).unwrap();
        cases.push(canonical("small_big_110_negative", neg_big, &atoms, Flavour::Default, SMALL_BIG_EXT));
        let mut digits = vec![0u64; 33];
        digits[32] = 1; // 1 << 2048 ⇒ 257 significant bytes ⇒ 111
        let large_big = make_big(&mut h, false, &digits).unwrap();
        cases.push(canonical("large_big_111", large_big, &atoms, Flavour::Default, LARGE_BIG_EXT));
    }

    // ---- containers, funs, identifiers, maps, records --------------------
    {
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let b = make_atom(atoms.intern("b"));
        let m_atom = atoms.intern("m");
        let r_atom = atoms.intern("r");
        let lists_atom = atoms.intern("lists");
        let reverse_atom = atoms.intern("reverse");
        let node = make_atom(atoms.intern("nonode@nohost"));
        let mut h = new_heap(1 << 18);

        // Tuples and NIL.
        let tup = h.tuple(&[a, b]).unwrap();
        cases.push(canonical("small_tuple_104", tup, &atoms, Flavour::Default, SMALL_TUPLE_EXT));
        let wide_tuple = h.tuple(&vec![a; 300]).unwrap();
        cases.push(canonical("large_tuple_105", wide_tuple, &atoms, Flavour::Default, LARGE_TUPLE_EXT));
        cases.push(canonical("nil_106", NIL, &atoms, Flavour::Default, NIL_EXT));

        // Lists: a byte list (`STRING_EXT`) and improper/non-byte lists (`LIST_EXT`).
        let mut byte_list = NIL;
        for v in [3i64, 2, 1] {
            byte_list = h.cons(make_small(v), byte_list).unwrap();
        }
        cases.push(canonical("string_107", byte_list, &atoms, Flavour::Default, STRING_EXT));
        let improper = h.cons(make_small(1), make_small(2)).unwrap();
        cases.push(canonical("list_108_improper_tail", improper, &atoms, Flavour::Default, LIST_EXT));
        let non_byte = h.cons(make_small(300), NIL).unwrap();
        cases.push(canonical("list_108_non_byte", non_byte, &atoms, Flavour::Default, LIST_EXT));

        // Binaries and bitstrings (a byte-aligned and a non-byte-aligned tail).
        let binary = make_bitstring(&mut h, b"abc", 24).unwrap();
        cases.push(canonical("binary_109", binary, &atoms, Flavour::Default, BINARY_EXT));
        let bits = make_bitstring(&mut h, &[0b1010_1010], 3).unwrap();
        cases.push(canonical("bit_binary_77", bits, &atoms, Flavour::Default, BIT_BINARY_EXT));

        // Funs: an `EXPORT_EXT` and an imported `NEW_FUN_EXT` (with an environment).
        let export = make_fun_external(&mut h, 1, lists_atom, reverse_atom).unwrap();
        cases.push(canonical("export_113", export, &atoms, Flavour::Default, EXPORT_EXT));
        let creator = FunPid { node: atoms.intern("n"), number: 10, serial: 20, creation: 0 };
        let imported =
            make_fun_imported(&mut h, 2, m_atom, &[0u8; 16], 0, 0, 0, creator, &[make_small(7)]).unwrap();
        cases.push(canonical("new_fun_112", imported, &atoms, Flavour::Default, NEW_FUN_EXT));

        // Identifiers — our encoder always emits the modern `88`/`120`/`90` forms.
        let pid = make_external_pid(&mut h, node, 10, 20, 0).unwrap();
        cases.push(canonical("new_pid_88", pid, &atoms, Flavour::Default, NEW_PID_EXT));
        let port = make_external_port(&mut h, node, 7, 0).unwrap();
        cases.push(canonical("v4_port_120", port, &atoms, Flavour::Default, V4_PORT_EXT));
        let rf = make_external_ref(&mut h, node, &[42], 0).unwrap();
        cases.push(canonical("newer_reference_90", rf, &atoms, Flavour::Default, NEWER_REFERENCE_EXT));

        // Maps: a flat integer-keyed map (≤ 32) and a > 32-pair HAMT.
        let flat = make_map(
            &mut h,
            &[(make_small(2), make_small(2)), (make_small(3), make_small(3)), (make_small(10), make_small(1))],
            &atoms,
        )
        .unwrap();
        cases.push(canonical("map_116_flat", flat, &atoms, Flavour::Default, MAP_EXT));
        let pairs: Vec<(Eterm, Eterm)> =
            (1..=40i64).map(|n| (make_small(n), make_small(n * 2))).collect();
        let hamt = make_map(&mut h, &pairs, &atoms).unwrap();
        cases.push(canonical("map_116_hamt", hamt, &atoms, Flavour::Default, MAP_EXT));

        // A native record. `term_to_binary/1` never emits `67` (an Erlang record is a
        // tagged tuple), so this image is pinned offline and the oracle is only asked to
        // *accept* it (`oracle_accepts_our_record_and_new_fun_images`).
        let fields = [(b, make_small(22)), (a, make_small(11))];
        let record = make_record(&mut h, &atoms, m_atom, r_atom, false, &fields).unwrap();
        cases.push(canonical("record_67", record, &atoms, Flavour::Default, RECORD_EXT));
    }

    // ---- legacy decode-only wire forms (our encoder rewrites them) -------
    cases.push(decode_only("reference_101_legacy", REFERENCE_EXT, legacy_reference_image(42, 0), NEWER_REFERENCE_EXT));
    cases.push(decode_only("new_reference_114_legacy", NEW_REFERENCE_EXT, new_reference_image(&[1], 0), NEWER_REFERENCE_EXT));
    cases.push(decode_only("port_102_legacy", PORT_EXT, legacy_port_image(7, 0), V4_PORT_EXT));
    cases.push(decode_only("new_port_89_legacy", NEW_PORT_EXT, new_port_image(7, 0), V4_PORT_EXT));
    cases.push(decode_only("pid_103_legacy", PID_EXT, legacy_pid_image(10, 20, 0), NEW_PID_EXT));
    cases.push(decode_only("float_99_legacy", FLOAT_EXT, legacy_float_image(b"1.5e+00"), NEW_FLOAT_EXT));
    cases.push(decode_only("atom_100_legacy", ATOM_EXT, wi(&[ATOM_EXT, 0, 1, b'a']), SMALL_ATOM_UTF8_EXT));
    cases.push(decode_only("atom_115_legacy", SMALL_ATOM_EXT, wi(&[SMALL_ATOM_EXT, 1, b'a']), SMALL_ATOM_UTF8_EXT));
    cases.push(decode_only("atom_118_legacy", ATOM_UTF8_EXT, wi(&[ATOM_UTF8_EXT, 0, 1, b'a']), SMALL_ATOM_UTF8_EXT));
    // Bignum leading-zero stripping: `[5, 0]` (little-endian) is `5` ⇒ a small; a
    // 9-byte `2^63` with a leading zero byte stays a `SMALL_BIG` of 8 bytes.
    cases.push(decode_only("small_big_leading_zero_to_small", SMALL_BIG_EXT, wi(&[SMALL_BIG_EXT, 2, 0, 5, 0]), SMALL_INTEGER_EXT));
    cases.push(decode_only(
        "small_big_leading_zero_kept",
        SMALL_BIG_EXT,
        wi(&[SMALL_BIG_EXT, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0]),
        SMALL_BIG_EXT,
    ));

    // ---- decode-only index-carrying atom forms ---------------------------
    // `ATOM_INTERNAL_REF2` (`'I'`, 73) / `ATOM_INTERNAL_REF3` (`'K'`, 75) name an atom
    // by atom-table index, which the C accepts unconditionally (`external.c:5177-5192`;
    // NOT gated on `internal_tags`, `external.c:6413-6434`). Verified against the
    // oracle: `binary_to_term(<<131,73,0,5>>)` → the atom at index 5, and likewise for
    // `<<131,75,0,0,5>>`. The table is seeded so the index exists; the re-encode is the
    // modern UTF-8 atom form, so these are decode-only rows.
    cases.push(decode_only_seeded(
        "atom_internal_ref2_73_index_1",
        ATOM_INTERNAL_REF2_EXT,
        wi(&[ATOM_INTERNAL_REF2_EXT, 0, 1]),
        SMALL_ATOM_UTF8_EXT,
        &["aaa", "bbb"],
    ));
    cases.push(decode_only_seeded(
        "atom_internal_ref3_75_index_1",
        ATOM_INTERNAL_REF3_EXT,
        wi(&[ATOM_INTERNAL_REF3_EXT, 0, 0, 1]),
        SMALL_ATOM_UTF8_EXT,
        &["aaa", "bbb"],
    ));

    cases
}

/// Every producible tag decodes, re-encodes to its pinned tag, and — for a canonical
/// row — re-encodes to the **exact** original bytes.
#[test]
fn every_producible_tag_round_trips_byte_exact() {
    let cases = tag_cases();
    assert!(
        cases.len() >= 30,
        "the per-tag table shrank to {} rows",
        cases.len()
    );

    let mut input_tags = std::collections::BTreeSet::new();
    let mut byte_exact = 0usize;
    for c in &cases {
        assert_eq!(c.image[0], VERSION_MAGIC, "{}: version byte", c.label);
        assert_eq!(c.image[1], c.tag, "{}: input tag", c.label);

        let mut heap = new_heap(1 << 18);
        let mut atoms = AtomTable::new();
        // The index-carrying rows name an atom their table must already hold; every
        // other row seeds nothing, so its decode starts from an empty table exactly as
        // before.
        atoms.seed(c.seed.iter().copied());
        let term = decode_term(&c.image, &mut heap, &mut atoms)
            .unwrap_or_else(|e| panic!("{}: our own image must decode: {e:?}", c.label));

        let out = encode_with(term, &atoms, c.flavour);
        assert_eq!(out[0], VERSION_MAGIC, "{}: re-encode version byte", c.label);
        assert_eq!(out[1], c.out_tag, "{}: re-encode tag", c.label);
        if c.byte_exact {
            assert_eq!(out, c.image, "{}: decode → re-encode must be byte-identical", c.label);
            byte_exact += 1;
        } else {
            assert_ne!(out, c.image, "{}: a decode-only row must not round-trip byte-exactly", c.label);
        }
        input_tags.insert(c.tag);
    }
    assert!(
        input_tags.len() >= 24,
        "only {} distinct input tags were exercised: {input_tags:?}",
        input_tags.len()
    );
    eprintln!(
        "[beam_codec tag_coverage] table 1: {} rows, {} byte-exact, {} distinct input tags: {input_tags:?}",
        cases.len(),
        byte_exact,
        input_tags.len()
    );
}

/// The tags the oracle cannot *produce* (`COMPRESSED`/`LOCAL_EXT` wrappers and the
/// distribution-only `ATOM_CACHE_REF`) round-trip through their own seams.
#[test]
fn wrapper_tags_80_121_82_round_trip() {
    // 80 COMPRESSED — a term long enough that zlib shrinks it.
    let mut atoms = AtomTable::new();
    let hello = make_atom(atoms.intern("hello"));
    let mut h = new_heap(1 << 15);
    let mut list = NIL;
    for _ in 0..1000 {
        list = h.cons(hello, list).unwrap();
    }
    let plain_body = encode_term_body(list, &atoms).unwrap();
    let image = encode_term_compressed(list, &atoms, 6).unwrap();
    assert_eq!(image[0], VERSION_MAGIC);
    assert_eq!(image[1], COMPRESSED_EXT, "a 1000-element atom list must compress");
    let declared = u32::from_be_bytes(image[2..6].try_into().unwrap()) as usize;
    assert_eq!(declared, plain_body.len(), "the declared inner size is the plain body's length");
    assert_eq!(
        decompress(&image[6..]).unwrap(),
        plain_body,
        "the zlib stream inflates to the plain body"
    );
    // Decode the wrapper and re-encode plain: the inner body is reproduced exactly.
    let mut h2 = new_heap(1 << 15);
    let mut atoms2 = AtomTable::new();
    let term = decode_term(&image, &mut h2, &mut atoms2).expect("a COMPRESSED image decodes");
    assert_eq!(encode_term_body(term, &atoms2).unwrap(), plain_body);

    // 121 LOCAL_EXT — a 6-byte wrapper (`[131,121,hash:4]`) followed by the verbatim body.
    let hash = 0xDEAD_BEEFu32;
    let local = encode_term_local(list, &atoms, hash).unwrap();
    assert_eq!(local[0], VERSION_MAGIC);
    assert_eq!(local[1], LOCAL_EXT);
    assert_eq!(&local[2..6], hash.to_be_bytes().as_slice());
    assert_eq!(&local[6..], plain_body.as_slice());
    let mut h3 = new_heap(1 << 15);
    let mut atoms3 = AtomTable::new();
    let term3 = decode_term(&local, &mut h3, &mut atoms3).expect("a LOCAL_EXT image decodes");
    assert_eq!(encode_term_body(term3, &atoms3).unwrap(), plain_body);

    // 82 ATOM_CACHE_REF — needs the outbound writer *and* the inbound cache (`beam_dist`'s pair).
    let mut atoms = AtomTable::new();
    let a = make_atom(atoms.intern("a"));
    let b = make_atom(atoms.intern("b"));
    let mut h4 = new_heap(64);
    let tuple = h4.tuple(&[a, b, a, a]).unwrap();

    let mut writer = FixedIndexCache;
    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut writer);
    encoder.encode(tuple).unwrap();
    let payload = encoder.finish();
    assert_eq!(
        payload,
        vec![
            SMALL_TUPLE_EXT, 4,
            ATOM_CACHE_REF_EXT, 0,
            ATOM_CACHE_REF_EXT, 1,
            ATOM_CACHE_REF_EXT, 0,
            ATOM_CACHE_REF_EXT, 0,
        ],
        "every atom becomes a cache ref, in wire order"
    );

    let mut atoms2 = AtomTable::new();
    let mut cache = AtomCache::new();
    cache.set(0, atoms2.intern("a"));
    cache.set(1, atoms2.intern("b"));
    let mut h5 = new_heap(64);
    let mut decoder = Decoder::new_body(&payload).with_atom_cache(&cache);
    let decoded = decoder.decode(&mut h5, &mut atoms2).expect("the cached payload decodes");
    decoder.finish().unwrap();

    let mut writer2 = FixedIndexCache;
    let mut encoder2 = Encoder::new_body(&atoms2).with_atom_cache(&mut writer2);
    encoder2.encode(decoded).unwrap();
    assert_eq!(encoder2.finish(), payload, "an atom-cache payload round-trips byte-exactly");

    eprintln!("[beam_codec tag_coverage] wrapper tags 80/121/82 round-tripped");
}

/// A test [`AtomCacheWriter`] caching `a`/`b` at payload indices 0/1 (the `beam_dist` use).
struct FixedIndexCache;

impl AtomCacheWriter for FixedIndexCache {
    fn intern(&mut self, atom: beam_term::prelude::Atom, atoms: &AtomTable) -> Option<u8> {
        match atoms.name(atom) {
            Some("a") => Some(0),
            Some("b") => Some(1),
            _ => None,
        }
    }
}

// ===========================================================================
// TABLE 2 — malformed input → exact typed error class
// ===========================================================================
//
// One consolidated table pairing every malformed wire form with the **exact**
// [`beam_common::BeamError`] variant the decoder must return. The C routes *every*
// decode failure — bounds included — to `B2TBadArg` (`external.c:1989-1991`), so the
// class is `BadArg` for every malformed input; the *only* decode-side `SystemLimit` is
// a nesting depth past `MAX_DECODE_DEPTH` (a resource bound, not malformed input — see
// `tests/decode_depth.rs`). None of these panics and none returns `Ok`.

/// The exact error class a malformed image must produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    BadArg,
    SystemLimit,
}

/// One row of the malformed-input table.
struct Malformed {
    label: String,
    image: Vec<u8>,
    expect: Expect,
    /// Whether the image violates a rule that only fires when the decoder knows the
    /// **local node identity** (the `90` first-number bound of `src/decode/local_node.rs`),
    /// so the row must be decoded through `decode_term_with_local_node`.
    local_node: bool,
    /// Whether the system oracle rejects the exact image too (`badarg`) — the subset
    /// cross-checked live by `malformed_inputs_match_the_oracle`.
    oracle: bool,
}

/// A `BadArg` row, oracle-checked by default.
fn malformed(label: &str, image: Vec<u8>) -> Malformed {
    Malformed { label: label.to_string(), image, expect: Expect::BadArg, local_node: false, oracle: true }
}

/// A `BadArg` row that only fires with the local-node identity installed, and is thus
/// **not** oracle-checkable: the oracle's own local node is neither `n1` nor creation
/// `0`, so it classifies this image as a *remote* reference and accepts it.
fn malformed_local(label: &str, image: Vec<u8>) -> Malformed {
    Malformed { label: label.to_string(), image, expect: Expect::BadArg, local_node: true, oracle: false }
}

/// A `NEW_FUN_EXT` image with the given `num_free`, `old_index`, `old_uniq` and creator
/// wire tails, and no environment terms.
fn new_fun_image(num_free: u32, old_index: &[u8], old_uniq: &[u8], creator: &[u8]) -> Vec<u8> {
    let mut image = wi(&[NEW_FUN_EXT]);
    image.extend_from_slice(&[0, 0, 0, 0]); // size (read and ignored)
    image.push(2); // arity
    image.extend_from_slice(&[0u8; 16]); // uniq
    image.extend_from_slice(&[0, 0, 0, 0]); // index
    image.extend_from_slice(&num_free.to_be_bytes());
    image.extend_from_slice(&[119, 1, b'm']); // module atom
    image.extend_from_slice(old_index);
    image.extend_from_slice(old_uniq);
    image.extend_from_slice(creator);
    image
}

/// A `BadArg` row the oracle cannot cross-check: a `LOCAL_EXT` (`121`) image carries a
/// per-VM-start node hash, so a synthetic one is rejected by the oracle for the *hash*
/// rather than for the wire rule under test.
fn malformed_unoracleable(label: &str, image: Vec<u8>) -> Malformed {
    Malformed { label: label.to_string(), image, expect: Expect::BadArg, local_node: false, oracle: false }
}

/// A `NEW_FUN_EXT` **body** (no `VERSION_MAGIC`, so it can sit behind a `LOCAL_EXT`
/// wrapper) whose module-atom field is `module_byte` and whose `num_free` is 0.
fn new_fun_body_with_module(module_byte: u8) -> Vec<u8> {
    let mut image = vec![NEW_FUN_EXT];
    image.extend_from_slice(&[0, 0, 0, 0]); // size (read and ignored)
    image.push(2); // arity
    image.extend_from_slice(&[0u8; 16]); // uniq
    image.extend_from_slice(&[0, 0, 0, 0]); // index
    image.extend_from_slice(&[0, 0, 0, 0]); // num_free
    image.push(module_byte); // the module atom
    image.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_index (a small)
    image.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_uniq (a small)
    image.extend_from_slice(&CREATOR_PID);
    image
}

/// `[131] ++ [SMALL_TUPLE_EXT,1]*depth ++ [NIL]` — `depth` nested 1-tuples.
fn nested_tuple_image(depth: usize) -> Vec<u8> {
    let mut image = Vec::with_capacity(1 + depth * 2 + 1);
    image.push(VERSION_MAGIC);
    for _ in 0..depth {
        image.push(SMALL_TUPLE_EXT);
        image.push(1);
    }
    image.push(NIL_EXT);
    image
}

/// The `NEW_FUN_EXT` creator-pid wire tail: `NEW_PID_EXT`, node `"n"`, num/serial/
/// creation (`[88][node][number:4][serial:4][creation:4]` — the creation field is four
/// bytes, so a three-byte tail would truncate the header rather than test the rule).
const CREATOR_PID: [u8; 16] = [88, 119, 1, b'n', 0, 0, 0, 10, 0, 0, 0, 20, 0, 0, 0, 0];

/// The malformed-input table, one row per wire-form violation.
fn malformed_cases() -> Vec<Malformed> {
    let mut cases: Vec<Malformed> = Vec::new();

    // Unknown / obsolete / transport tags.
    cases.push(malformed("unknown_tag_00", wi(&[0x00])));
    cases.push(malformed("unknown_tag_ff", wi(&[0xFF])));
    cases.push(malformed("obsolete_fun_ext_117", wi(&[FUN_EXT, 0, 0, 0, 1])));
    cases.push(malformed("dead_new_cache_78", wi(&[NEW_CACHE_EXT, 0, 0])));
    cases.push(malformed("dist_header_68", wi(&[DIST_HEADER_EXT, 0])));

    // `ATOM_INTERNAL_REF2` (73) / `ATOM_INTERNAL_REF3` (75): the two forms the plain
    // decode *does* accept, so their malformed inputs are an index past the table and a
    // truncated index field. `decode_row` decodes with a fresh (empty) table, so any
    // index is out of range; the oracle rejects these same images with `badarg`
    // (verified: `binary_to_term(<<131,73,255,255>>)` and
    // `binary_to_term(<<131,75,255,255,255>>)` are `badarg`, while index `0`/`5` are
    // accepted — and the sibling `internal_tags`-gated tags `74`/`76` are `badarg`
    // too, so they stay unmodelled).
    cases.push(malformed("atom_internal_ref2_index_out_of_range", wi(&[ATOM_INTERNAL_REF2_EXT, 0xFF, 0xFF])));
    cases.push(malformed("atom_internal_ref2_index_truncated", wi(&[ATOM_INTERNAL_REF2_EXT, 0x00])));
    cases.push(malformed("atom_internal_ref2_no_index", wi(&[ATOM_INTERNAL_REF2_EXT])));
    cases.push(malformed(
        "atom_internal_ref3_index_out_of_range",
        wi(&[ATOM_INTERNAL_REF3_EXT, 0xFF, 0xFF, 0xFF]),
    ));
    cases.push(malformed("atom_internal_ref3_index_truncated", wi(&[ATOM_INTERNAL_REF3_EXT, 0x00, 0x00])));
    cases.push(malformed("atom_internal_ref3_no_index", wi(&[ATOM_INTERNAL_REF3_EXT])));
    // Every truncation of a set of small valid images.
    for (name, image) in [
        ("small", vec![VERSION_MAGIC, SMALL_INTEGER_EXT, 7]),
        ("tuple", wi(&[SMALL_TUPLE_EXT, 2, SMALL_INTEGER_EXT, 1, SMALL_INTEGER_EXT, 2])),
        ("nil", wi(&[NIL_EXT])),
        ("string", wi(&[STRING_EXT, 0, 3, 1, 2, 3])),
    ] {
        for len in 0..image.len() {
            cases.push(malformed(&format!("{name}_truncated_to_{len}"), image[..len].to_vec()));
        }
    }

    // Lying lengths.
    cases.push(malformed("string_len_10_has_2", wi(&[STRING_EXT, 0, 10, 1, 2])));
    cases.push(malformed("string_len_3_has_2", wi(&[STRING_EXT, 0, 3, 1, 2])));
    cases.push(malformed("list_len_5_has_1", wi(&[LIST_EXT, 0, 0, 0, 5, SMALL_INTEGER_EXT, 1])));
    cases.push(malformed("binary_len_10_has_3", wi(&[BINARY_EXT, 0, 0, 0, 10, 1, 2, 3])));
    cases.push(malformed("bit_binary_len_10_has_1", wi(&[BIT_BINARY_EXT, 0, 0, 0, 10, 3, 1])));

    // Oversized counts (wire bounds ⇒ `BadArg`, never `SystemLimit`).
    cases.push(malformed("large_tuple_arity_2_24", wi(&[LARGE_TUPLE_EXT, 1, 0, 0, 0])));
    cases.push(malformed("small_tuple_255_no_elements", wi(&[SMALL_TUPLE_EXT, 255])));
    cases.push(malformed("large_big_count_past_bound", wi(&[LARGE_BIG_EXT, 0, 7, 255, 249, 0])));
    cases.push(malformed("small_big_count_200_has_1", wi(&[SMALL_BIG_EXT, 200, 0, 1])));
    let mut ref6 = wi(&[NEWER_REFERENCE_EXT, 0, 6]);
    ref6.extend_from_slice(&NODE_N1);
    ref6.extend_from_slice(&[0, 0, 0, 1]);
    ref6.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 6]);
    cases.push(malformed("newer_reference_len_6", ref6));

    // Floats: a non-finite `NEW_FLOAT_EXT` and unparsable legacy `FLOAT_EXT` spellings.
    cases.push(malformed("new_float_infinity", wi(&[NEW_FLOAT_EXT, 0x7F, 0xF0, 0, 0, 0, 0, 0, 0])));
    cases.push(malformed("new_float_nan", wi(&[NEW_FLOAT_EXT, 0x7F, 0xF8, 0, 0, 0, 0, 0, 0])));
    cases.push(malformed("float_ext_nan", legacy_float_image(b"nan")));
    cases.push(malformed("float_ext_no_radix", legacy_float_image(b"1e5")));

    // Over-long atom names (256 characters > `MAX_ATOM_CHARS`).
    let mut long = wi(&[ATOM_UTF8_EXT, 1, 0]);
    long.extend(std::iter::repeat(b'a').take(256));
    cases.push(malformed("atom_name_256_chars_utf8", long));
    let mut long_latin1 = wi(&[ATOM_EXT, 1, 0]);
    long_latin1.extend(std::iter::repeat(b'a').take(256));
    cases.push(malformed("atom_name_256_chars_latin1", long_latin1));

    // Legacy identifiers: a non-tiny creation, and a legacy/local first id ≥ MAX_REFERENCE.
    cases.push(malformed("reference_ext_creation_4", legacy_reference_image(1, 4)));
    cases.push(malformed("port_ext_creation_4", legacy_port_image(1, 4)));
    cases.push(malformed("pid_ext_creation_4", legacy_pid_image(1, 2, 4)));
    cases.push(malformed("reference_ext_first_id_at_max", legacy_reference_image(1 << 18, 1)));
    cases.push(malformed("new_reference_ext_first_id_at_max", new_reference_image(&[1 << 18], 1)));
    // The `90` first-number bound fires only for the *local* node, so this row installs
    // the identity `NODE_N1`/creation `0` and takes the `decode_term_with_local_node`
    // path (a remote `r0 = 1 << 18` is legal and round-trips).
    let mut local_ref = wi(&[NEWER_REFERENCE_EXT, 0, 1]);
    local_ref.extend_from_slice(&NODE_N1);
    local_ref.extend_from_slice(&0u32.to_be_bytes()); // creation
    local_ref.extend_from_slice(&(1u32 << 18).to_be_bytes()); // r0 == MAX_REFERENCE
    cases.push(malformed_local("newer_reference_local_first_id_at_max", local_ref));

    // Maps: a bit-31 pair count, and a duplicate key.
    cases.push(malformed("map_pair_count_bit31", wi(&[MAP_EXT, 0x80, 0, 0, 1])));
    cases.push(malformed(
        "map_duplicate_key",
        wi(&[MAP_EXT, 0, 0, 0, 2, SMALL_INTEGER_EXT, 1, SMALL_INTEGER_EXT, 2, SMALL_INTEGER_EXT, 1, SMALL_INTEGER_EXT, 3]),
    ));

    // Native records: a bad `flags` byte, and a field count past the header bound.
    cases.push(malformed(
        "record_bad_flags_byte",
        wi(&[RECORD_EXT, 0, 0, 0, 1, 2, 119, 1, b'm', 119, 1, b'r', 119, 1, b'k', SMALL_INTEGER_EXT, 1]),
    ));
    cases.push(malformed(
        "record_field_count_2_24",
        wi(&[RECORD_EXT, 1, 0, 0, 0, 0, 119, 1, b'm', 119, 1, b'r']),
    ));

    // `NEW_FUN_EXT` field violations: `num_free` past its 8-bit header field, a
    // non-small `old_index`, and a non-pid creator.
    cases.push(malformed(
        "new_fun_num_free_256",
        new_fun_image(256, &[SMALL_INTEGER_EXT, 0], &[SMALL_INTEGER_EXT, 0], &CREATOR_PID),
    ));
    cases.push(malformed(
        "new_fun_old_index_not_small",
        new_fun_image(0, &[SMALL_TUPLE_EXT, 0], &[SMALL_INTEGER_EXT, 0], &CREATOR_PID),
    ));
    cases.push(malformed(
        "new_fun_creator_not_pid",
        new_fun_image(0, &[SMALL_INTEGER_EXT, 0], &[SMALL_INTEGER_EXT, 0], &[SMALL_INTEGER_EXT, 0]),
    ));

    // The six C **`dec_atom`** positions (`external.c:3010-3106`), all of which have no
    // `NIL_EXT` case: the `[local]` internal-node marker is legal only as an
    // identifier's node field (`dec_sysname`, `:3114-3122`), so a `NIL` in an *atom*
    // position must be `badarg` — never a term built from `atom_val(NIL)` (whatever atom
    // sits at table index 0, i.e. a wrong value). Behind `LOCAL_EXT` (`121`) is exactly
    // where the marker reaches them; with no local identity installed the 4-byte hash is
    // consumed-but-unchecked (`external.c:5250-5254`), so the plain `decode_term` path
    // exercises them. These rows are **not** oracle-checked: a synthetic `LOCAL_EXT` hash
    // is a per-VM-start secret of the oracle's, which rejects every one of these images
    // for the *hash* rather than for the rule under test.
    for (label, body) in [
        // `EXPORT_EXT`: `[NIL][f][1]` and `[m][NIL][1]`.
        ("local_export_module_nil", wi(&[EXPORT_EXT, NIL_EXT, 119, 1, b'f', SMALL_INTEGER_EXT, 1])),
        (
            "local_export_function_nil",
            wi(&[EXPORT_EXT, 119, 1, b'm', NIL_EXT, SMALL_INTEGER_EXT, 1]),
        ),
        // `NEW_FUN_EXT`: the module atom (`external.c:5130`).
        ("local_new_fun_module_nil", new_fun_body_with_module(NIL_EXT)),
        // `RECORD_EXT`: the module, the name and each key atom (`external.c:5301-5318`).
        (
            "local_record_module_nil",
            wi(&[RECORD_EXT, 0, 0, 0, 1, 0, NIL_EXT, 119, 1, b'r', 119, 1, b'k', SMALL_INTEGER_EXT, 1]),
        ),
        (
            "local_record_name_nil",
            wi(&[RECORD_EXT, 0, 0, 0, 1, 0, 119, 1, b'm', NIL_EXT, 119, 1, b'k', SMALL_INTEGER_EXT, 1]),
        ),
        (
            "local_record_key_nil",
            wi(&[RECORD_EXT, 0, 0, 0, 1, 0, 119, 1, b'm', 119, 1, b'r', NIL_EXT, SMALL_INTEGER_EXT, 1]),
        ),
    ] {
        let mut image = wi(&[LOCAL_EXT, 0xDE, 0xAD, 0xBE, 0xEF]);
        image.extend_from_slice(&body);
        cases.push(malformed_unoracleable(label, image));
    }
    // The one decode-side `SystemLimit`: a nesting depth past `MAX_DECODE_DEPTH`
    // (1500). `2000` is over the bound yet comfortably under the ~6250 stack-overflow
    // cliff, so this stays a typed error on the default test stack. The oracle handles
    // deep nesting fine, so it is not part of the `badarg` cross-check.
    cases.push(Malformed {
        label: "nesting_over_max_decode_depth".to_string(),
        image: nested_tuple_image(2000),
        expect: Expect::SystemLimit,
        local_node: false,
        oracle: false,
    });

    cases
}

/// Decode one table row: the identity-carrying entry point for a `local_node` row (the
/// image's node is `NODE_N1` / creation `0`), the plain one otherwise.
fn decode_row(c: &Malformed) -> Result<(), BeamError> {
    let mut heap = Heap::new(1 << 14).unwrap();
    let mut atoms = AtomTable::new();
    if c.local_node {
        let node = atoms.intern("n1");
        decode_term_with_local_node(&c.image, &mut heap, &mut atoms, node, 0, None).map(|_| ())
    } else {
        decode_term(&c.image, &mut heap, &mut atoms).map(|_| ())
    }
}

/// Every malformed row yields its **exact** typed error variant — never a panic, never
/// `Ok`, and never a different class.
#[test]
fn malformed_inputs_are_the_exact_typed_error() {
    let cases = malformed_cases();
    assert!(cases.len() >= 40, "the malformed table shrank to {} rows", cases.len());

    let mut bad_arg = 0usize;
    let mut system_limit = 0usize;
    for c in &cases {
        match (c.expect, decode_row(c)) {
            (Expect::BadArg, Err(BeamError::BadArg)) => bad_arg += 1,
            (Expect::SystemLimit, Err(BeamError::SystemLimit)) => system_limit += 1,
            (expect, other) => panic!(
                "{}: expected {expect:?} for {:?}, got {other:?}",
                c.label, c.image
            ),
        }
    }
    assert!(bad_arg > 0 && system_limit == 1, "{bad_arg} badarg / {system_limit} system_limit");
    eprintln!(
        "[beam_codec tag_coverage] table 2: {} malformed rows ({bad_arg} BadArg, {system_limit} SystemLimit), none panicked, none Ok",
        cases.len()
    );
}

// ===========================================================================
// LIVE oracle cross-checks
// ===========================================================================

/// `<<b0,b1,…>>` — the Erlang binary literal the oracle's `-eval` accepts.
fn erl_binary(bytes: &[u8]) -> String {
    let joined = bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("<<{joined}>>")
}

/// Classifies each image as the oracle's `binary_to_term/1` sees it (`ok` or `badarg`),
/// in one batched `erl` spawn.
fn oracle_classify(oracle: &Path, images: &[Vec<u8>]) -> Vec<String> {
    let list = images.iter().map(|b| erl_binary(b)).collect::<Vec<_>>().join(",");
    let eval = format!(
        "F = fun(B) -> try binary_to_term(B), ok catch _:_ -> badarg end end, \
         io:format(\"~w~n\", [[F(B) || B <- [{list}]]])"
    );
    let out = run_erl(oracle, &eval);
    let inner = out
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or_else(|| panic!("unexpected oracle output {out:?}"));
    if inner.trim().is_empty() {
        Vec::new()
    } else {
        inner.split(',').map(|s| s.trim().to_string()).collect()
    }
}

/// The representative corpus: `(label, Erlang expression, our encoder's image of the
/// identical term)`. Every entry is integer-keyed or carries its atoms as **names** on
/// the wire, so our bytes are VM-start independent and comparable to the oracle's
/// `term_to_binary/1` (see the Table-1 atom-table hazard note).
fn representative_corpus() -> Vec<(&'static str, &'static str, Vec<u8>)> {
    let mut atoms = AtomTable::new();
    let a = make_atom(atoms.intern("a"));
    let b = make_atom(atoms.intern("b"));
    let x = make_atom(atoms.intern("x"));
    let wide = make_atom(atoms.intern(&"\u{03bb}".repeat(130)));
    let mut h = new_heap(1 << 18);
    let enc = |t: Eterm, atoms: &AtomTable| encode_term(t, atoms).unwrap();

    let mut byte_list = NIL;
    for v in [3i64, 2, 1] {
        byte_list = h.cons(make_small(v), byte_list).unwrap();
    }
    let improper = h.cons(make_small(1), make_small(2)).unwrap();
    let tup = h.tuple(&[a, b]).unwrap();
    let wide_tuple = h.tuple(&vec![x; 300]).unwrap();
    let flat = make_map(&mut h, &[(make_small(1), make_small(2))], &atoms).unwrap();
    let pairs: Vec<(Eterm, Eterm)> = (1..=40i64).map(|n| (make_small(n), make_small(n * 2))).collect();
    let hamt = make_map(&mut h, &pairs, &atoms).unwrap();
    let mut digits = vec![0u64; 33];
    digits[32] = 1; // 1 << 2048
    let big = make_big(&mut h, false, &digits).unwrap();
    let bin = make_bitstring(&mut h, b"abc", 24).unwrap();
    let bits = make_bitstring(&mut h, &[0b0010_0000], 3).unwrap(); // <<1:3>>

    vec![
        ("small_integer_97", "5", enc(make_small(5), &atoms)),
        ("integer_98", "100000", enc(make_small(100000), &atoms)),
        ("small_big_110", "1 bsl 63", enc(make_big(&mut h, false, &[1u64 << 63]).unwrap(), &atoms)),
        ("large_big_111", "1 bsl 2048", enc(big, &atoms)),
        ("new_float_70", "1.5", enc(make_float(&mut h, 1.5).unwrap(), &atoms)),
        ("small_atom_utf8_119", "'a'", enc(a, &atoms)),
        ("atom_utf8_118", "list_to_atom(lists:duplicate(130,955))", enc(wide, &atoms)),
        ("nil_106", "[]", enc(NIL, &atoms)),
        ("string_107", "[1,2,3]", enc(byte_list, &atoms)),
        ("list_108_improper", "[1|2]", enc(improper, &atoms)),
        ("binary_109", "<<\"abc\">>", enc(bin, &atoms)),
        ("bit_binary_77", "<<1:3>>", enc(bits, &atoms)),
        ("small_tuple_104", "{a,b}", enc(tup, &atoms)),
        ("large_tuple_105", "list_to_tuple(lists:duplicate(300,x))", enc(wide_tuple, &atoms)),
        ("map_116_flat", "#{1=>2}", enc(flat, &atoms)),
        ("map_116_hamt", "maps:from_list([{N,N*2} || N <- lists:seq(1,40)])", enc(hamt, &atoms)),
    ]
}

/// A representative subset of tags whose terms we can build *identically* on both sides
/// must encode **byte-for-byte** like the oracle's own `term_to_binary/1`.
#[test]
fn representative_tags_match_the_oracle_byte_for_byte() {
    let Some(oracle) = oracle_or_skip("representative_tags_match_the_oracle_byte_for_byte") else {
        return;
    };

    let corpus = representative_corpus();
    let erl_list = corpus
        .iter()
        .map(|(_, erl, _)| format!("term_to_binary({erl})"))
        .collect::<Vec<_>>()
        .join(",");
    // NOTE: the outer `[ … ]` is `io:format`'s argument list, so the corpus must be
    // nested one level deeper — a single argument holding the list of images.
    let images =
        parse_binary_list(&run_erl(&oracle, &format!("io:format(\"~w~n\", [[ {erl_list} ]])")));
    assert_eq!(images.len(), corpus.len(), "one oracle image per corpus entry");

    for ((label, erl, ours), theirs) in corpus.iter().zip(&images) {
        assert_eq!(theirs[0], VERSION_MAGIC, "{label}: oracle version byte");
        assert_eq!(
            ours, theirs,
            "{label} ({erl}): our encoder's image must match the oracle byte-for-byte\n\
             ours:   {ours:?}\noracle: {theirs:?}"
        );
    }
    eprintln!(
        "[beam_codec tag_coverage] {} representative tags matched the oracle byte-for-byte",
        corpus.len()
    );
}

/// Tags whose terms we cannot build identically (a pid/port/ref is VM-specific, and an
/// `EXPORT_EXT` fun's module must exist): take the oracle's own image, decode it, and
/// assert our re-encode is byte-identical.
#[test]
fn oracle_identifier_and_fun_tags_round_trip_byte_exact() {
    let Some(oracle) = oracle_or_skip("oracle_identifier_and_fun_tags_round_trip_byte_exact")
    else {
        return;
    };

    let corpus = r#"[ term_to_binary(self())
         , term_to_binary(hd(erlang:ports()))
         , term_to_binary(make_ref())
         , term_to_binary(fun lists:reverse/1)
       ]"#;
    let images = parse_binary_list(&run_erl(&oracle, &format!("io:format(\"~w~n\", [ {corpus} ])")));
    assert_eq!(images.len(), 4, "four oracle images: {images:?}");

    for (image, expected_tag) in images.iter().zip([NEW_PID_EXT, V4_PORT_EXT, NEWER_REFERENCE_EXT, EXPORT_EXT]) {
        assert_eq!(image[0], VERSION_MAGIC);
        assert_eq!(image[1], expected_tag, "oracle top-level tag");
        let mut heap = new_heap(1 << 16);
        let mut atoms = AtomTable::new();
        let term = decode_term(image, &mut heap, &mut atoms)
            .unwrap_or_else(|e| panic!("the oracle's {image:?} must decode: {e:?}"));
        let out = encode_term(term, &atoms).expect("a decoded identifier/fun re-encodes");
        assert_eq!(&out, image, "our re-encode of the oracle's image must be byte-exact");
    }
    eprintln!("[beam_codec tag_coverage] oracle pid/port/ref/export tags re-encoded byte-exactly");
}

/// The oracle cannot *produce* `RECORD_EXT` (`67` — an Erlang record is a tagged tuple)
/// and our hand-built `NEW_FUN_EXT` has its own identity, so pin the weaker property the
/// oracle *can* confirm: it **accepts** both images via `binary_to_term/1`.
#[test]
fn oracle_accepts_our_record_and_new_fun_images() {
    let Some(oracle) = oracle_or_skip("oracle_accepts_our_record_and_new_fun_images") else {
        return;
    };

    let record = tag_cases()
        .into_iter()
        .find(|c| c.label == "record_67")
        .expect("the record row exists")
        .image;
    let new_fun = tag_cases()
        .into_iter()
        .find(|c| c.label == "new_fun_112")
        .expect("the new-fun row exists")
        .image;

    let eval = format!(
        "F = fun(B) -> try binary_to_term(B), true catch _:_ -> false end end, \
         io:format(\"~w~n\", [[F({record}), F({new_fun})]])",
        record = erl_binary(&record),
        new_fun = erl_binary(&new_fun),
    );
    let out = run_erl(&oracle, &eval);
    assert!(
        out.contains("[true,true]"),
        "the oracle must accept our RECORD_EXT and NEW_FUN_EXT images; got {out:?}"
    );
    eprintln!("[beam_codec tag_coverage] the oracle accepted our 67 and 112 images");
}

/// The oracle's `binary_to_term/1` rejects the same representative malformed inputs we do
/// (`badarg`) — so our typed `BadArg` is fidelity, not a new restriction.
#[test]
fn malformed_inputs_match_the_oracle() {
    let Some(oracle) = oracle_or_skip("malformed_inputs_match_the_oracle") else {
        return;
    };

    let cases: Vec<Malformed> = malformed_cases().into_iter().filter(|c| c.oracle).collect();
    let images: Vec<Vec<u8>> = cases.iter().map(|c| c.image.clone()).collect();
    let theirs = oracle_classify(&oracle, &images);
    assert_eq!(theirs.len(), cases.len(), "the oracle classified every malformed image");

    for (case, verdict) in cases.iter().zip(&theirs) {
        assert_eq!(
            verdict, "badarg",
            "{}: the oracle must reject {:?} as badarg, got {verdict}",
            case.label, case.image
        );
    }
    eprintln!(
        "[beam_codec tag_coverage] the oracle agreed: all {} malformed images are badarg",
        cases.len()
    );
}
