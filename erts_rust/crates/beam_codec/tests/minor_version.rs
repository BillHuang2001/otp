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

//! `{minor_version, N}` wire flavours — byte-exact goldens + a live oracle check.
//!
//! `erlang:term_to_binary/2` accepts `{minor_version, N}` and turns it into a set of
//! encoder dflags (`parse_t2b_opts`, `external.c:1501-1516`):
//!
//! | `N` | `DFLAG_NEW_FLOATS` | `DFLAG_UTF8_ATOMS` | atoms | floats |
//! |---|---|---|---|---|
//! | 0 | no | no | `ATOM_EXT` (`100`) for Latin-1, UTF-8 tags otherwise | `FLOAT_EXT` (`99`) |
//! | 1 | yes | no | `ATOM_EXT` (`100`) for Latin-1, UTF-8 tags otherwise | `NEW_FLOAT_EXT` (`70`) |
//! | 2 | yes | yes | UTF-8 tags (`119`/`118`) | `NEW_FLOAT_EXT` (`70`) |
//!
//! `TERM_TO_BINARY_DFLAGS` (`dist.h:138-139`) is `DFLAG_NEW_FLOATS | DFLAG_UTF8_ATOMS`
//! and deliberately carries **no** `DFLAG_SMALL_ATOM_TAGS`, so `N` 0/1 emit every
//! Latin-1 atom as `ATOM_EXT` regardless of length (`enc_atom`, `external.c:2924`/`:2937`).
//!
//! Every vector below is a literal `term_to_binary(T, [{minor_version, N}])` image
//! captured from the reference implementation (OTP 29.1.1 / erts-17.1, `erl` on
//! `PATH`); regenerate with
//! `erl -noshell -eval 'io:format("~w~n",[term_to_binary(T,[{minor_version,N}])])' -s init stop`.
//!
//! ## Oracle gating
//!
//! The pure golden tests always run. `oracle_agrees_on_the_minor_version_corpus`
//! shells out to the system `erl` (resolved like `../tools/differential`:
//! `$ERTS_ORACLE` if it is an executable, else `erl` on `$PATH`) and **skips loudly**
//! with a stderr note when none exists — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;

use beam_codec::term::make_float;
use beam_codec::{
    decode_term, encode_term, encode_term_with_minor_version, AtomEncoding, Encoder, MinorVersion,
    ATOM_EXT, ATOM_UTF8_EXT, FLOAT_EXT, NEW_FLOAT_EXT, SMALL_ATOM_EXT, SMALL_ATOM_UTF8_EXT,
    VERSION_MAGIC,
};
use beam_term::prelude::{make_atom, AtomTable};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fresh decode/term heap for one test.
fn heap() -> Heap {
    Heap::new(1 << 14).expect("the carrier allocator never fails for this size")
}/// `term_to_binary(atom, [{minor_version, minor}])` for the atom named `name`.
fn encode_atom_named(name: &str, minor: MinorVersion) -> Vec<u8> {
    let mut atoms = AtomTable::new();
    let atom = atoms.intern(name);
    encode_term_with_minor_version(make_atom(atom), &atoms, minor).expect("the atom encodes")
}

/// The `FLOAT_EXT`/`NEW_FLOAT_EXT` image of `value` under `minor`.
fn encode_float_value(value: f64, minor: MinorVersion) -> Vec<u8> {
    let atoms = AtomTable::new();
    let mut heap = heap();
    let term = make_float(&mut heap, value).expect("the heap fits a float");
    encode_term_with_minor_version(term, &atoms, minor).expect("the float encodes")
}

/// A 31-byte `FLOAT_EXT` payload: the string bytes then zero padding.
fn float_payload(s: &str) -> Vec<u8> {
    let mut out = vec![0u8; 31];
    out[..s.len()].copy_from_slice(s.as_bytes());
    out
}

// ---------------------------------------------------------------------------
// Atoms
// ---------------------------------------------------------------------------

