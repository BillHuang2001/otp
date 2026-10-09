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

//! `binary_to_term/1,2` **length/arity-bound** error class parity with the system
//! oracle.
//!
//! The C routes *every* decode failure to `B2TBadArg`, bounds included:
//! `binary_to_term_int` maps both the `decoded_size` (sizing-pass) failure and the
//! `dec_term` (decode-pass) failure onto the same `B2TBadArg` state, which raises
//! `BADARG` (`../../erts/emulator/beam/external.c:1989-1991`, `:2068-2087`). A
//! `system_limit` raise is therefore **never** an oracle answer for a malformed or
//! over-long wire image, so our decoder's own length/arity-bound guards must also be
//! `BeamError::BadArg` (they used to be `BeamError::SystemLimit`, which
//! `beam_bifs::binconv::decode_term_bif` propagated straight to the caller — an
//! observable `error:system_limit` where the oracle raises `error:badarg`).
//!
//! The bounds covered here (each verified live against OTP 29.1.1 / erts-17.1):
//!
//! | wire form | bound | C reference |
//! |---|---|---|
//! | `LARGE_TUPLE_EXT` (`105`) arity | `> MAX_ARITYVAL` (`2^24 - 1`) | `decoded_size`, `external.c:6135` |
//! | `LARGE_BIG_EXT` (`111`) byte count | `> BIG_ARITY_MAX * sizeof(ErtsDigit)` (`65535 * 8 = 524280`) | `external.c:6146` |
//! | `NEWER_REFERENCE_EXT` (`90`) id count | `> ERTS_MAX_REF_NUMBERS` (`5`) | `ref_ext_common`, `external.c:4788` |
//! | `RECORD_EXT` (`67`) field count | `record_def_size_words(n) - 1 > MAX_ARITYVAL` | `external.c:6447` |
//! | `STRING_EXT` (`107`) / `LIST_EXT` (`108`) length | a lying length runs off the cursor | `external.c:6120`-`6140` |
//!
//! Each case is asserted (a) against a pinned classification the system oracle
//! produces, so the test is meaningful with no oracle installed, and (b) **live**
//! against a real `erl` that classifies the same byte vectors. The oracle is
//! `$ERTS_ORACLE` when that is an existing executable, else `erl` on `$PATH`; when
//! neither exists the live test prints a loud stderr note and returns — a skip, never
//! a silent pass (the convention of `tests/interop.rs`, `tests/internal_ids.rs` and
//! `tests/scalar_ext_decode.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::{
    decode_term, LARGE_BIG_EXT, LARGE_TUPLE_EXT, LIST_EXT, NEWER_REFERENCE_EXT, RECORD_EXT,
    STRING_EXT, VERSION_MAGIC,
};
use beam_common::BeamError;
use beam_term::AtomTable;

/// A decode heap of `words` term words.
fn new_heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

/// `BIG_ARITY_MAX * sizeof(ErtsDigit)` (`erl_term.h:466`, `external.c:6146`): the
/// largest `LARGE_BIG_EXT` byte count the wire may carry.
const BIG_BYTE_BOUND: usize = 65_535 * 8;

// ---------------------------------------------------------------------------
// The corpus: one entry per length/arity bound, plus an in-bound twin
// ---------------------------------------------------------------------------

/// One corpus entry: the wire image, its Erlang twin, and the oracle's answer.
struct Case {
    /// A short human label, used in assertion messages.
    label: &'static str,
    /// The exact bytes our decoder is fed.
    image: Vec<u8>,
    /// The Erlang expression that builds the same image for the oracle. Kept short
    /// even for the half-megabyte bignum images (`binary:copy/2` rather than a
    /// megabyte of literal source, which would overflow the `-eval` argument limit).
    erl: String,
    /// The classification the oracle prints for `image` (OTP 29.1.1 / erts-17.1).
    expected: &'static str,
}

/// A corpus entry whose image is short enough to paste as an Erlang `<<…>>` literal.
fn case(label: &'static str, image: Vec<u8>, expected: &'static str) -> Case {
    Case {
        erl: binary_text(&image),
        label,
        image,
        expected,
    }
}

