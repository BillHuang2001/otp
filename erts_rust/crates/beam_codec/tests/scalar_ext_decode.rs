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

//! Scalar `binary_to_term/1` decode parity with the system oracle.
//!
//! Three scalar ETF wire forms are *not* carried over verbatim from the wire into
//! the term layer — the decoder has to apply the same normalization / rejection the
//! C `dec_term` applies (`../../erts/emulator/beam/external.c`):
//!
//! 1. **A bignum's sign byte is a boolean.** C reads `neg = get_int8(ep)` and only
//!    tests it for zero-ness (`bytes_to_big`'s `if (xsgn) d = -d;`, `big.c:3446-3448`),
//!    so **any** non-zero sign byte means negative — `2` and `1` both negate — while a
//!    zero magnitude is `0` whatever the sign byte is (`external.c:4484-4486`).
//! 2. **`NEW_FLOAT_EXT` (`70`) cannot carry a non-finite value.**
//!    `if (!erts_isfinite(ff.fd)) goto error;` (`external.c:4667-4669`) makes an
//!    infinity or a NaN a `badarg` rather than the term `f64::from_bits` would build.
//! 3. **An over-long atom name is malformed input, not a resource limit.** A name
//!    longer than `MAX_ATOM_CHARACTERS` (255 *characters*, not bytes) is rejected in
//!    `erts_atom_put`/`erts_atom_get` (`atom.c:258-274`, `atom.c:392-418`) and
//!    `dec_term` turns that into `goto error` (`external.c:4529-4539`), i.e. `badarg`
//!    — not `system_limit`.
//!
//! Each case is asserted (a) against a pinned classification the system oracle
//! (OTP 29.1.1 / erts-17.1) produces, so the test is meaningful with no oracle
//! installed, and (b) **live** against a real `erl` that runs the same classification
//! over the same byte vectors. The oracle is `$ERTS_ORACLE` when that is an existing
//! executable, else `erl` on `$PATH`; when neither exists the live test prints a loud
//! stderr note and returns — a skip, never a silent pass (the convention of
//! `tests/interop.rs` and `tests/internal_ids.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::term::{big_parts, float_value};
use beam_codec::{
    decode_term, ATOM_EXT, ATOM_UTF8_EXT, NEW_FLOAT_EXT, SMALL_ATOM_UTF8_EXT, SMALL_BIG_EXT,
    VERSION_MAGIC,
};
use beam_common::BeamError;
use beam_term::{atom_val, is_atom, is_small, signed_value, AtomTable};

/// A decode heap of `words` term words.
fn new_heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

// ---------------------------------------------------------------------------
// The corpus: byte images of the three scalar wire forms, plus their positive twins
// ---------------------------------------------------------------------------

