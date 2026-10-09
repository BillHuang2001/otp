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

//! The `MAP_EXT` (`116`) **key-order contract**: the codec *preserves* a flat map's
//! stored pair order and never sorts or canonicalises it.
//!
//! ## Who owns the order
//!
//! ERTS keeps a *small* map (≤ 32 pairs, [`beam_term::map::MAP_SMALL_MAP_LIMIT`]) as a
//! **flat map** whose key tuple is sorted in the internal map-key order
//! `erts_cmp_flatmap_keys` (`../../erts/emulator/beam/erl_utils.h:265-272`): two **atom**
//! keys order by atom-table index (`key_a - key_b`), every other pair by full term order
//! (`erts_cmp`). A larger map is a **hash map** (HAMT), traversed in node-slot order
//! (`external.c:3746-3823`).
//!
//! The encoder emits a flat map's pairs **in that stored order** (`external.c:3746-3769`
//! — `flatmap_get_keys`/`flatmap_get_values`, `ENC_MAP_PAIR`, no sort unless the
//! `deterministic` dflag is set). So *sorting is the constructor's job*, not the codec's:
//! [`beam_term::map::write_map`] is what applies `cmp_flatmap_keys`, and every map built
//! through the compiled path already carries ERTS's order. `beam_codec` therefore only
//! *preserves* whatever order its input term stores — which is exactly what makes
//! decode → re-encode **byte-stable** (see [`flatmap_pairs_encode_in_stored_order`] and
//! [`oracle_and_our_encoder_agree_for_integer_keyed_maps`]).
//!
//! **Sorting inside `beam_codec` would be wrong.** It breaks the byte-exact fixture
//! `tests/ground_truth.rs::literals_gen_server_decodes_with_its_export_funs` — a real
//! `gen_server` literal pool whose map wire order is *not* this crate's atom-table index
//! order — and the live `interop` harness. (Confirmed by a throwaway experiment that
//! sorted in `encode_map`: those two tests fail; the change was reverted. See the crate
//! `CONTEXT.md`, "Map key encode order".)
//!
//! ## Why atom-keyed multi-key maps cannot be byte-compared across implementations
//!
//! A flat map's *atom* keys sort by **atom-table index**, and that index depends on the
//! order the atoms were first interned **during this VM start** — which is not stable
//! across runs. The same map can therefore encode to different `MAP_EXT` bytes in two
//! fresh `erl` processes. [`atom_keyed_map_bytes_depend_on_interning_order`] demonstrates
//! this against the live oracle: interning `zzzq_one` before `zzzq_two` yields a
//! different pair order than the reverse. Byte-exact cross-implementation comparison is
//! consequently only meaningful for **non-atom** keys (or for an already-ordered
//! flatmap), which is why the oracle cross-check below uses an **integer-keyed** corpus.
//!
//! ## Hash maps (> 32 pairs) are encoded too, byte-identically to ERTS
//!
//! A map of more than 32 pairs is a **hash map** (HAMT), which
//! [`beam_codec::term::flatmap_parts`] rejects. The encoder probes
//! [`beam_codec::term::map_pairs_storage_order`] **after** the flat-map probe and emits
//! the returned pairs through the hash-map emitter. That reader returns the pairs in
//! **ascending node-slot pre-order** (slots `0..sz`, descending depth-first at each), but
//! the C encoder walks the same tree through a **LIFO** work stack
//! (`external.c:3745-3823`: pushes slots ascending, pops descending), i.e. in the exact
//! **reverse** order — so the hash-map path reverses the pair list before emitting. See
//! [`hashmap_terms_encode_and_roundtrip`].
//!
//! `beam_term` drives the HAMT trie with the **bit-exact** `erts_map_hash`
//! (`../beam_term/src/map.rs`), so a map built by [`beam_term::map::write_map`] has the
//! same node structure — and therefore, with the reversal above, the same `MAP_EXT` bytes
//! — as the oracle's own image of the same key set. That byte-identity is pinned live by
//! [`hashmap_images_match_the_oracle_byte_exactly`] for maps of 33/40/50/64/100/1000 keys.
//! [`oracle_accepts_our_hashmap_image`] additionally proves the image is valid wire.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;

use beam_codec::term::{flatmap_parts, make_flatmap, map_pairs_storage_order};
use beam_codec::{decode_term, encode_term, MAP_EXT, VERSION_MAGIC};