/// The bound corpus, in the order the oracle prints its classifications.
fn corpus() -> Vec<Case> {
    let mut cases = Vec::new();

    // ---- LARGE_TUPLE_EXT (105) arity ----------------------------------------
    // An arity past `MAX_ARITYVAL` (`(1 << 24) - 1`, `erl_term.h:322`) is a bound.
    cases.push(case(
        "large_tuple arity 2^24",
        vec![VERSION_MAGIC, LARGE_TUPLE_EXT, 1, 0, 0, 0],
        "badarg",
    ));
    // `MAX_ARITYVAL` itself passes the bound and fails on the missing elements.
    cases.push(case(
        "large_tuple arity MAX_ARITYVAL truncated",
        vec![VERSION_MAGIC, LARGE_TUPLE_EXT, 0, 255, 255, 255],
        "badarg",
    ));
    // …and a tuple inside the bound still decodes.
    cases.push(case(
        "large_tuple arity 2",
        vec![VERSION_MAGIC, LARGE_TUPLE_EXT, 0, 0, 0, 2, 97, 1, 97, 2],
        "ok",
    ));

    // ---- LARGE_BIG_EXT (111) byte count -------------------------------------
    // 524281 bytes = `BIG_ARITY_MAX * 8 + 1`: one past the wire bound. The wire is
    // `[tag][count:4][sign][count bytes]`, so the sign byte is written explicitly and
    // the payload is exactly `count` bytes long.
    let over = BIG_BYTE_BOUND + 1;
    let mut image = vec![VERSION_MAGIC, LARGE_BIG_EXT];
    image.extend_from_slice(&(over as u32).to_be_bytes());
    image.push(0); // sign: positive
    image.resize(image.len() + over, 0);
    cases.push(Case {
        label: "large_big byte count 524281",
        image,
        erl: format!(
            "<<131,111,{},{},{},{},0,(binary:copy(<<0>>,{over}))/binary>>",
            (over >> 24) & 0xff,
            (over >> 16) & 0xff,
            (over >> 8) & 0xff,
            over & 0xff,
        ),
        expected: "badarg",
    });
    // The same over-long count with no payload at all (the image from the bug report).
    cases.push(case(
        "large_big byte count 524281 truncated",
        vec![VERSION_MAGIC, LARGE_BIG_EXT, 0, 7, 255, 249, 0],
        "badarg",
    ));
    // 524280 bytes is exactly at the bound, so it decodes (`0`, all-zero magnitude).
    let at_bound = BIG_BYTE_BOUND;
    let mut image = vec![VERSION_MAGIC, LARGE_BIG_EXT];
    image.extend_from_slice(&(at_bound as u32).to_be_bytes());
    image.push(0); // sign: positive
    image.resize(image.len() + at_bound, 0);
    cases.push(Case {
        label: "large_big byte count 524280",
        image,
        erl: format!(
            "<<131,111,{},{},{},{},0,(binary:copy(<<0>>,{at_bound}))/binary>>",
            (at_bound >> 24) & 0xff,
            (at_bound >> 16) & 0xff,
            (at_bound >> 8) & 0xff,
            at_bound & 0xff,
        ),
        expected: "ok",
    });

    // ---- NEWER_REFERENCE_EXT (90) id count ----------------------------------
    // Six ids is one past `ERTS_MAX_REF_NUMBERS` (5, `erl_term.h:780`).
    let mut image = vec![VERSION_MAGIC, NEWER_REFERENCE_EXT, 0, 6];
    image.extend_from_slice(&[119, 2, b'n', b'1', 0, 0, 0, 1]);
    for _ in 0..6 {
        image.extend_from_slice(&[0, 0, 0, 1]);
    }
    cases.push(Case {
        label: "newer_reference len 6",
        image,
        erl: "<<131,90,0,6,119,2,\"n1\",0,0,0,1,(binary:copy(<<0,0,0,1>>,6))/binary>>".to_string(),
        expected: "badarg",
    });
    // The same over-long count with the ids missing (the image from the bug report).
    cases.push(case(
        "newer_reference len 6 truncated",
        vec![
            VERSION_MAGIC,
            NEWER_REFERENCE_EXT,
            0,
            6,
            119,
            2,
            b'n',
            b'1',
            0,
            0,
            0,
            1,
        ],
        "badarg",
    ));
    // Five ids is exactly at the bound, so it decodes.
    let mut image = vec![VERSION_MAGIC, NEWER_REFERENCE_EXT, 0, 5];
    image.extend_from_slice(&[119, 2, b'n', b'1', 0, 0, 0, 1]);
    for _ in 0..5 {
        image.extend_from_slice(&[0, 0, 0, 1]);
    }
    cases.push(Case {
        label: "newer_reference len 5",
        image,
        erl: "<<131,90,0,5,119,2,\"n1\",0,0,0,1,(binary:copy(<<0,0,0,1>>,5))/binary>>".to_string(),
        expected: "ok",
    });

    // ---- RECORD_EXT (67) field count ----------------------------------------
    // `record_def_size_words(n) - 1 = n + 5` must fit the header's arity field, so a
    // count past `MAX_ARITYVAL - 5` is a bound.
    cases.push(case(
        "record field count 2^24",
        vec![
            VERSION_MAGIC,
            RECORD_EXT,
            1,
            0,
            0,
            0,
            0,
            119,
            1,
            b'm',
            119,
            1,
            b'r',
        ],
        "badarg",
    ));
    // A small record still decodes (`#m:r{a = 1, b = 2}`).
    cases.push(case(
        "record field count 2",
        vec![
            VERSION_MAGIC,
            RECORD_EXT,
            0,
            0,
            0,
            2,
            0,
            119,
            1,
            b'm',
            119,
            1,
            b'r',
            119,
            1,
            b'a',
            119,
            1,
            b'b',
            97,
            1,
            97,
            2,
        ],
        "ok",
    ));

    // ---- Lying length fields ------------------------------------------------
    // `STRING_EXT` with a 16-bit length and no bytes behind it.
    cases.push(case(
        "string len 65535 truncated",
        vec![VERSION_MAGIC, STRING_EXT, 255, 255],
        "badarg",
    ));
    // `LIST_EXT` with a 32-bit length and no elements behind it.
    cases.push(case(
        "list len 2^24 truncated",
        vec![VERSION_MAGIC, LIST_EXT, 1, 0, 0, 0],
        "badarg",
    ));

    cases
}