/// Builds the shared corpus: `(label, image)` in the order the oracle prints.
///
/// The `a`-runs are built as ASCII so a Latin-1 and a UTF-8 atom of the same length
/// are byte-identical on the wire.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    /// `VERSION_MAGIC`, `tag`, then `len_bytes` and `extra`.
    fn tagged(tag: u8, len_bytes: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut image = vec![VERSION_MAGIC, tag];
        image.extend_from_slice(len_bytes);
        image.extend_from_slice(extra);
        image
    }

    /// A `SMALL_BIG_EXT` image with an 8-bit byte count.
    fn big(byte_count: u8, sign: u8, magnitude: &[u8]) -> Vec<u8> {
        tagged(SMALL_BIG_EXT, &[byte_count, sign], magnitude)
    }

    /// A `NEW_FLOAT_EXT` image from the 8 bytes of an `f64`.
    fn new_float(bytes: [u8; 8]) -> Vec<u8> {
        tagged(NEW_FLOAT_EXT, &[], &bytes)
    }

    vec![
        // 1. Bignum sign byte: any non-zero means negative.
        ("big sign=2 mag=42", big(1, 2, &[42])),
        ("big sign=5 mag=0", big(1, 5, &[0])),
        ("big sign=0 mag=42", big(1, 0, &[42])),
        ("big sign=1 mag=42", big(1, 1, &[42])),
        ("big sign=200 mag=2^64", big(9, 200, &[0, 0, 0, 0, 0, 0, 0, 0, 1])),
        // 2. `NEW_FLOAT_EXT` must be finite.
        ("new_float +inf", new_float([127, 240, 0, 0, 0, 0, 0, 0])),
        ("new_float -inf", new_float([255, 240, 0, 0, 0, 0, 0, 0])),
        ("new_float NaN", new_float([127, 248, 0, 0, 0, 0, 0, 0])),
        ("new_float signaling NaN", new_float([127, 240, 0, 0, 0, 0, 0, 1])),
        ("new_float -0.0", new_float([128, 0, 0, 0, 0, 0, 0, 0])),
        ("new_float 3.14", new_float([64, 9, 30, 184, 81, 235, 133, 31])),
        // 3. Atom name length is a *character* count, and an over-long name is badarg.
        ("atom 256 chars (latin1)", tagged(ATOM_EXT, &[1, 0], &[b'b'; 256])),
        ("atom 256 chars (utf8)", tagged(ATOM_UTF8_EXT, &[1, 0], &[b'b'; 256])),
        ("atom 255 chars (latin1)", tagged(ATOM_EXT, &[0, 255], &[b'a'; 255])),
        (
            "atom 255 chars (utf8, small tag)",
            tagged(SMALL_ATOM_UTF8_EXT, &[255], &[b'a'; 255]),
        ),
    ]
}

/// The classification the system oracle prints for each [`corpus`] entry, in order
/// (OTP 29.1.1 / erts-17.1, `binary_to_term/1`; see the oracle cross-check test, which
/// re-derives these live).
fn pinned_classes() -> Vec<String> {
    let a255 = "a".repeat(255);
    vec![
        "int:-42".to_string(),
        "int:0".to_string(),
        "int:42".to_string(),
        "int:-42".to_string(),
        "int:-18446744073709551616".to_string(),
        "badarg".to_string(),
        "badarg".to_string(),
        "badarg".to_string(),
        "badarg".to_string(),
        "float:8000000000000000".to_string(),
        "float:40091eb851eb851f".to_string(),
        "badarg".to_string(),
        "badarg".to_string(),
        format!("atom:{a255}"),
        format!("atom:{a255}"),
    ]
}

// ---------------------------------------------------------------------------
// Our side of the comparison
// ---------------------------------------------------------------------------

/// Renders a decoded bignum as the oracle's `integer_to_list/1` would.
///
/// The corpus magnitudes fit two `ErtsDigit`s; anything wider is rendered as
/// `bignum:<sign>:<digits>` (never reached by this corpus, but the classifier must not
/// overflow for a caller that reuses it).
fn render_big(negative: bool, digits: &[u64]) -> String {
    if digits.len() > 2 {
        return format!("bignum:{negative}:{digits:?}");
    }
    let mut value: i128 = 0;
    for &digit in digits.iter().rev() {
        value = (value << 64) | i128::from(digit);
    }
    if negative {
        value = -value;
    }
    format!("int:{value}")
}

/// Classifies one wire image the same way the oracle's Erlang classifier does:
/// `int:<decimal>`, `float:<16 hex digits of the f64 bits>`, `atom:<name>`, `badarg`, or
/// `err:<debug>` for any other typed error (never expected here).
fn classify(image: &[u8]) -> String {
    let mut heap = new_heap(4096);
    let mut atoms = AtomTable::new();
    match decode_term(image, &mut heap, &mut atoms) {
        Err(BeamError::BadArg) => "badarg".to_string(),
        Err(err) => format!("err:{err:?}"),
        Ok(term) => {
            if is_small(term) {
                return format!("int:{}", signed_value(term));
            }
            if let Ok((negative, digits)) = big_parts(term) {
                return render_big(negative, digits);
            }
            if let Ok(value) = float_value(term) {
                return format!("float:{:016x}", value.to_bits());
            }
            if is_atom(term) {
                let name = atoms.name(atom_val(term)).unwrap_or("<unnamed>");
                return format!("atom:{name}");
            }
            "other".to_string()
        }
    }
}

