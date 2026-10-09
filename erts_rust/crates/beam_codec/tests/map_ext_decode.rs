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

//! `MAP_EXT` (`116`) **decode** fidelity against the live oracle.
//!
//! ERTS does not merely *read* a map off the wire — it **canonicalizes** it in the
//! `external.c:5438-5456` fixup, reached through `external.c:5042-5101`:
//!
//! - `count <= MAP_SMALL_MAP_LIMIT` (32) ⇒ a **flat map** whose keys are sorted in the
//!   internal map-key order `erts_cmp_flatmap_keys`
//!   ([`erts_validate_and_sort_flatmap`], `erl_map.c:3187`);
//! - `count > 32` ⇒ a **hash map** (HAMT) keyed by `erts_map_hash`
//!   (`erts_hashmap_from_array(..., 1)`, `erl_map.c:644`);
//! - a **duplicate key** is rejected (`badarg` in *both* representations) — only
//!   `maps:from_list/1` collapses duplicates, never `binary_to_term/1`;
//! - a pair count with bit 31 set is malformed (`(n >> 31) != 0`, `external.c:6312`).
//!
//! These tests pin our decoder to exactly that: duplicate keys ⇒ `badarg`, an unsorted
//! small image decodes to the same (sorted) term as the oracle, and the oracle's own
//! `term_to_binary/1` image re-encodes **byte-identically** here. The oracle is the
//! system `erl`; every case skips **loudly** (never a silent pass) without one.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::term::flatmap_parts;
use beam_codec::{decode_term, encode_term, MAP_EXT, VERSION_MAGIC};
use beam_common::BeamError;
use beam_term::prelude::{make_small, AtomTable};

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
            "[beam_codec map_ext_decode] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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
            eprintln!("=============== beam_codec map_ext_decode: {test} SKIPPED ===============");
            eprintln!("No usable system `erl` oracle found — see the note above, if any");
            eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("==========================================================================");
            eprintln!();
            None
        }
    }
}

/// Run one `erl -noshell -eval <eval> -s init stop` and return its stdout. The eval must
/// abort the VM (a failed match) on any unexpected result, which surfaces as a failure.
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

/// Render a byte image as the `<<b1,b2,...>>` Erlang literal the oracle's `-eval` accepts.
fn byte_literal(image: &[u8]) -> String {
    let list = image
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("<<{list}>>")
}

/// Parse the first `<<...>>` byte image the oracle prints, if any.
fn parse_binary(out: &str) -> Vec<u8> {
    let bytes = out.as_bytes();
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
            return if body.trim().is_empty() {
                Vec::new()
            } else {
                body.split(',')
                    .map(|t| {
                        t.trim()
                            .parse::<u8>()
                            .unwrap_or_else(|_| panic!("bad byte {t:?} in oracle output {out:?}"))
                    })
                    .collect()
            };
        }
        i += 1;
    }
    Vec::new()
}

/// A fresh heap big enough for any map in this file.
fn heap() -> Heap {
    Heap::new(1 << 16).expect("the carrier allocator never fails for this size")
}

/// `#{1 => 10, 1 => 20}` — **key `1` twice** (two pairs, both keyed by the small int `1`).
const DUPLICATE_KEY_MAP: &[u8] = &[VERSION_MAGIC, MAP_EXT, 0, 0, 0, 2, 97, 1, 97, 10, 97, 1, 97, 20];

/// `#{10 => 1, 2 => 2, 3 => 3}` written in **unsorted** wire order (`10,2,3`); the oracle
/// canonicalizes it to `#{2=>2,3=>3,10=>1}`.
const UNSORTED_SMALL_MAP: &[u8] = &[
    VERSION_MAGIC, MAP_EXT, 0, 0, 0, 3, 97, 10, 97, 1, 97, 2, 97, 2, 97, 3, 97, 3,
];

// ---------------------------------------------------------------------------
// (a) A duplicate key is rejected, exactly as the oracle rejects it.
// ---------------------------------------------------------------------------

#[test]
fn duplicate_key_map_ext_is_rejected_like_the_oracle() {
    let Some(oracle) = oracle_or_skip("duplicate_key_map_ext_is_rejected_like_the_oracle") else {
        return;
    };

    // The oracle's `binary_to_term/1` of the duplicate-key image must be `badarg` — the
    // `external.c:5448` `error_map_fixup` path. `catch` turns the error into a term so the
    // VM survives; a match assertion confirms the reason is `badarg`.
    let out = run_erl(
        &oracle,
        &format!(
            "{{'EXIT', {{badarg, _}}}} = catch binary_to_term({}), io:format(\"badarg~n\", [])",
            byte_literal(DUPLICATE_KEY_MAP)
        ),
    );
    assert!(
        out.contains("badarg"),
        "the oracle must reject the duplicate-key image with badarg; got {out:?}"
    );

    // Ours: the same image is a decode error, and it is the SAME error class (`BadArg`,
    // whose atom is `badarg`) — ERTS rejects duplicates in both representations, so this
    // is not a case where we may collapse them.
    let mut h = heap();
    let mut atoms = AtomTable::new();
    let err = decode_term(DUPLICATE_KEY_MAP, &mut h, &mut atoms)
        .expect_err("a duplicate-key MAP_EXT must not decode");
    assert!(
        matches!(err, BeamError::BadArg),
        "our error class must match the oracle's badarg; got {err:?}"
    );
    assert_eq!(err.as_atom(), Some("badarg"));
}

// ---------------------------------------------------------------------------
// (b) An unsorted small map decodes to the oracle's (canonicalized) term.
// ---------------------------------------------------------------------------

