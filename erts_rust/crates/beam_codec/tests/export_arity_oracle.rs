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

//! `EXPORT_EXT` (`113`) **arity is truncated to 32 bits**, pinned against the
//! system Erlang oracle.
//!
//! The C reads an external fun's arity as a 64-bit `Sint`
//! (`Sint arity = signed_val(temp);`, `external.c:5028`) but passes it to
//! `erts_export_get_or_make_stub(Eterm, Eterm, unsigned)` (`export.h:137`), so the
//! value is truncated to its **low 32 bits** before it is stored in
//! `ErtsCodeMFA.arity`, a `Uint` (`code_ix.h:89-97`) — the same 32-bit width as
//! `ErlFunEntry.arity` (`erl_fun.h:43`). The ETF encoder then reads those 32 bits
//! back out of the export entry (`external.c:4000`/`:5912`) and re-emits them.
//!
//! So a 60-bit small arity does **not** survive the round trip: only its low 32 bits
//! do. Verified against erts-17.1 (OTP 29.1.1) — an `EXPORT_EXT` for
//! `fun lists:reverse/N`, whose arity is the `SMALL_BIG_EXT` image of `N`:
//!
//! | wire arity `N` | decoded (oracle) | re-encoded (oracle) |
//! |---|---|---|
//! | `2^32` | `0` | `SMALL_INTEGER_EXT` `0` |
//! | `2^32 + 7` | `7` | `SMALL_INTEGER_EXT` `7` |
//! | `2^40 + 1` | `1` | `SMALL_INTEGER_EXT` `1` |
//! | `2^32 - 1` | `0xFFFF_FFFF` | `SMALL_BIG_EXT` `255,255,255,255` |
//! | `3 * 2^31` | `0x8000_0000` | `SMALL_BIG_EXT` `0,0,0,128` |
//! | `2^31` | `0x8000_0000` | `SMALL_BIG_EXT` `0,0,0,128` |
//! | a small arity (e.g. `5`) | unchanged | unchanged |
//!
//! The 8-bit fun-header mask is a *separate* rule (`fun_info(F, arity)` reports the
//! truncated value `& 0xFF`), covered by `tests/ref_ext_zero_and_arity.rs`.
//!
//! ## Oracle resolution
//!
//! `$ERTS_ORACLE` if it names an existing executable, otherwise `erl` on `$PATH`
//! (the same resolution as `tests/interop.rs`, `tests/ref_ext_zero_and_arity.rs` and
//! `tools/differential`). With no oracle the oracle-dependent test prints a **loud**
//! stderr note and `return` — a skip, never a silent pass. The pure (oracle-free)
//! tests below always run.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::term::{fun_arity_term, fun_kind_term, fun_true_arity_term, FunKind};
use beam_codec::{decode_term, encode_term, EXPORT_EXT, VERSION_MAGIC};
use beam_term::AtomTable;

// ---------------------------------------------------------------------------
// Wire images
// ---------------------------------------------------------------------------

/// The module/function atoms every image here writes (`fun lists:reverse/N`) in the
/// `SMALL_ATOM_UTF8_EXT` form the C's own encoder emits, so the oracle round-trips
/// them unchanged.
const MFA: [u8; 16] = [
    119, 5, b'l', b'i', b's', b't', b's', 119, 7, b'r', b'e', b'v', b'e', b'r', b's', b'e',
];

/// The arity term as the C's `term_to_binary/1` writes it: `SMALL_INTEGER_EXT` below
/// 256, `INTEGER_EXT` when it fits a signed 32-bit int, else `SMALL_BIG_EXT` with the
/// significant little-endian magnitude bytes (at least one).
fn arity_term(value: u64) -> Vec<u8> {
    if value < 256 {
        return vec![97, value as u8];
    }
    if value <= 0x7FFF_FFFF {
        let mut out = vec![98]; // INTEGER_EXT
        out.extend_from_slice(&(value as u32).to_be_bytes());
        return out;
    }
    let mut magnitude = value.to_le_bytes().to_vec();
    while magnitude.len() > 1 && magnitude[magnitude.len() - 1] == 0 {
        magnitude.pop();
    }
    let mut out = vec![110, magnitude.len() as u8, 0]; // SMALL_BIG_EXT, sign 0
    out.extend_from_slice(&magnitude);
    out
}

/// `EXPORT_EXT` (`113`) `[113][module atom][function atom][arity term]` for
/// `fun lists:reverse/<value>`.
fn export_image(value: u64) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, EXPORT_EXT];
    image.extend_from_slice(&MFA);
    image.extend_from_slice(&arity_term(value));
    image
}

/// The byte-exact re-encode the oracle produces for an `EXPORT_EXT` whose arity
/// truncates to `truncated`.
fn expected_reencode(truncated: u64) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, EXPORT_EXT];
    image.extend_from_slice(&MFA);
    image.extend_from_slice(&arity_term(truncated));
    image
}

// ---------------------------------------------------------------------------
// Our side: decode / re-encode helpers
// ---------------------------------------------------------------------------

