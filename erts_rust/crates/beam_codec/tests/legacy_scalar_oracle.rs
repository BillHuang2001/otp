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

//! Live oracle cross-checks for the two legacy/scalar decode rules that carry the
//! subtlest wire detail:
//!
//! 1. **`FLOAT_EXT` (`99`) text grammar.** The payload is 31 bytes of a
//!    NUL-terminated `%.20e` rendering, but the C *decoder* does not simply
//!    `strtod` it: `sys_chars_to_double`
//!    (`../../erts/emulator/sys/unix/sys_float.c:72-140`) accepts exactly
//!    `[+-]? DIGIT+ [.,] DIGIT+ ([eE] [+-]? DIGIT+)?`, requires a finite result,
//!    and rejects everything else (`inf`, `nan`, `1e5`, `.5`, `1.`, hex floats, …).
//!    Rust's `parse::<f64>()` is far more permissive, so this crate validates the
//!    grammar byte by byte before parsing. [`legacy_float_text_grammar_matches_the_oracle`]
//!    pins the accepted/rejected split both directly and against the live oracle.
//!
//! 2. **A `NEW_FUN_EXT` creator pid in the legacy `PID_EXT` (`103`) form.** The C
//!    routes that field through `dec_pid` (`external.c:5153-5160`), so its 8-bit
//!    creation must be *tiny* (`is_tiny_creation`, `external.c:71-72`) — a value
//!    above 3 is `badarg`.
//!    [`new_fun_legacy_pid_creator_matches_the_oracle`] pins that boundary.
//!
//! The oracle is the system `erl` (`$ERTS_ORACLE` if it is an executable, else
//! `erl` on `$PATH`). Without one the oracle half of each test **skips loudly to
//! stderr** — never a silent pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_common::BeamError;

use beam_codec::term::float_value;
use beam_codec::{
    decode_term, FLOAT_EXT, NEW_FUN_EXT, PID_EXT, SMALL_ATOM_UTF8_EXT, SMALL_INTEGER_EXT,
    VERSION_MAGIC,
};

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