#[test]
fn short_latin1_atom_is_atom_ext_under_minor_versions_0_and_1() {
    // Oracle: term_to_binary(hello, [{minor_version,0}]) ==
    //         term_to_binary(hello, [{minor_version,1}]) ==
    //         <<131,100,0,5,104,101,108,108,111>>
    let expected = vec![131, 100, 0, 5, 104, 101, 108, 108, 111];
    assert_eq!(encode_atom_named("hello", MinorVersion::V0), expected);
    assert_eq!(encode_atom_named("hello", MinorVersion::V1), expected);
    // The default (v2) is the unchanged UTF-8 form.
    assert_eq!(
        encode_atom_named("hello", MinorVersion::V2),
        vec![131, 119, 5, 104, 101, 108, 108, 111]
    );
    // `encode_term` (no explicit flavour) must be byte-identical to v2.
    let mut atoms = AtomTable::new();
    let hello = atoms.intern("hello");
    assert_eq!(
        encode_term(make_atom(hello), &atoms).unwrap(),
        encode_atom_named("hello", MinorVersion::V2)
    );
}

#[test]
fn minority_latin1_atom_uses_one_byte_under_atom_ext() {
    // Oracle: term_to_binary('é', [{minor_version,0}]) == <<131,100,0,1,233>>
    let expected = vec![131, 100, 0, 1, 233];
    assert_eq!(encode_atom_named("\u{00e9}", MinorVersion::V0), expected);
    assert_eq!(encode_atom_named("\u{00e9}", MinorVersion::V1), expected);
    // Under v2 the same atom is UTF-8 (2 bytes).
    assert_eq!(
        encode_atom_named("\u{00e9}", MinorVersion::V2),
        vec![131, 119, 2, 0xC3, 0xA9]
    );
}

#[test]
fn non_latin1_atom_falls_back_to_utf8_tags_under_minor_versions_0_and_1() {
    // Oracle: term_to_binary('λ', [{minor_version,0}]) == <<131,119,2,206,187>>
    // (a non-Latin-1 name is emitted with the UTF-8 tags even though UTF8_ATOMS is
    // clear — `external.c:2910`: `utf8_atoms || a->latin1_chars < 0`).
    let expected = vec![131, 119, 2, 0xCE, 0xBB];
    assert_eq!(encode_atom_named("\u{03bb}", MinorVersion::V0), expected);
    assert_eq!(encode_atom_named("\u{03bb}", MinorVersion::V1), expected);

    // The same tag is used for a non-Latin-1 name whose UTF-8 image exceeds 255
    // bytes (130 × 'λ' = 260 bytes): `ATOM_UTF8_EXT` (118) + u16 length 260.
    // Oracle: <<131,118,1,4,...>>, total 264 bytes.
    let long = "\u{03bb}".repeat(130);
    let image = encode_atom_named(&long, MinorVersion::V0);
    assert_eq!(&image[..4], &[131, ATOM_UTF8_EXT, 0x01, 0x04]);
    assert_eq!(image.len(), 4 + 260);
}

#[test]
fn long_latin1_atom_is_atom_ext_under_minor_versions_0_and_1() {
    // 255 Latin-1 characters: still `ATOM_EXT` (100), because DFLAG_SMALL_ATOM_TAGS
    // is clear — never `SMALL_ATOM_EXT` (115).
    // Oracle: <<131,100,0,255,97,97,...>>, total 259 bytes.
    let name = "a".repeat(255);
    let image = encode_atom_named(&name, MinorVersion::V0);
    assert_eq!(&image[..4], &[131, ATOM_EXT, 0x00, 0xFF]);
    assert_eq!(image.len(), 4 + 255);
    assert_eq!(encode_atom_named(&name, MinorVersion::V1), image);
    assert_ne!(image[1], SMALL_ATOM_EXT);
    // Under the default v2 the same name is `SMALL_ATOM_UTF8_EXT` (119).
    assert_eq!(encode_atom_named(&name, MinorVersion::V2)[1], SMALL_ATOM_UTF8_EXT);
}