/// Decodes `image` and returns `(masked 8-bit arity, true arity, re-encoded bytes)`.
fn our_view(image: &[u8]) -> (usize, usize, Vec<u8>) {
    let mut heap = Heap::new(64).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    let term = decode_term(image, &mut heap, &mut atoms)
        .unwrap_or_else(|err| panic!("expected {image:?} to decode, got {err:?}"));
    assert_eq!(
        fun_kind_term(term).expect("a fun"),
        FunKind::External,
        "an `EXPORT_EXT` decodes to an external fun"
    );
    let masked = fun_arity_term(term).expect("a masked arity");
    let true_arity = fun_true_arity_term(term).expect("a true arity");
    let bytes = encode_term(term, &atoms).expect("the term re-encodes");
    (masked, true_arity, bytes)
}

// ---------------------------------------------------------------------------
// Oracle resolution and process plumbing
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

/// Resolve the oracle the same way `tools/differential` does: `$ERTS_ORACLE` if it
/// is an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec export_arity_oracle] $ERTS_ORACLE='{}' is not an existing \
             executable — treating it as no oracle.",
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
            eprintln!("============ beam_codec export_arity_oracle: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found.");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP)");
            eprintln!("and re-run.");
            eprintln!("========================================================================");
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

/// An Erlang binary literal for `image`.
fn erl_binary(image: &[u8]) -> String {
    let bytes = image.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("<<{bytes}>>")
}

/// Runs the oracle **once** over `images`, evaluating the Erlang expression `expr`
/// (over the variable `I`, expected to yield an iolist) per image and joining the
/// results by newlines.
fn oracle_lines(oracle: &Path, images: &[Vec<u8>], expr: &str) -> Vec<String> {
    let corpus = images
        .iter()
        .map(|image| erl_binary(image))
        .collect::<Vec<_>>()
        .join(",\n    ");
    let eval = format!(
        "F = fun(I) -> {expr} end,\n\
         Imgs = [{corpus}],\n\
         io:format(\"~s~n\", [lists:join(<<10>>, [iolist_to_binary(F(I)) || I <- Imgs])]),\n\
         ok."
    );
    let stdout = run_erl(oracle, &eval);
    let lines = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        images.len(),
        "the oracle printed {} lines for {} images:\n{stdout}",
        lines.len(),
        images.len()
    );
    lines
}

/// The expression `ok <fun_info arity> <re-encoded bytes>` / `badarg`.
const EXPORT_EXPR: &str = "try binary_to_term(I) of F -> io_lib:format(\"ok ~w ~w\", \
                           [element(2, erlang:fun_info(F, arity)), \
                            binary:bin_to_list(term_to_binary(F))]); \
                           _ -> \"unexpected\" catch error:badarg -> \"badarg\"; \
                           C:R -> io_lib:format(\"err ~w:~w\", [C, R]) end";

