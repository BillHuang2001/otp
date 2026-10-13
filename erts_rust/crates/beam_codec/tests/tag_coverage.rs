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
use beam_term::prelude::AtomTable;

use beam_codec::{
    decode_term, AtomCache, Decoder, Tag, COMPRESSED_EXT, LOCAL_EXT, VERSION_MAGIC,
};

/// The sysname bytes of the smallest node atom (`SMALL_ATOM_UTF8_EXT`, `"n"`).
const NODE_N: [u8; 3] = [119, 1, b'n'];

/// Decode a `term_to_binary` image, returning the typed error on failure.
fn decode(image: &[u8]) -> Result<(), BeamError> {
    let mut heap = Heap::new(1 << 14).unwrap();
    let mut atoms = AtomTable::new();
    decode_term(image, &mut heap, &mut atoms).map(|_| ())
}

/// A **minimal valid image** for `tag`, or `None` for the tags whose minimal form
/// needs context the plain `decode_term` entry cannot supply (`COMPRESSED`,
/// `LOCAL_EXT`, `ATOM_CACHE_REF` — each handled in [`every_covered_tag_decodes`]).
///
/// Every image is `[VERSION_MAGIC][<one term of that tag>]`; the identifier node atom
/// is the 1-byte `SMALL_ATOM_UTF8_EXT` name `"n"` (`[119, 1, b'n']`).
fn minimal_image(tag: Tag) -> Option<Vec<u8>> {
    let mut image = vec![VERSION_MAGIC];
    match tag {
        Tag::SmallInteger => image.extend_from_slice(&[97, 0]),
        Tag::Integer => image.extend_from_slice(&[98, 0, 0, 0, 1]),
        // `"1.0e+00"` NUL-padded to the fixed 31-byte `FLOAT_EXT` field (a spelling
        // `sys_chars_to_double` accepts, `sys_float.c:72-140`).
        Tag::Float => {
            image.push(99);
            let text = b"1.0e+00";
            image.extend_from_slice(text);
            image.extend(std::iter::repeat(0).take(31 - text.len()));
        }
        Tag::Atom => image.extend_from_slice(&[100, 0, 1, b'a']),
        Tag::SmallAtom => image.extend_from_slice(&[115, 1, b'a']),
        Tag::AtomUtf8 => image.extend_from_slice(&[118, 0, 1, b'a']),
        Tag::SmallAtomUtf8 => image.extend_from_slice(&[119, 1, b'a']),
        // `REFERENCE_EXT`: `[node][id:4][creation:1]`.
        Tag::Reference => {
            image.push(101);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 42, 0]);
        }
        // `NEW_REFERENCE_EXT`: `[len:2][node][creation:1][ids:len×4]`.
        Tag::NewReference => {
            image.push(114);
            image.extend_from_slice(&[0, 1]);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0]);
            image.extend_from_slice(&[0, 0, 0, 1]);
        }
        // `NEWER_REFERENCE_EXT`: `[len:2][node][creation:4][ids:len×4]`.
        Tag::NewerReference => {
            image.push(90);
            image.extend_from_slice(&[0, 1]);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 0]);
            image.extend_from_slice(&[0, 0, 0, 1]);
        }
        // `PORT_EXT`: `[node][id:4][creation:1]`.
        Tag::Port => {
            image.push(102);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 7, 0]);
        }
        // `NEW_PORT_EXT`: `[node][id:4][creation:4]`.
        Tag::NewPort => {
            image.push(89);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 7, 0, 0, 0, 0]);
        }
        // `V4_PORT_EXT`: `[node][number:8][creation:4]`.
        Tag::V4Port => {
            image.push(120);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0]);
        }
        // `PID_EXT`: `[node][number:4][serial:4][creation:1]`.
        Tag::Pid => {
            image.push(103);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 10, 0, 0, 0, 20, 0]);
        }
        // `NEW_PID_EXT`: `[node][number:4][serial:4][creation:4]`.
        Tag::NewPid => {
            image.push(88);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 10, 0, 0, 0, 20]);
            image.extend_from_slice(&[0, 0, 0, 0]);
        }
        Tag::SmallTuple => image.extend_from_slice(&[104, 0]),
        Tag::LargeTuple => image.extend_from_slice(&[105, 0, 0, 0, 0]),
        Tag::Nil => image.push(106),
        Tag::String => image.extend_from_slice(&[107, 0, 0]),
        // `LIST_EXT`: `[length:4][elements][tail]` — one cons (`1`) whose tail is `NIL`.
        Tag::List => image.extend_from_slice(&[108, 0, 0, 0, 1, 97, 1, 106]),
        // `SMALL_BIG_EXT`: sign `0`, one magnitude byte `5`.
        Tag::SmallBig => image.extend_from_slice(&[110, 1, 0, 5]),
        // `LARGE_BIG_EXT`: 32-bit count `1`, sign `0`, one magnitude byte `5`.
        Tag::LargeBig => image.extend_from_slice(&[111, 0, 0, 0, 1, 0, 5]),
        Tag::NewFloat => {
            image.push(70);
            image.extend_from_slice(&1.5f64.to_be_bytes());
        }
        Tag::Binary => image.extend_from_slice(&[109, 0, 0, 0, 0]),
        // `BIT_BINARY_EXT`: 1 byte, 1 significant bit (0b1 in the top bit).
        Tag::BitBinary => image.extend_from_slice(&[77, 0, 0, 0, 1, 1, 0x80]),
        Tag::Map => image.extend_from_slice(&[116, 0, 0, 0, 0]),
        // `EXPORT_EXT`: `[module atom][function atom][arity term]`.
        Tag::Export => {
            image.push(113);
            image.extend_from_slice(&[119, 1, b'm']);
            image.extend_from_slice(&[119, 1, b'f']);
            image.extend_from_slice(&[97, 1]);
        }
        // `NEW_FUN_EXT` with `num_free = 0` — no environment to follow.
        Tag::NewFun => {
            image.push(112);
            image.extend_from_slice(&[0, 0, 0, 0]); // `size` (read and ignored since OTP 23)
            image.push(2); // arity (8-bit header field)
            image.extend_from_slice(&[0u8; 16]); // uniq
            image.extend_from_slice(&[0, 0, 0, 0]); // index
            image.extend_from_slice(&[0, 0, 0, 0]); // num_free
            image.extend_from_slice(&[119, 1, b'm']); // module atom
            image.extend_from_slice(&[97, 0]); // old_index (small)
            image.extend_from_slice(&[97, 0]); // old_uniq (small)
            image.extend_from_slice(&[88]); // creator pid: NEW_PID_EXT
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 10, 0, 0, 0, 20]);
            image.extend_from_slice(&[0, 0, 0, 0]);
        }
        // `RECORD_EXT` with zero fields: only the 2-word definition follows.
        Tag::Record => {
            image.push(67);
            image.extend_from_slice(&[0, 0, 0, 0]); // num_fields
            image.push(0); // flags: not exported
            image.extend_from_slice(&[119, 1, b'm']); // module atom
            image.extend_from_slice(&[119, 1, b'r']); // name atom
        }
        // The wrapper/contextual tags have no plain minimal image.
        Tag::Compressed | Tag::Local | Tag::AtomCacheRef => return None,
        // Uncovered tags: never a term, so no valid image exists.
        Tag::Fun | Tag::NewCache | Tag::DistHeader => return None,
    }
    assert_eq!(
        Tag::from_byte(image[1]),
        Some(tag),
        "the minimal image for {tag:?} does not start with its own tag"
    );
    Some(image)
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
            minimal_image(tag).is_some() || matches!(tag, Tag::Compressed | Tag::Local),
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