#[test]
fn unsorted_small_map_decodes_to_the_oracle_term() {
    let Some(oracle) = oracle_or_skip("unsorted_small_map_decodes_to_the_oracle_term") else {
        return;
    };

    // The oracle accepts the unsorted image and canonicalizes it: `binary_to_term` of the
    // raw image then `term_to_binary` gives the sorted wire order (`2,3,10`), and it must
    // equal the image of the literal map `#{2=>2,3=>3,10=>1}`.
    let oracle_image = parse_binary(&run_erl(
        &oracle,
        &format!(
            "B = binary_to_term({}), io:format(\"~w~n\", [term_to_binary(B)])",
            byte_literal(UNSORTED_SMALL_MAP)
        ),
    ));
    assert_eq!(oracle_image[0], VERSION_MAGIC);
    assert_eq!(oracle_image[1], MAP_EXT);

    // Ours: decode the unsorted image, then re-encode. The result must be byte-identical
    // to the oracle's canonical image (integer keys — order is stable across VM starts).
    let mut h = heap();
    let mut atoms = AtomTable::new();
    let term = decode_term(UNSORTED_SMALL_MAP, &mut h, &mut atoms).expect("the image is valid");
    let ours = encode_term(term, &atoms).expect("a decoded map re-encodes");
    assert_eq!(
        ours, oracle_image,
        "our re-encode of an unsorted MAP_EXT must match the oracle's canonical image"
    );

    // And the decoded map is a *flat* map (3 <= 32) whose keys are canonicalized.
    let (keys, _values) = flatmap_parts(term).expect("a <= 32-pair map is a flat map");
    assert_eq!(keys, &[make_small(2), make_small(3), make_small(10)]);
}

// ---------------------------------------------------------------------------
// (c) The oracle's own > 32-pair image decodes and re-encodes byte-identically.
// ---------------------------------------------------------------------------

#[test]
fn oracle_hashmap_image_decodes_and_reencodes_byte_identically() {
    let Some(oracle) = oracle_or_skip("oracle_hashmap_image_decodes_and_reencodes_byte_identically")
    else {
        return;
    };

    // Integer keys keep this comparable across VM starts (atom-keyed HAMTs are not
    // byte-stable). 40 distinct keys > MAP_SMALL_MAP_LIMIT ⇒ the oracle builds a HAMT.
    let oracle_image = parse_binary(&run_erl(
        &oracle,
        "io:format(\"~w~n\", [term_to_binary(maps:from_list([{N, N*2} || N <- lists:seq(1,40)]))])",
    ));
    assert_eq!(oracle_image[0], VERSION_MAGIC);
    assert_eq!(oracle_image[1], MAP_EXT);
    assert_eq!(
        u32::from_be_bytes([
            oracle_image[2],
            oracle_image[3],
            oracle_image[4],
            oracle_image[5]
        ]),
        40
    );

    // Ours: the HAMT image decodes to a > 32-pair map (a HAMT, so `flatmap_parts` fails)
    // and re-encodes byte-identically — our HAMT builder uses the bit-exact `erts_map_hash`.
    let mut h = heap();
    let mut atoms = AtomTable::new();
    let term = decode_term(&oracle_image, &mut h, &mut atoms).expect("the oracle image decodes");
    assert!(
        flatmap_parts(term).is_err(),
        "a > 32-pair map is a HAMT, not a flat map"
    );
    assert_eq!(beam_term::map::map_size(term, &atoms).unwrap(), 40);
    assert_eq!(
        beam_term::map::map_get(term, make_small(40), &atoms).unwrap(),
        Some(make_small(80))
    );
    let ours = encode_term(term, &atoms).expect("a decoded map re-encodes");
    assert_eq!(
        ours, oracle_image,
        "our re-encode of the oracle's HAMT image must be byte-identical"
    );
}

// ---------------------------------------------------------------------------
// (d) `1` and `1.0` are distinct keys — accepted by both.
// ---------------------------------------------------------------------------

/// `1.0` as `NEW_FLOAT_EXT` (`70`) + 8 IEEE-754 bytes.
const NEW_FLOAT_1_0: [u8; 9] = [70, 63, 240, 0, 0, 0, 0, 0, 0];

#[test]
fn integer_and_float_keys_stay_distinct() {
    let Some(oracle) = oracle_or_skip("integer_and_float_keys_stay_distinct") else {
        return;
    };

    // `#{1 => 10, 1.0 => 20}` — a small int and a float. Maps key by `=:=`, so these are
    // two distinct keys; neither implementation may treat the pair as a duplicate.
    let mut image = vec![VERSION_MAGIC, MAP_EXT, 0, 0, 0, 2, 97, 1, 97, 10];
    image.extend_from_slice(&NEW_FLOAT_1_0);
    image.push(97);
    image.push(20);

    // The oracle accepts it as a two-key map and its `term_to_binary` image is the
    // canonical (sorted) form.
    let oracle_image = parse_binary(&run_erl(
        &oracle,
        &format!(
            "B = binary_to_term({b}), 2 = maps:size(B), 10 = maps:get(1, B), \
             20 = maps:get(1.0, B), io:format(\"~w~n\", [term_to_binary(B)])",
            b = byte_literal(&image)
        ),
    ));
    assert_eq!(oracle_image[0], VERSION_MAGIC);
    assert_eq!(oracle_image[1], MAP_EXT);

    // Ours: decodes to a 2-pair map (not collapsed to 1) and re-encodes byte-identically.
    let mut h = heap();
    let mut atoms = AtomTable::new();
    let term = decode_term(&image, &mut h, &mut atoms).expect("1 and 1.0 are two keys");
    assert_eq!(
        beam_term::map::map_size(term, &atoms).unwrap(),
        2,
        "`1` and `1.0` must stay distinct keys"
    );
    assert_eq!(encode_term(term, &atoms).unwrap(), oracle_image);
}