/// The newline-free `|`-joined classification of the whole corpus.
fn classify_corpus() -> String {
    corpus()
        .iter()
        .map(|(_, image)| classify(image))
        .collect::<Vec<_>>()
        .join("|")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The pinned classification is what our decoder produces, so the three scalar rules
/// are covered even with no `erl` installed.
#[test]
fn scalar_decode_classes_match_the_pinned_oracle_classes() {
    let expected = pinned_classes();
    let actual: Vec<String> = corpus()
        .iter()
        .map(|(_, image)| classify(image))
        .collect();
    assert_eq!(
        actual.len(),
        expected.len(),
        "the corpus and the pinned classification must stay in step"
    );
    for ((label, _), (got, want)) in corpus().iter().zip(actual.iter().zip(expected.iter())) {
        assert_eq!(got, want, "case `{label}`");
    }
}

/// The three scalar wire rules, asserted directly (independent of the classifier).
#[test]
fn scalar_wire_rules_are_applied() {
    // (1) Any non-zero sign byte means negative; a zero magnitude is zero.
    let mut h = new_heap(256);
    let mut atoms = AtomTable::new();
    let t = decode_term(&[VERSION_MAGIC, SMALL_BIG_EXT, 1, 2, 42], &mut h, &mut atoms).unwrap();
    assert!(is_small(t));
    assert_eq!(signed_value(t), -42);

    let mut h = new_heap(256);
    let mut atoms = AtomTable::new();
    let t = decode_term(&[VERSION_MAGIC, SMALL_BIG_EXT, 1, 5, 0], &mut h, &mut atoms).unwrap();
    assert_eq!(signed_value(t), 0);

    let mut h = new_heap(256);
    let mut atoms = AtomTable::new();
    let t = decode_term(&[VERSION_MAGIC, SMALL_BIG_EXT, 1, 0, 42], &mut h, &mut atoms).unwrap();
    assert_eq!(signed_value(t), 42);

    // (2) A non-finite `NEW_FLOAT_EXT` is `badarg`; a finite one (incl. `-0.0`) decodes.
    for bits in [
        f64::INFINITY.to_bits(),
        f64::NEG_INFINITY.to_bits(),
        f64::NAN.to_bits(),
        0x7ff0_0000_0000_0001, // signaling NaN
        0x7ff8_0000_dead_beef, // quiet NaN with a payload
    ] {
        let mut image = vec![VERSION_MAGIC, NEW_FLOAT_EXT];
        image.extend_from_slice(&bits.to_be_bytes());
        let mut h = new_heap(256);
        let mut atoms = AtomTable::new();
        assert!(
            matches!(
                decode_term(&image, &mut h, &mut atoms),
                Err(BeamError::BadArg)
            ),
            "bits {bits:#018x} must be rejected"
        );
    }
    let mut image = vec![VERSION_MAGIC, NEW_FLOAT_EXT];
    image.extend_from_slice(&(-0.0f64).to_bits().to_be_bytes());
    let mut h = new_heap(256);
    let mut atoms = AtomTable::new();
    let t = decode_term(&image, &mut h, &mut atoms).unwrap();
    assert_eq!(float_value(t).unwrap().to_bits(), (-0.0f64).to_bits());

    // (3) A 256-character atom is `BadArg` (not `SystemLimit`); 255 still decodes, and
    //     the limit counts characters rather than UTF-8 bytes.
    for image in [
        {
            let mut i = vec![VERSION_MAGIC, ATOM_EXT, 1, 0];
            i.extend_from_slice(&[b'b'; 256]);
            i
        },
        {
            let mut i = vec![VERSION_MAGIC, ATOM_UTF8_EXT, 1, 0];
            i.extend_from_slice(&[b'b'; 256]);
            i
        },
    ] {
        let mut h = new_heap(256);
        let mut atoms = AtomTable::new();
        assert!(matches!(
            decode_term(&image, &mut h, &mut atoms),
            Err(BeamError::BadArg)
        ));
    }

    let mut at_limit = vec![VERSION_MAGIC, ATOM_EXT, 0, 255];
    at_limit.extend_from_slice(&[b'a'; 255]);
    let mut h = new_heap(256);
    let mut atoms = AtomTable::new();
    let t = decode_term(&at_limit, &mut h, &mut atoms).unwrap();
    assert_eq!(atoms.name(atom_val(t)).map(str::len), Some(255));

    let mut multi_byte = vec![VERSION_MAGIC, ATOM_UTF8_EXT, 1, 44];
    for _ in 0..150 {
        multi_byte.extend_from_slice(&[0xC3, 0xA9]); // "é"
    }
    let mut h = new_heap(256);
    let mut atoms = AtomTable::new();
    let t = decode_term(&multi_byte, &mut h, &mut atoms).unwrap();
    let name = atoms.name(atom_val(t)).unwrap();
    assert_eq!((name.chars().count(), name.len()), (150, 300));
}

/// The live cross-check: the oracle runs the *same* classification over the *same*
/// byte vectors and the two strings must be identical. Skips loudly when no `erl`
/// exists.
#[test]
fn scalar_decode_classes_match_the_system_oracle() {
    let Some(oracle) = oracle_or_skip("scalar_decode_classes_match_the_system_oracle") else {
        return;
    };

    let images = corpus()
        .iter()
        .map(|(_, image)| binary_text(image))
        .collect::<Vec<_>>()
        .join(", ");
    let eval = format!(
        "C = fun(B) -> try binary_to_term(B) of \
           V when is_integer(V) -> \"int:\" ++ integer_to_list(V); \
           V when is_float(V) -> \"float:\" ++ lists:flatten(io_lib:format(\"~.16b\", \
             [binary:decode_unsigned(<<V:64/float>>)])); \
           V when is_atom(V) -> \"atom:\" ++ atom_to_list(V); \
           _ -> \"other\" \
         catch error:badarg -> \"badarg\" end end, \
         io:format(\"~s\", [lists:join(\"|\", [C(B) || B <- [{images}]])])"
    );
    let stdout = run_erl(&oracle, &eval);

    let ours = classify_corpus();
    let theirs = stdout.trim();
    assert_eq!(
        ours, theirs,
        "our per-case classification must equal the oracle's\n--- ours ---\n{ours}\n--- oracle ---\n{theirs}"
    );
    // Belt and braces: the pinned table is the oracle's own output.
    assert_eq!(pinned_classes().join("|"), theirs);
}

// ---------------------------------------------------------------------------
// Oracle resolution (mirrors `tests/interop.rs` / `tests/internal_ids.rs`)
// ---------------------------------------------------------------------------

/// `true` when `p` is an existing executable file.
#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

/// `true` when `p` is an existing executable file (non-unix fallback).
#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// `$ERTS_ORACLE` if it is an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec scalar_ext_decode] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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

/// Returns the oracle, or prints a loud skip note and returns `None`.
fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("============ beam_codec scalar_ext_decode: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("======================================================================");
            eprintln!();
            None
        }
    }
}

/// Runs one `erl -noshell -eval <eval> -s init stop` and returns its trimmed stdout.
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
        .trim()
        .to_string()
}

/// The `<<131,97,...>>` textual form `io:format("~w", [Bin])` prints for `bytes`.
fn binary_text(bytes: &[u8]) -> String {
    let joined = bytes
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("<<{joined}>>")
}