/// Resolve the oracle the same way `tools/differential` does: `$ERTS_ORACLE` if it is
/// an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec legacy_scalar_oracle] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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
            eprintln!("=============== beam_codec legacy_scalar_oracle: {test} SKIPPED ===============");
            eprintln!("No usable system `erl` oracle found — the oracle cross-check was NOT run");
            eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
            eprintln!("This is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("===============================================================================");
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

/// Renders an image as an Erlang binary literal `<<131,...>>`.
fn erl_binary(image: &[u8]) -> String {
    let body = image
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("<<{body}>>")
}

/// Asks the oracle to classify each image as one space-separated token.
///
/// * `F:<bits>` — `binary_to_term/1` decoded a float; `<bits>` is its IEEE-754 bit
///   pattern (`binary:decode_unsigned(<<X:64/float>>)`), so a value comparison is a
///   `to_bits()` comparison.
/// * `T` — the image decoded to some non-float term (e.g. a fun).
/// * `B` — `binary_to_term/1` raised `badarg`.
///
/// The classifier uses `catch` rather than `try`: `-eval` runs on `init`'s
/// restricted `erl_eval` context, which does not evaluate `try` expressions (a
/// `try` in the eval aborts the VM with no output).
fn oracle_tokens(oracle: &Path, images: &[Vec<u8>]) -> Vec<String> {
    let list = images
        .iter()
        .map(|image| erl_binary(image))
        .collect::<Vec<_>>()
        .join(",");
    let eval = format!(
        "F = fun(B) -> \
             case catch binary_to_term(B) of \
               {{'EXIT', _}} -> \"B\"; \
               X when is_float(X) -> \
                 [\"F:\", integer_to_list(binary:decode_unsigned(<<X:64/float>>))]; \
               _ -> \"T\" \
             end \
           end, \
         io:format(\"~s~n\", [lists:flatten(lists:join(\" \", [F(B) || B <- [{list}]]))]), \
         ok."
    );
    let out = run_erl(oracle, &eval);
    let tokens: Vec<String> = out.split_whitespace().map(str::to_owned).collect();
    assert_eq!(
        tokens.len(),
        images.len(),
        "the oracle printed {} tokens for {} images: {out:?}",
        tokens.len(),
        images.len()
    );
    tokens
}

// ---------------------------------------------------------------------------
// Our own classification (the same shape the oracle tokens use)
// ---------------------------------------------------------------------------

/// How this crate's decoder classifies an image — the mirror of the oracle tokens.
#[derive(Debug, PartialEq, Eq)]
enum Class {
    /// Decoded to a float, carrying its IEEE-754 bit pattern.
    Float(u64),
    /// Decoded to a non-float term.
    Other,
    /// `BeamError::BadArg`.
    BadArg,
}

impl Class {
    fn token(&self) -> String {
        match self {
            Class::Float(bits) => format!("F:{bits}"),
            Class::Other => "T".to_owned(),
            Class::BadArg => "B".to_owned(),
        }
    }
}

/// Decodes `image` with this crate and classifies the result.
fn our_class(image: &[u8]) -> Class {
    let mut h = Heap::new(512).expect("the carrier allocator never fails for this size");
    let mut a = beam_term::AtomTable::new();
    match decode_term(image, &mut h, &mut a) {
        Ok(term) => match float_value(term) {
            Ok(value) => Class::Float(value.to_bits()),
            Err(_) => Class::Other,
        },
        Err(BeamError::BadArg) => Class::BadArg,
        Err(e) => panic!("unexpected decode error for {image:?}: {e:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. `FLOAT_EXT` (`99`) text grammar
// ---------------------------------------------------------------------------

/// The image for a legacy float whose 31-byte field begins with `text` and is
/// NUL-padded to the full 31 bytes (which is what the C writer emits).
fn float_ext_image(text: &str) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, FLOAT_EXT];
    image.extend_from_slice(text.as_bytes());
    assert!(image.len() <= 2 + 31, "the text field is 31 bytes");
    image.resize(2 + 31, 0);
    image
}

/// The corpus: `Some(value)` for a text the grammar accepts (the value it must
/// decode to), `None` for a text the oracle rejects with `badarg`.
fn float_corpus() -> Vec<(&'static str, Option<f64>)> {
    vec![
        // Accepted: `[+-]? DIGIT+ [.,] DIGIT+ ([eE] [+-]? DIGIT+)?`.
        ("3.14", Some(3.14)),
        ("3.14000000000000000000e+00", Some(3.14)),
        ("1.0e5", Some(1.0e5)),
        ("1.0E+05", Some(1.0e5)),
        ("+1.0", Some(1.0)),
        ("-0.0", Some(-0.0)),
        ("1,5", Some(1.5)), // the C retries `strtod` with the flipped radix character
        ("3,14", Some(3.14)),
        // Rejected: `inf`/`nan` spellings, no radix, a missing integer or
        // fractional part, whitespace, hex floats, and trailing junk.
        ("inf", None),
        ("-inf", None),
        ("nan", None),
        ("infinity", None),
        ("1e5", None),
        ("1.", None),
        (".5", None),
        (" 1.0", None),
        ("0x1p3", None),
        ("1", None),
        ("+", None),
        ("1.0e", None),
        ("1.5x", None),
        ("", None),
    ]
}

#[test]
fn legacy_float_text_grammar_matches_the_oracle() {
    let corpus = float_corpus();
    let images: Vec<Vec<u8>> = corpus.iter().map(|(text, _)| float_ext_image(text)).collect();

    // Direct check first: our classification matches the documented grammar, so the
    // rule is pinned even without an oracle.
    for ((text, expected), image) in corpus.iter().zip(&images) {
        let want = match expected {
            Some(value) => Class::Float(value.to_bits()),
            None => Class::BadArg,
        };
        assert_eq!(
            our_class(image),
            want,
            "FLOAT_EXT {text:?} misclassified"
        );
    }

    let Some(oracle) = oracle_or_skip("legacy_float_text_grammar_matches_the_oracle") else {
        return;
    };

    // Live cross-check: the oracle agrees with us on every image, token for token.
    let oracle = oracle_tokens(&oracle, &images);
    for ((text, _), (ours, theirs)) in corpus
        .iter()
        .zip(images.iter().map(|image| our_class(image)).zip(&oracle))
    {
        assert_eq!(
            &ours.token(),
            theirs,
            "FLOAT_EXT {text:?}: ours {:?} vs the oracle {theirs:?}",
            ours
        );
    }
}

/// A valid legacy `FLOAT_EXT` decodes to the same value as the oracle, and
/// re-encodes as the modern `NEW_FLOAT_EXT` rather than the legacy tag.
#[test]
fn valid_legacy_float_matches_the_oracle() {
    let image = float_ext_image("2.50000000000000000000e+00");

    let mut h = Heap::new(64).expect("heap");
    let mut a = beam_term::AtomTable::new();
    let term = decode_term(&image, &mut h, &mut a).expect("a well-formed FLOAT_EXT decodes");
    assert_eq!(float_value(term).unwrap().to_bits(), 2.5f64.to_bits());

    let Some(oracle) = oracle_or_skip("valid_legacy_float_matches_the_oracle") else {
        return;
    };
    assert_eq!(
        oracle_tokens(&oracle, &[image]),
        vec![format!("F:{}", 2.5f64.to_bits())],
        "the oracle must decode the same legacy float"
    );
}

// ---------------------------------------------------------------------------
// 2. `NEW_FUN_EXT` creator pid with the legacy `PID_EXT` (`103`) form
// ---------------------------------------------------------------------------

/// A `NEW_FUN_EXT` of arity 0 / `num_free` 0 whose creator is a legacy `PID_EXT` with
/// the given 8-bit creation byte.
///
/// Layout: `[size:4][arity:1][uniq:16][index:4][num_free:4][module atom]
/// [old_index term][old_uniq term][creator pid term][env…]` (`external.c:5102-5176`).
fn new_fun_with_legacy_pid_creator(creation: u8) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(0); // arity 0
    payload.extend_from_slice(&[0u8; 16]); // 16-byte `uniq`
    payload.extend_from_slice(&0u32.to_be_bytes()); // index
    payload.extend_from_slice(&0u32.to_be_bytes()); // num_free
    payload.extend_from_slice(&[SMALL_ATOM_UTF8_EXT, 1, b'm']); // module atom
    payload.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_index (a small)
    payload.extend_from_slice(&[SMALL_INTEGER_EXT, 0]); // old_uniq (a small)
    // Creator: the legacy `PID_EXT` form, `[node][id:4][serial:4][creation:1]`.
    payload.push(PID_EXT);
    payload.extend_from_slice(&[SMALL_ATOM_UTF8_EXT, 1, b'n']); // node atom
    payload.extend_from_slice(&0u32.to_be_bytes()); // id
    payload.extend_from_slice(&0u32.to_be_bytes()); // serial
    payload.push(creation);

    let mut image = vec![VERSION_MAGIC, NEW_FUN_EXT];
    image.extend_from_slice(&((payload.len() as u32) + 4).to_be_bytes());
    image.extend_from_slice(&payload);
    image
}

#[test]
fn new_fun_legacy_pid_creator_matches_the_oracle() {
    // A creation ≤ 3 is tiny and accepted; anything above is `badarg`
    // (`is_tiny_creation`, `external.c:71-72`, reached through `dec_pid`).
    let corpus: &[(u8, &str)] = &[(0, "T"), (1, "T"), (3, "T"), (4, "B"), (5, "B"), (255, "B")];
    let images: Vec<Vec<u8>> = corpus
        .iter()
        .map(|(creation, _)| new_fun_with_legacy_pid_creator(*creation))
        .collect();

    for ((creation, expected), image) in corpus.iter().zip(&images) {
        assert_eq!(
            our_class(image).token(),
            *expected,
            "NEW_FUN_EXT with creator creation {creation} misclassified"
        );
    }

    let Some(oracle) = oracle_or_skip("new_fun_legacy_pid_creator_matches_the_oracle") else {
        return;
    };

    let oracle = oracle_tokens(&oracle, &images);
    for ((creation, _), (ours, theirs)) in corpus
        .iter()
        .zip(images.iter().map(|image| our_class(image)).zip(&oracle))
    {
        assert_eq!(
            &ours.token(),
            theirs,
            "NEW_FUN_EXT creator creation {creation}: ours {:?} vs the oracle {theirs:?}",
            ours
        );
    }
}