use beam_term::map::map_pairs;
use beam_term::prelude::{atom_val, cmp_flatmap_keys, make_small, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// Oracle resolution and process plumbing (mirrors tests/interop.rs)
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
            "[beam_codec map_key_order] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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
            eprintln!("=============== beam_codec map_key_order: {test} SKIPPED ===============");
            eprintln!("No usable system `erl` oracle found — see the note above, if any");
            eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("=========================================================================");
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

/// Parse the `[<<131,116,...>>,<<...>>]` list the oracle prints into byte vectors.
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
                            .unwrap_or_else(|_| panic!("bad byte {t:?} in the oracle output {out:?}"))
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

/// A fresh heap for building/decoding a term.
fn heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for this size")
}

// ---------------------------------------------------------------------------
// 1. The codec preserves the stored order (it does not sort).
// ---------------------------------------------------------------------------

/// `#{10=>1, 2=>2, 3=>3}` as ERTS encodes it: `erts_cmp_flatmap_keys` (term) order gives
/// `2, 3, 10`. Regenerate with
/// `erl -noshell -eval 'io:format("~w~n",[term_to_binary(#{10=>1,2=>2,3=>3})])' -s init stop`.
const GOLDEN_10_1_2_2_3_3: &[u8] = &[
    VERSION_MAGIC, MAP_EXT, 0, 0, 0, 3, 97, 2, 97, 2, 97, 3, 97, 3, 97, 10, 97, 1,
];

#[test]
fn flatmap_pairs_encode_in_stored_order() {
    // Integer keys deliberately **not** in numeric order. The encoder must emit the
    // stored order verbatim: it preserves, it does not sort or canonicalise.
    let stored = [(10_i64, 1_i64), (2, 2), (7, 3), (3, 4)];
    let mut h = heap(64);
    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = stored
        .iter()
        .map(|(k, v)| (make_small(*k), make_small(*v)))
        .collect();
    let map = make_flatmap(&mut h, &pairs).unwrap();

    // The reader reports the same stored order back (wire order == storage order).
    let (keys, values) = flatmap_parts(map).unwrap();
    assert_eq!(
        keys,
        &[make_small(10), make_small(2), make_small(7), make_small(3)],
        "a flat map stores the caller's pair order as-is"
    );
    assert_eq!(
        values,
        &[make_small(1), make_small(2), make_small(3), make_small(4)]
    );

    // ... and the wire carries that same order, interleaved `K1,V1,...,Kn,Vn`.
    let bytes = encode_term(map, &atoms).unwrap();
    let expected = [
        VERSION_MAGIC, MAP_EXT, 0, 0, 0, 4, 97, 10, 97, 1, 97, 2, 97, 2, 97, 7, 97, 3, 97, 3,
        97, 4,
    ];
    assert_eq!(
        bytes, expected,
        "MAP_EXT must preserve the stored pair order byte-for-byte"
    );

    // Sanity: the emitted order is NOT the sorted (2,3,7,10) order ERTS's flat-map
    // invariant would store — proving the codec did not re-order anything.
    assert_ne!(
        &bytes[6..],
        &[97, 2, 97, 2, 97, 3, 97, 4, 97, 7, 97, 3, 97, 10, 97, 1][..]
    );
}

#[test]
fn integer_keyed_map_matches_the_oracle_golden() {
    // A map built the ERTS way — pairs sorted by `erts_cmp_flatmap_keys` before storage
    // (`beam_term::map::write_map` is what does this) — encodes to the oracle's exact
    // bytes. Here the keys are integers, so the order is plain term order.
    let mut h = heap(64);
    let atoms = AtomTable::new();
    let mut pairs: Vec<(Eterm, Eterm)> = vec![
        (make_small(10), make_small(1)),
        (make_small(2), make_small(2)),
        (make_small(3), make_small(3)),
    ];
    pairs.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, &atoms));
    let map = make_flatmap(&mut h, &pairs).unwrap();

    let bytes = encode_term(map, &atoms).unwrap();
    assert_eq!(bytes, GOLDEN_10_1_2_2_3_3);

    // ... and the golden decodes back to a flat map that re-encodes byte-exactly.
    let mut h2 = heap(64);
    let mut atoms2 = AtomTable::new();
    let decoded = decode_term(GOLDEN_10_1_2_2_3_3, &mut h2, &mut atoms2).unwrap();
    assert_eq!(
        encode_term(decoded, &atoms2).unwrap(),
        GOLDEN_10_1_2_2_3_3
    );
    let (keys, _values) = flatmap_parts(decoded).unwrap();
    assert_eq!(keys, &[make_small(2), make_small(3), make_small(10)]);
}

// ---------------------------------------------------------------------------
// 2. Live oracle cross-check over a deterministic (integer-keyed) corpus.
// ---------------------------------------------------------------------------