/// Parses the first `[a,b,c]` integer list out of `text` into bytes.
fn parse_int_list(text: &str) -> Vec<u8> {
    let start = text
        .find('[')
        .unwrap_or_else(|| panic!("no list in {text:?}"));
    let end = text
        .rfind(']')
        .unwrap_or_else(|| panic!("no list end in {text:?}"));
    text[start + 1..end]
        .split(',')
        .map(|item| {
            item.trim()
                .parse::<u8>()
                .unwrap_or_else(|e| panic!("bad byte {item:?} in {text:?}: {e}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Pure tests (no oracle needed)
// ---------------------------------------------------------------------------

/// The truncation is the C's: the 64-bit `Sint` is narrowed to the 32-bit `unsigned`
/// field, so the arity that survives the round trip is `value & 0xFFFF_FFFF`.
///
/// The corpus pins both directions of the rule — a value below `2^32` is *untouched*
/// (so there is no accidental over-truncation), and a value at/above `2^32` keeps
/// exactly its low 32 bits.
#[test]
fn export_ext_arity_truncates_to_the_low_32_bits() {
    let cases: [(u64, u64); 10] = [
        (0, 0),
        (1, 1),
        (5, 5),
        (255, 255),
        (256, 256),                     // INTEGER_EXT, unchanged
        (2_147_483_647, 0x7FFF_FFFF),   // i32::MAX, unchanged
        (2_147_483_648, 0x8000_0000),   // 2^31: DOES NOT fit a signed i32, yet survives
        (0xFFFF_FFFF, 0xFFFF_FFFF),     // 2^32 - 1, unchanged
        (1 << 32, 0),                   // 2^32 wraps to 0
        ((1 << 32) + 7, 7),             // 2^32 + 7 wraps to 7
    ];
    for (wire, truncated) in cases {
        let image = export_image(wire);
        let (masked, true_arity, bytes) = our_view(&image);
        assert_eq!(
            true_arity, truncated as usize,
            "wire arity {wire} must truncate to {truncated}"
        );
        assert_eq!(
            masked,
            (truncated & 0xFF) as usize,
            "the 8-bit header mask of arity {wire} (truncated {truncated}) is wrong"
        );
        assert_eq!(
            bytes,
            expected_reencode(truncated),
            "EXPORT_EXT arity {wire} must re-encode as the truncated {truncated}"
        );
    }
    // `2^40 + 1` keeps only its low 32 bits (which is 1).
    let image = export_image((1 << 40) + 1);
    assert_eq!(our_view(&image).1, 1, "2^40 + 1 truncates to 1");
    assert_eq!(our_view(&image).2, expected_reencode(1));
    // `3 * 2^31` keeps `0x8000_0000`.
    let image = export_image(3 << 31);
    assert_eq!(our_view(&image).1, 0x8000_0000, "3 * 2^31 truncates to 2^31");
    assert_eq!(our_view(&image).2, expected_reencode(0x8000_0000));
}

/// The exact bug report vector: a `SMALL_BIG_EXT` arity of `2^32`
/// (`110,5,0,0,0,0,0,1`) decodes to arity 0 and re-encodes to the 19-byte
/// `SMALL_INTEGER_EXT` image the oracle emits.
#[test]
fn export_ext_arity_2_pow_32_reencodes_byte_exactly() {
    let image = vec![
        VERSION_MAGIC, EXPORT_EXT, 119, 5, b'l', b'i', b's', b't', b's', 119, 7, b'r', b'e',
        b'v', b'e', b'r', b's', b'e', 110, 5, 0, 0, 0, 0, 0, 1,
    ];
    let (masked, true_arity, bytes) = our_view(&image);
    assert_eq!(true_arity, 0, "2^32 truncates to 0");
    assert_eq!(masked, 0);
    assert_eq!(
        bytes,
        vec![
            VERSION_MAGIC, EXPORT_EXT, 119, 5, b'l', b'i', b's', b't', b's', 119, 7, b'r',
            b'e', b'v', b'e', b'r', b's', b'e', 97, 0,
        ],
        "2^32 must re-encode as the 19-byte `97,0` image the oracle emits"
    );
}

/// The API surface a decoder consumer relies on (`Eterm`, `Heap`) stays untouched by
/// the truncation: decoding is still total on a well-formed image.
#[test]
fn export_ext_small_arity_is_unchanged() {
    let image = export_image(3);
    let (masked, true_arity, bytes) = our_view(&image);
    assert_eq!((masked, true_arity), (3, 3));
    assert_eq!(bytes, image, "a small arity round-trips byte-exactly");
}

// ---------------------------------------------------------------------------
// Oracle cross-check
// ---------------------------------------------------------------------------

/// Our decode -> encode of an `EXPORT_EXT` with an arity at/above `2^32` agrees with
/// the oracle byte for byte, and our header mask agrees with `fun_info(F, arity)`.
#[test]
fn export_ext_arity_matches_the_oracle_at_the_32_bit_boundary() {
    let Some(oracle) = oracle_or_skip("export_ext_arity_matches_the_oracle_at_the_32_bit_boundary")
    else {
        return;
    };
    // A small arity (unchanged), the two `i32`/`u32` boundaries, and three values that
    // only survive as their low 32 bits.
    let arities: [u64; 8] = [
        5,
        256,
        1 << 31,
        0xFFFF_FFFF,
        1 << 32,
        (1 << 32) + 7,
        (1 << 40) + 1,
        3 << 31,
    ];
    let images = arities.iter().map(|arity| export_image(*arity)).collect::<Vec<_>>();
    let lines = oracle_lines(&oracle, &images, EXPORT_EXPR);

    for (arity, (image, line)) in arities.iter().zip(images.iter().zip(&lines)) {
        assert_ne!(*line, "badarg", "the oracle accepts arity {arity} (line {line:?})");
        let rest = line
            .strip_prefix("ok ")
            .unwrap_or_else(|| panic!("expected an `ok` oracle line, got {line:?}"));
        let (oracle_masked, oracle_bytes) = rest
            .split_once(' ')
            .unwrap_or_else(|| panic!("bad oracle line {line:?}"));
        let oracle_masked = oracle_masked
            .parse::<usize>()
            .unwrap_or_else(|e| panic!("bad arity in the oracle line {line:?}: {e}"));
        let theirs = parse_int_list(oracle_bytes);

        let (masked, true_arity, ours) = our_view(image);
        assert_eq!(
            ours, theirs,
            "EXPORT_EXT arity {arity}: our re-encode must match the oracle's byte \
             for byte (ours {ours:?}, theirs {theirs:?})"
        );
        assert_eq!(
            masked, oracle_masked,
            "EXPORT_EXT arity {arity}: our 8-bit header mask must match \
             `fun_info(F, arity)`"
        );
        // The true arity is the truncated value, which is what the re-encode carries.
        assert_eq!(
            true_arity,
            (arity & 0xFFFF_FFFF) as usize,
            "EXPORT_EXT arity {arity} must truncate to its low 32 bits"
        );
        assert_eq!(ours, expected_reencode(arity & 0xFFFF_FFFF));
    }
}