#[test]
fn the_distribution_latin1_flavour_is_unchanged() {
    // `AtomEncoding::Latin1` (the DFLAG_SMALL_ATOM_TAGS dist flavour) keeps its exact
    // old bytes: `SMALL_ATOM_EXT` (115) for a ≤255-byte name, and a typed
    // `unsupported` for a non-Latin-1 name — even when a minor version is also set.
    let mut atoms = AtomTable::new();
    let ok = atoms.intern("ok");
    let mut encoder = Encoder::new(&atoms);
    encoder.set_atom_encoding(AtomEncoding::Latin1);
    encoder.encode(make_atom(ok)).unwrap();
    assert_eq!(encoder.finish(), vec![131, 115, 2, 111, 107]);

    // A minor_version does not displace the dist flavour under v2 (it *does* pin the
    // tags under v0/v1 — asserted above — but `Latin1` semantics are untouched).
    let mut encoder = Encoder::new(&atoms);
    encoder.set_atom_encoding(AtomEncoding::Latin1);
    encoder.set_minor_version(MinorVersion::V2);
    encoder.encode(make_atom(ok)).unwrap();
    assert_eq!(encoder.finish(), vec![131, 115, 2, 111, 107]);

    let lambda = atoms.intern("\u{03bb}");
    let mut encoder = Encoder::new(&atoms);
    encoder.set_atom_encoding(AtomEncoding::Latin1);
    assert!(beam_codec::is_unsupported(
        &encoder.encode(make_atom(lambda)).unwrap_err()
    ));
}

// ---------------------------------------------------------------------------
// Floats
// ---------------------------------------------------------------------------

#[test]
fn float_ext_golden_3_14_under_minor_version_0() {
    // Oracle: term_to_binary(3.14, [{minor_version,0}]) ==
    //   <<131,99, "3.14000000000000012434e+00", 0,0,0,0,0>>
    let mut expected = vec![131, FLOAT_EXT];
    expected.extend_from_slice(&float_payload("3.14000000000000012434e+00"));
    assert_eq!(encode_float_value(3.14, MinorVersion::V0), expected);
    assert_eq!(expected.len(), 2 + 31);
}

#[test]
fn float_ext_goldens_for_a_spread_of_values() {
    // Oracle goldens: the exact 31-byte payload `sys_double_to_chars(f, buf, 31)`
    // writes (`%.20e` then zero-fill). Verified against the system `erl`.
    let cases: &[(f64, &str)] = &[
        (0.0, "0.00000000000000000000e+00"),
        (1.0, "1.00000000000000000000e+00"),
        (-2.5, "-2.50000000000000000000e+00"),
        (1.0e-5, "1.00000000000000008180e-05"),
        (1.0e20, "1.00000000000000000000e+20"),
        (1.0e-300, "1.00000000000000002506e-300"),
        (123_456_789.123_456, "1.23456789123456001282e+08"),
        (5.0e-324, "4.94065645841246544177e-324"),
        // Round-half-to-even tie (exact value 644048203731.4326171875).
        (644_048_203_731.432_6, "6.44048203731432617188e+11"),
    ];
    for (value, text) in cases {
        let mut expected = vec![131, FLOAT_EXT];
        expected.extend_from_slice(&float_payload(text));
        assert_eq!(
            encode_float_value(*value, MinorVersion::V0),
            expected,
            "FLOAT_EXT image mismatch for {value:?}"
        );
        // FLOAT_EXT is written only under v0; v1 keeps the modern form.
        assert_eq!(encode_float_value(*value, MinorVersion::V1)[1], NEW_FLOAT_EXT);
        assert_eq!(encode_float_value(*value, MinorVersion::V2)[1], NEW_FLOAT_EXT);
    }
}

#[test]
fn minor_version_2_keeps_new_float_ext() {
    // Oracle: term_to_binary(3.14, []) == term_to_binary(3.14, [{minor_version,2}])
    //   == <<131,70,64,9,30,184,81,235,133,31>>
    let expected = vec![131, 70, 64, 9, 30, 184, 81, 235, 133, 31];
    assert_eq!(encode_float_value(3.14, MinorVersion::V2), expected);
    let atoms = AtomTable::new();
    let mut heap = heap();
    let term = make_float(&mut heap, 3.14).unwrap();
    assert_eq!(encode_term(term, &atoms).unwrap(), expected);
}

#[test]
fn float_ext_images_round_trip_through_our_decoder() {
    // Decoding a FLOAT_EXT image yields the float value; re-encoding under the same
    // minor version reproduces the bytes exactly (the text form is a fixed point).
    for (value, minor) in [
        (3.14f64, MinorVersion::V0),
        (-2.5, MinorVersion::V0),
        (1.0e-300, MinorVersion::V0),
    ] {
        let image = encode_float_value(value, minor);
        let mut atoms = AtomTable::new();
        let mut heap = heap();
        let term = decode_term(&image, &mut heap, &mut atoms)
            .unwrap_or_else(|e| panic!("our decoder rejected {image:?}: {e:?}"));
        let round = encode_term_with_minor_version(term, &atoms, minor).unwrap();
        assert_eq!(round, image, "FLOAT_EXT round trip changed the bytes");
    }
}