/// Integer-keyed maps only: their key order is term order, which is stable across VM
/// starts, so the oracle's `MAP_EXT` bytes are comparable.
const INT_KEYED_CORPUS: &str = "[ term_to_binary(#{}) \
    , term_to_binary(#{42=>7}) \
    , term_to_binary(#{10=>1,2=>2,3=>3}) \
    , term_to_binary(#{5=>1,100=>2,7=>3,2=>4}) \
    , term_to_binary(#{-3=>1,0=>2,1000000000000=>3}) \
    , term_to_binary(maps:from_list([{N, N*2} || N <- lists:seq(1,40)])) ]";

const INT_KEYED_CORPUS_LEN: usize = 6;

#[test]
fn oracle_and_our_encoder_agree_for_integer_keyed_maps() {
    let Some(oracle) = oracle_or_skip("oracle_and_our_encoder_agree_for_integer_keyed_maps")
    else {
        return;
    };

    // The corpus is wrapped in one more `[ ]` so it is a *single* `~w` argument (a list
    // of images), not five arguments to a one-directive format string.
    let images = parse_binary_list(&run_erl(
        &oracle,
        &format!("io:format(\"~w~n\", [{INT_KEYED_CORPUS}])"),
    ));
    assert_eq!(images.len(), INT_KEYED_CORPUS_LEN);

    for (index, image) in images.iter().enumerate() {
        assert_eq!(image[0], VERSION_MAGIC, "image {index}: version byte");
        assert_eq!(image[1], MAP_EXT, "image {index}: MAP_EXT tag");

        // Decode the oracle bytes and re-encode with our encoder. For an integer-keyed
        // map this MUST be byte-identical: our decoder preserves the wire (== ERTS
        // storage) order and our encoder preserves the stored order.
        let mut h = heap(1 << 12);
        let mut atoms = AtomTable::new();
        let term = decode_term(image, &mut h, &mut atoms)
            .unwrap_or_else(|e| panic!("image {index} ({image:?}) must decode: {e:?}"));
        let reencoded = encode_term(term, &atoms).expect("a decoded term always re-encodes");
        assert_eq!(
            &reencoded, image,
            "image {index}: our encode of the decoded oracle map must be byte-exact"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Why atom-keyed multi-key maps are not byte-comparable across implementations.
// ---------------------------------------------------------------------------

#[test]
fn atom_keyed_map_bytes_depend_on_interning_order() {
    let Some(oracle) = oracle_or_skip("atom_keyed_map_bytes_depend_on_interning_order") else {
        return;
    };

    // The SAME map in both runs; only the order the two atoms are first interned differs.
    // A flat map sorts atom keys by atom-table index, so the two fresh VMs store (and
    // thus emit) the pair order differently — which is why atom-keyed `MAP_EXT` bytes are
    // not a cross-implementation invariant.
    let one_first = run_erl(
        &oracle,
        r#"A = list_to_atom("zzzq_one"), B = list_to_atom("zzzq_two"), io:format("~w~n", [term_to_binary(#{A => 1, B => 2})])"#,
    );
    let two_first = run_erl(
        &oracle,
        r#"B = list_to_atom("zzzq_two"), A = list_to_atom("zzzq_one"), io:format("~w~n", [term_to_binary(#{A => 1, B => 2})])"#,
    );

    let images_one = parse_binary_list(&one_first);
    let images_two = parse_binary_list(&two_first);
    assert_eq!(images_one.len(), 1, "run 1 printed exactly one image");
    assert_eq!(images_two.len(), 1, "run 2 printed exactly one image");
    let (img_one, img_two) = (&images_one[0], &images_two[0]);

    // Both are two-pair `MAP_EXT` images of the same map...
    for img in [img_one, img_two] {
        assert_eq!(img[0], VERSION_MAGIC);
        assert_eq!(img[1], MAP_EXT);
        assert_eq!(u32::from_be_bytes([img[2], img[3], img[4], img[5]]), 2);
    }
    // ... but the byte streams differ, purely because of atom interning order.
    assert_ne!(
        img_one, img_two,
        "atom-keyed flat-map bytes must depend on the per-VM-start atom index order"
    );

    // The first-interned atom is the one that sorts first in each run: the ordering is
    // driven entirely by atom index, not by key content.
    let mut h1 = heap(64);
    let mut a1 = AtomTable::new();
    let t1 = decode_term(img_one, &mut h1, &mut a1).unwrap();
    let (keys1, _) = flatmap_parts(t1).unwrap();
    assert_eq!(a1.name(atom_val(keys1[0])).unwrap(), "zzzq_one");

    let mut h2 = heap(64);
    let mut a2 = AtomTable::new();
    let t2 = decode_term(img_two, &mut h2, &mut a2).unwrap();
    let (keys2, _) = flatmap_parts(t2).unwrap();
    assert_eq!(a2.name(atom_val(keys2[0])).unwrap(), "zzzq_two");
}

// ---------------------------------------------------------------------------
// 4. Hash maps (> 32 pairs) encode in HAMT storage order and round-trip.
// ---------------------------------------------------------------------------

#[test]
fn hashmap_terms_encode_and_roundtrip() {
    use beam_term::map::{map_words_capacity, write_map};

    // A 40-pair map built through the real HAMT builder is a *hash map*, not a flat map.
    let mut h = heap(4096);
    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = (1..=40_i64)
        .map(|n| (make_small(n), make_small(n * 2)))
        .collect();
    // SAFETY: `map_words_capacity` is the documented upper bound for `write_map`, and the
    // heap holds at least that many free words.
    let hp = unsafe { h.alloc_words(map_words_capacity(pairs.len())) }.unwrap();
    // SAFETY: `hp` points at `map_words_capacity(40) >= map_words_capacity(n)` exclusively
    // owned, writable words, the preconditions `write_map` documents.
    let map = unsafe { write_map(hp, &pairs, &atoms) }.unwrap();

    // A hash map is not a flat map, so the flat path cannot read it as pairs...
    assert!(
        flatmap_parts(map).is_err(),
        "a > 32-pair map is a HAMT, not a flat map"
    );
    // ... but the storage-order reader sees every pair, in HAMT traversal order.
    let stored = map_pairs_storage_order(map).expect("a HAMT has a storage-order pair list");
    assert_eq!(stored.len(), 40, "the HAMT holds every pair");

    // (a) A hash map is now encodable (it used to be a typed `unsupported`).
    let bytes = encode_term(map, &atoms).expect("a hash map is now encodable");
    assert_eq!(bytes[0], VERSION_MAGIC);
    assert_eq!(bytes[1], MAP_EXT);
    assert_eq!(u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]), 40);

    // (b) Content equality: decode and compare in a representation-independent order
    //     (`map_pairs` sorts by term order, so flat vs. hash representation is irrelevant).
    let mut h2 = heap(4096);
    let mut atoms2 = AtomTable::new();
    let decoded =
        decode_term(&bytes, &mut h2, &mut atoms2).expect("our own hash-map image must decode");
    assert_eq!(
        map_pairs(map, &atoms).unwrap(),
        map_pairs(decoded, &atoms2).unwrap(),
        "the decoded map must hold exactly the original's pairs (as a set)"
    );

    // (c) The wire carries the HAMT's *stored* order **reversed** — the C walks the tree
    //     through a LIFO work stack, so it emits the reverse of the ascending node-slot
    //     pre-order `map_pairs_storage_order` returns. The decoded map is itself a HAMT
    //     (40 > `MAP_SMALL_MAP_LIMIT`), rebuilt by the same `write_map`, so its storage
    //     order equals the original's while the image carries the reverse.
    let wire_stored = map_pairs_storage_order(decoded).unwrap();
    assert_eq!(
        wire_stored, stored,
        "the decoded HAMT stores the same pairs in the same order as the original"
    );
    // Read the pair order straight off the image (every key/value is a small here) and
    // check it is the reverse of that storage order.
    let mut wire_keys: Vec<Eterm> = Vec::new();
    let mut i = 6; // past `[version, MAP_EXT, count:4]`
    for _ in 0..40 {
        assert_eq!(bytes[i], 97, "SMALL_INTEGER_EXT key");
        wire_keys.push(make_small(i64::from(bytes[i + 1])));
        assert_eq!(bytes[i + 2], 97, "SMALL_INTEGER_EXT value");
        i += 4;
    }
    let mut reversed_stored: Vec<Eterm> = stored.iter().map(|(k, _)| *k).collect();
    reversed_stored.reverse();
    assert_eq!(
        wire_keys, reversed_stored,
        "MAP_EXT must carry the reverse of the HAMT storage order (the C's LIFO walk)"
    );
    // ... and that order is NOT the sorted key order (1,2,…,40), proving no sort happened.
    let sorted_keys: Vec<Eterm> = (1..=40).map(make_small).collect();
    assert_ne!(
        wire_keys, sorted_keys,
        "the encoder must not sort HAMT pairs into key order"
    );

    // (d) Byte-stability: re-encoding the decoded image reproduces our bytes exactly.
    assert_eq!(encode_term(decoded, &atoms2).unwrap(), bytes);
}

/// Our `write_map`-built HAMT must encode **byte-identically** to the oracle's own
/// `term_to_binary/1` image of the same integer-keyed map, for sizes 33/40/50/64/100/1000.
///
/// `beam_term` drives the HAMT with the bit-exact `erts_map_hash`, so our node structure
/// equals ERTS's; combined with the hash-map path's reversal of the ascending node-slot
/// pre-order (the C's LIFO work-stack walk — see `encode::Encoder::encode_hashmap_pairs`),
/// the wire bytes match exactly. Integer keys keep the comparison stable across VM starts
/// (unlike atom keys — see the module doc).
#[test]
fn hashmap_images_match_the_oracle_byte_exactly() {
    use beam_term::map::{map_words_capacity, write_map};

    let Some(oracle) = oracle_or_skip("hashmap_images_match_the_oracle_byte_exactly") else {
        return;
    };

    const SIZES: [i64; 6] = [33, 40, 50, 64, 100, 1000];

    // One oracle spawn for every size: a list of `term_to_binary/1` images, in `SIZES`
    // order, of `maps:from_list([{N, N*2} || N <- lists:seq(1, Sz)])`.
    let sizes_list = SIZES
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let images = parse_binary_list(&run_erl(
        &oracle,
        &format!(
            "io:format(\"~w~n\", [ [ term_to_binary(maps:from_list([{{N, N*2}} || N <- lists:seq(1, Sz)])) || Sz <- [{sizes_list}] ] ])"
        ),
    ));
    assert_eq!(images.len(), SIZES.len(), "one oracle image per size");

    for (image, size) in images.iter().zip(SIZES) {
        assert_eq!(image[0], VERSION_MAGIC, "{size}: version byte");
        assert_eq!(image[1], MAP_EXT, "{size}: MAP_EXT tag");
        assert_eq!(
            u32::from_be_bytes([image[2], image[3], image[4], image[5]]),
            size as u32,
            "{size}: pair count"
        );

        // Build the same map through the real HAMT builder (a > 32-pair map is a hash map).
        let mut h = heap(1 << 16);
        let atoms = AtomTable::new();
        let pairs: Vec<(Eterm, Eterm)> = (1..=size)
            .map(|n| (make_small(n), make_small(n * 2)))
            .collect();
        // SAFETY: `map_words_capacity` is the documented upper bound for `write_map`
        // (`21n + 18` words for a hash map), and the heap holds at least that many.
        let hp = unsafe { h.alloc_words(map_words_capacity(pairs.len())) }.unwrap();
        // SAFETY: `hp` points at `map_words_capacity(size)` exclusively-owned, writable
        // words, the preconditions `write_map` documents.
        let map = unsafe { write_map(hp, &pairs, &atoms) }.unwrap();

        let ours = encode_term(map, &atoms).expect("a hash map is encodable");
        assert_eq!(
            ours, *image,
            "{size}-key map: our HAMT image must be byte-identical to the oracle's"
        );
    }
}

/// Feeds our `write_map`-built 40-key HAMT image to the live oracle's `binary_to_term/1`
/// and asserts the oracle reconstructs a 40-key map with the expected content — proof that
/// our HAMT traversal output is **valid wire**.
#[test]
fn oracle_accepts_our_hashmap_image() {
    let Some(oracle) = oracle_or_skip("oracle_accepts_our_hashmap_image") else {
        return;
    };

    use beam_term::map::{map_words_capacity, write_map};

    let mut h = heap(4096);
    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = (1..=40_i64)
        .map(|n| (make_small(n), make_small(n * 2)))
        .collect();
    // SAFETY: as in `hashmap_terms_encode_and_roundtrip` — `hp` owns
    // `map_words_capacity(40)` writable words, the documented `write_map` precondition.
    let hp = unsafe { h.alloc_words(map_words_capacity(pairs.len())) }.unwrap();
    // SAFETY: `hp` owns `map_words_capacity(40) >= map_words_capacity(n)` writable words.
    let map = unsafe { write_map(hp, &pairs, &atoms) }.unwrap();
    let image = encode_term(map, &atoms).unwrap();

    // Hand the oracle our image as a binary literal; it must accept it as a 40-pair map
    // with the expected content (each `=` is a match assertion that aborts the VM on
    // mismatch, which `run_erl` surfaces as a failure).
    let byte_list = image
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let eval = format!(
        "B = binary_to_term(<<{byte_list}>>), \
         true = is_map(B), 40 = map_size(B), \
         80 = maps:get(40, B), 2 = maps:get(1, B), \
         io:format(\"ok~n\", [])"
    );
    let out = run_erl(&oracle, &eval);
    assert!(
        out.contains("ok"),
        "the oracle must accept our hash-map image; got {out:?}"
    );
}
