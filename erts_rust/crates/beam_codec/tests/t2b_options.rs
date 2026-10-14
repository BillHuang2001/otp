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

//! The `term_to_binary/2` **`deterministic`** option (`DFLAG_DETERMINISTIC`).
//!
//! With `[deterministic]`, the C encoder collects a map's `(key, value)` pairs into an
//! array, sorts them by the key in **full Erlang term order** (`map_key_compare` =
//! `CMP_TERM` = `erts_cmp(a, b, exact=1, eq_only=0)`, `external.c:3217-3227`) and emits
//! `K1,V1,…,Kn,Vn` in that order — for a ≤ 32-pair **flat** map (`external.c:3757-3764`)
//! and for a > 32-pair **hash** map / HAMT (`external.c:3783-3807`) alike. The
//! `beam_codec` half of that is [`beam_codec::Encoder::with_deterministic`] /
//! [`beam_codec::Encoder::set_deterministic`].
//!
//! This file is the live differential pin. A **single** `erl` invocation (VM boot is
//! ~120 ms, so every case is batched) yields the oracle images; our encoder's
//! `deterministic` output for the equivalent terms must match **byte-for-byte**, and the
//! `deterministic` image must differ from the default one.
//!
//! Integer keys are preferred for the cross-VM comparisons: a flat map's **atom** keys
//! sort by atom-table index, which depends on interning order, so the *default* image of
//! an atom-keyed map is not stable across VM starts. Under `deterministic` the atom keys
//! are ordered by **name** (full term order), which *is* stable — that is exactly the
//! property this option buys, so the deterministic atom-keyed image is comparable too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;

use beam_codec::term::{make_flatmap, make_map};
use beam_codec::{Encoder, MAP_EXT, VERSION_MAGIC};

use beam_term::prelude::{make_atom, make_small, AtomTable, Eterm};
// ---------------------------------------------------------------------------
// Oracle resolution and process plumbing (mirrors tests/map_key_order.rs)
// ---------------------------------------------------------------------------

/// Whether `p` is an existing executable file.
fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Resolve the oracle the same way `tools/differential` does: `$ERTS_ORACLE` if it is an
/// existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec t2b_options] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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

/// Returns the oracle, or prints a loud skip note and returns `None` (never a silent pass).
fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("=============== beam_codec t2b_options: {test} SKIPPED ===============");
            eprintln!("No usable system `erl` oracle found — see the note above, if any");
            eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
            eprintln!(
                "This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP) and re-run."
            );
            eprintln!("=======================================================================");
            eprintln!();
            None
        }
    }
}

/// Run one `erl -noshell -eval <eval> -s init stop` and return its stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// Parse the `[<<131,...>>,<<...>>]` list the oracle prints into byte vectors.
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
                        t.trim().parse::<u8>().unwrap_or_else(|_| {
                            panic!("bad byte {t:?} in the oracle output {out:?}")
                        })
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

fn heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for a test heap")
}

/// Encodes `term` through a `deterministic` `Encoder` (with the version byte) — the
/// crate's `term_to_binary/2` image under `[deterministic]`.
fn encode_deterministic(term: Eterm, atoms: &AtomTable) -> Vec<u8> {
    let mut encoder = Encoder::new(atoms).with_deterministic();
    encoder.encode(term).expect("a well-formed term encodes");
    encoder.finish()
}

/// Encodes `term` through the default (non-deterministic) `Encoder`.
fn encode_default(term: Eterm, atoms: &AtomTable) -> Vec<u8> {
    let mut encoder = Encoder::new(atoms);
    encoder.encode(term).expect("a well-formed term encodes");
    encoder.finish()
}

// ---------------------------------------------------------------------------
// The live differential pin
// ---------------------------------------------------------------------------