/// The classification the oracle prints for each [`corpus`] entry, in order.
///
/// Kept as a separate table so a change to one side cannot silently move the other;
/// the live cross-check re-derives it from a real `erl`.
fn pinned_classes() -> Vec<&'static str> {
    corpus().iter().map(|c| c.expected).collect()
}

// ---------------------------------------------------------------------------
// Our side of the comparison
// ---------------------------------------------------------------------------

/// Classifies one wire image the way the oracle's Erlang classifier does: `ok` for any
/// accepted term, `badarg` for `BeamError::BadArg`, or `err:<debug>` for any other
/// typed error (never expected here).
fn classify(image: &[u8]) -> String {
    let mut heap = new_heap(8192);
    let mut atoms = AtomTable::new();
    match decode_term(image, &mut heap, &mut atoms) {
        Ok(_) => "ok".to_string(),
        Err(BeamError::BadArg) => "badarg".to_string(),
        Err(err) => format!("err:{err:?}"),
    }
}

/// The `|`-joined classification of the whole corpus.
fn classify_corpus() -> String {
    corpus()
        .iter()
        .map(|c| classify(&c.image))
        .collect::<Vec<_>>()
        .join("|")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every bound is `badarg` — the oracle's answer for *every* decode failure — and never
/// `system_limit`, even with no `erl` installed.
#[test]
fn decode_bound_classes_match_the_pinned_oracle_classes() {
    let pinned = pinned_classes();
    let cases = corpus();
    assert_eq!(
        cases.len(),
        pinned.len(),
        "the corpus and the pinned classification must stay in step"
    );
    for (c, want) in cases.iter().zip(pinned.iter()) {
        let got = classify(&c.image);
        assert_eq!(got, *want, "case `{}`", c.label);
    }
}

/// The length/arity bounds report `BadArg` directly — asserted per case, independent of
/// the classifier, so a regression to `SystemLimit` (the pre-fix behaviour, and the
/// observable `error:system_limit` divergence against the oracle) fails loudly.
#[test]
fn decode_bounds_report_bad_arg_not_system_limit() {
    // Only the over-bound (and lying-length) entries: the in-bound twins must decode.
    for c in corpus().iter().filter(|c| c.expected == "badarg") {
        let mut heap = new_heap(8192);
        let mut atoms = AtomTable::new();
        let err = decode_term(&c.image, &mut heap, &mut atoms)
            .expect_err(&format!("`{}` must be rejected", c.label));
        assert!(
            matches!(err, BeamError::BadArg),
            "case `{}` must be BadArg (the oracle's answer for every decode failure), \
             got {err:?}",
            c.label
        );
    }
}

/// The live cross-check: the oracle runs the *same* classification over the *same*
/// byte vectors and the two strings must be identical. Skips loudly when no `erl`
/// exists.
#[test]
fn decode_bound_classes_match_the_system_oracle() {
    let Some(oracle) = oracle_or_skip("decode_bound_classes_match_the_system_oracle") else {
        return;
    };

    let images = corpus()
        .iter()
        .map(|c| c.erl.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let eval = format!(
        "C = fun(B) -> try binary_to_term(B) of \
           _ -> \"ok\" \
         catch error:badarg -> \"badarg\"; \
           Cls:Rs -> \"error:\" ++ atom_to_list(Cls) ++ \":\" ++ \
             lists:flatten(io_lib:format(\"~p\", [Rs])) end end, \
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
            "[beam_codec decode_bound_oracle] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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
            eprintln!("============ beam_codec decode_bound_oracle: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("=========================================================================");
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

/// The `<<131,97,…>>` textual form `io:format("~w", [Bin])` prints for `bytes`.
fn binary_text(bytes: &[u8]) -> String {
    let joined = bytes
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("<<{joined}>>")
}