#[test]
fn atoms_round_trip_through_our_decoder_under_minor_versions_0_and_1() {
    for name in ["hello", "\u{00e9}", "\u{03bb}", &"a".repeat(255)] {
        for minor in [MinorVersion::V0, MinorVersion::V1] {
            let image = encode_atom_named(name, minor);
            let mut atoms = AtomTable::new();
            let mut heap = heap();
            let term = decode_term(&image, &mut heap, &mut atoms)
                .unwrap_or_else(|e| panic!("our decoder rejected {image:?}: {e:?}"));
            let mut expected = AtomTable::new();
            let expected_atom = expected.intern(name);
            assert_eq!(
                beam_term::prelude::atom_val(term),
                expected_atom,
                "the decoded atom index does not name {name:?}"
            );
            // The tag family is the minor-version one.
            assert!(matches!(
                image[1],
                ATOM_EXT | SMALL_ATOM_UTF8_EXT | ATOM_UTF8_EXT
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
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

/// Resolve the oracle the same way `tools/differential` does: `$ERTS_ORACLE` if it is
/// an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
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

/// Returns the oracle, or prints a loud skip note and returns `None`.
fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("=========== beam_codec minor_version: {test} SKIPPED ===========");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
            eprintln!("================================================================");
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
                        t.trim()
                            .parse::<u8>()
                            .unwrap_or_else(|_| panic!("bad byte {t:?} in the oracle output"))
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

/// The corpus: **one Erlang expression** evaluating to the list of terms whose
/// `{minor_version,0|1}` images we compare. It covers every atom flavour (short and
/// long Latin-1, multi-byte UTF-8, non-Latin-1) and a spread of floats alongside
/// integers, tuples, lists, strings, maps and binaries.
const CORPUS: &str = r#"[ hello,
                           list_to_atom([16#00E9]),
                           list_to_atom([16#03BB]),
                           list_to_atom(lists:duplicate(255, $a)),
                           list_to_atom(lists:duplicate(130, 16#03BB)),
                           0, 255, 256, -1, 1234567890123456789012345,
                           3.14, 1.0e-5, 0.0, -2.5, 644048203731.4326, 1.0e300,
                           {ok,17}, [1,2,3], "abc", #{a=>1}, {hello, 3.14}, [] ]"#;

/// How many entries [`CORPUS`] evaluates to.
const CORPUS_LEN: usize = 22;

#[test]
fn oracle_agrees_on_the_minor_version_corpus() {
    let Some(oracle) = oracle_or_skip("oracle_agrees_on_the_minor_version_corpus") else {
        return;
    };

    for (minor, opt) in [
        (MinorVersion::V0, "{minor_version,0}"),
        (MinorVersion::V1, "{minor_version,1}"),
    ] {
        let eval = format!(
            "io:format(\"~w~n\", [ [ term_to_binary(T,[{opt}]) || T <- {CORPUS} ] ])"
        );
        let images = parse_binary_list(&run_erl(&oracle, &eval));
        assert_eq!(
            images.len(),
            CORPUS_LEN,
            "the oracle printed {} images for the {minor:?} corpus",
            images.len()
        );
        assert!(
            images.iter().all(|i| i.first() == Some(&VERSION_MAGIC)),
            "every oracle image must start with the version byte {VERSION_MAGIC}"
        );

        let mut exact = 0usize;
        for (i, bi) in images.iter().enumerate() {
            let mut atoms = AtomTable::new();
            let mut heap = heap();
            let term = decode_term(bi, &mut heap, &mut atoms).unwrap_or_else(|e| {
                panic!("{minor:?} corpus #{i}: our decoder rejected the oracle image {bi:?}: {e:?}")
            });
            let ours = encode_term_with_minor_version(term, &atoms, minor).unwrap_or_else(|e| {
                panic!("{minor:?} corpus #{i}: our encoder rejected the decoded term: {e:?}")
            });
            assert_eq!(
                ours, *bi,
                "{minor:?} corpus #{i}: our bytes differ from the oracle\n oracle = {bi:?}\n ours   = {ours:?}"
            );
            exact += 1;
        }
        eprintln!("[beam_codec minor_version] {minor:?}: {exact} images byte-identical to the oracle");
    }
}