/// One `erl` spawn yields the oracle's `[deterministic]` images for an atom-keyed flat
/// map, an integer-keyed flat map and a 40-pair integer-keyed HAMT, plus the HAMT's
/// default image; our encoder must reproduce all four **byte-for-byte**.
#[test]
fn deterministic_images_match_the_oracle_byte_exactly() {
    let Some(oracle) = oracle_or_skip("deterministic_images_match_the_oracle_byte_exactly") else {
        return;
    };

    // Batched: one VM boot for every case. Atoms are interned eagerly by the `-eval`
    // scanner, but that only affects the *default* atom-keyed image (not requested).
    let images = parse_binary_list(&run_erl(
        &oracle,
        "io:format(\"~w~n\", [ [ \
            term_to_binary(#{b=>1,a=>2,c=>3}, [deterministic]) \
          , term_to_binary(#{10=>1,2=>2,3=>3}, [deterministic]) \
          , term_to_binary(maps:from_list([{N,N} || N <- lists:seq(1,40)]), [deterministic]) \
          , term_to_binary(maps:from_list([{N,N} || N <- lists:seq(1,40)])) \
          ] ])",
    ));
    assert_eq!(images.len(), 4, "one oracle image per requested case");

    // (a) Atom-keyed flat map: `deterministic` orders the keys by name → a, b, c.
    // Pinned literally first (the objective's recorded oracle evidence) ...
    assert_eq!(
        images[0],
        vec![131, 116, 0, 0, 0, 3, 119, 1, 97, 97, 2, 119, 1, 98, 97, 1, 119, 1, 99, 97, 3],
        "the oracle's deterministic atom-keyed image is the a,b,c order"
    );
    // ... then against our encoder. The map is built in the stored order b, a, c.
    {
        let mut atoms = AtomTable::new();
        let mut h = heap(64);
        let a = make_atom(atoms.intern("a"));
        let b = make_atom(atoms.intern("b"));
        let c = make_atom(atoms.intern("c"));
        let map = make_flatmap(
            &mut h,
            &[(b, make_small(1)), (a, make_small(2)), (c, make_small(3))],
        )
        .unwrap();
        let ours_det = encode_deterministic(map, &atoms);
        assert_eq!(ours_det, images[0], "(a) deterministic atom-keyed flat map");
        // The option actually did something here: the stored order b,a,c differs from
        // the name order a,b,c.
        assert_ne!(
            ours_det,
            encode_default(map, &atoms),
            "(a) the deterministic image must differ from the default image"
        );
    }

    // (b) Integer-keyed flat map: stored order 10, 2, 3, deterministic → 2, 3, 10.
    {
        let atoms = AtomTable::new();
        let mut h = heap(64);
        let map = make_flatmap(
            &mut h,
            &[
                (make_small(10), make_small(1)),
                (make_small(2), make_small(2)),
                (make_small(3), make_small(3)),
            ],
        )
        .unwrap();
        let ours_det = encode_deterministic(map, &atoms);
        assert_eq!(
            ours_det, images[1],
            "(b) deterministic integer-keyed flat map"
        );
        assert_ne!(
            ours_det,
            encode_default(map, &atoms),
            "(b) the deterministic image must differ from the default image"
        );
    }

    // (c) A 40-pair (> MAP_SMALL_MAP_LIMIT) integer-keyed HAMT: deterministic orders the
    //     keys ascending 1..40; the default image is the HAMT's node-slot traversal.
    {
        let atoms = AtomTable::new();
        let mut h = heap(1 << 16);
        let pairs: Vec<(Eterm, Eterm)> = (1..=40i64)
            .map(|n| (make_small(n), make_small(n)))
            .collect();
        // `make_map` builds a hash map above `MAP_SMALL_MAP_LIMIT`.
        let map = make_map(&mut h, &pairs, &atoms).unwrap();
        let ours_det = encode_deterministic(map, &atoms);
        assert_eq!(ours_det, images[2], "(c) deterministic 40-pair HAMT");
        assert_eq!(
            encode_default(map, &atoms),
            images[3],
            "(c) the default 40-pair HAMT image is byte-identical to the oracle's"
        );
        assert_ne!(
            ours_det, images[3],
            "(c) the deterministic HAMT image must differ from the default one"
        );
    }

    // Every deterministic image carries the version byte and the `MAP_EXT` tag.
    for (index, image) in images.iter().enumerate() {
        assert_eq!(image[0], VERSION_MAGIC, "image {index}: version byte");
        assert_eq!(image[1], MAP_EXT, "image {index}: MAP_EXT tag");
    }
}

/// The oracle decodes every one of our `deterministic` images (`binary_to_term/1`) to a
/// map of the expected size — proof the sorted order is valid wire, not merely
/// byte-equal by accident.
#[test]
fn the_oracle_accepts_our_deterministic_images() {
    let Some(oracle) = oracle_or_skip("the_oracle_accepts_our_deterministic_images") else {
        return;
    };

    // Build our deterministic images for the three cases.
    let mut atoms = AtomTable::new();
    let mut h = heap(1 << 16);

    let a = make_atom(atoms.intern("a"));
    let b = make_atom(atoms.intern("b"));
    let c = make_atom(atoms.intern("c"));
    let atom_map = make_flatmap(
        &mut h,
        &[(b, make_small(1)), (a, make_small(2)), (c, make_small(3))],
    )
    .unwrap();
    let int_map = make_flatmap(
        &mut h,
        &[
            (make_small(10), make_small(1)),
            (make_small(2), make_small(2)),
            (make_small(3), make_small(3)),
        ],
    )
    .unwrap();
    let pairs: Vec<(Eterm, Eterm)> = (1..=40i64)
        .map(|n| (make_small(n), make_small(n)))
        .collect();
    let hamt = make_map(&mut h, &pairs, &atoms).unwrap();

    let images = [
        encode_deterministic(atom_map, &atoms),
        encode_deterministic(int_map, &atoms),
        encode_deterministic(hamt, &atoms),
    ];

    // Feed all three images to one oracle run as a list of binary literals; the oracle
    // decodes each and prints the map's size — proof the image is valid wire carrying the
    // expected number of pairs.
    let literal_list = images
        .iter()
        .map(|img| {
            let body = img
                .iter()
                .map(|byte| byte.to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!("<<{body}>>")
        })
        .collect::<Vec<_>>()
        .join(",");
    let out = run_erl(
        &oracle,
        &format!(
            "io:format(\"~w~n\", [ [ maps:size(binary_to_term(B)) || B <- [{literal_list}] ] ])"
        ),
    );
    assert_eq!(
        out.trim(),
        "[3,3,40]",
        "the oracle must decode our deterministic images to 3-, 3- and 40-pair maps"
    );
}
