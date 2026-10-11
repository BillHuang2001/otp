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

//! **The `binary_to_term/2` `[used]` count for `COMPRESSED` and plain images.**
//!
//! This crate exposes the consumed source-byte count the distribution reader needs:
//! [`beam_codec::decode_term_used`] / [`beam_codec::decode_term_with_local_node_used`],
//! plus the public predicate [`beam_codec::is_compressed_image`] and the zlib-level
//! helper [`beam_codec::compress::decompress_bounded_used`]. They report how many
//! bytes of a possibly-over-long input the term occupied — the C `binary_to_term/2`
//! `used` count (`external.c:1797-1801`).
//!
//! The property that matters is that the count **stops at the term**: trailing bytes
//! after a plain image and after a `COMPRESSED` image are neither read nor counted
//! (for a compressed image the count is `6 + <zlib stream length>`, the stream ending
//! at its Adler-32 trailer). This is exactly why `is_compressed_image` and the
//! consumed-count API are public and why `beam_bifs::binconv`'s old
//! re-inflate-and-binary-search workaround (`consumed_compressed_len`) is no longer
//! needed.
//!
//! Every oracle-dependent test resolves the oracle the same way [`tools/differential`]
//! does (`$ERTS_ORACLE` if it is an existing executable, else `erl` on `$PATH`) and
//! skips **loudly** — never a silent pass — when no oracle is available. The
//! oracle-free test below pins the same invariant using our own encoder.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_term::prelude::{make_atom, AtomTable, Eterm, NIL};

use beam_codec::{
    decode_term_used, decode_term_with_local_node_used, encode_term, encode_term_compressed,
    is_compressed_image,
};

// ---------------------------------------------------------------------------
// Oracle plumbing (same resolution policy as tools/differential / compressed_oracle.rs)
// ---------------------------------------------------------------------------

/// Resolve the oracle: `$ERTS_ORACLE` if it is an existing executable, else `erl` on
/// `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec compressed_used] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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

/// Returns the oracle, or prints a loud skip note and returns `None` (never a silent
/// pass).
fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("=============== beam_codec compressed_used: {test} SKIPPED ===============");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("==========================================================================");
            eprintln!();
            None
        }
    }
}

/// Whether `path` is an existing executable file.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Runs `erl -noshell -eval <eval> -s init stop` and returns its stdout.
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

/// The non-empty stdout lines of an oracle run.
fn oracle_lines(oracle: &Path, eval: &str) -> Vec<String> {
    run_erl(oracle, eval)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parses an Erlang binary rendering — `~w` prints a binary as `"<<131,80,0,…>>"` —
/// into bytes.
fn parse_byte_list(s: &str) -> Vec<u8> {
    let t = s.trim();
    let inner = t
        .strip_prefix("<<")
        .and_then(|x| x.strip_suffix(">>"))
        .or_else(|| t.strip_prefix('[').and_then(|x| x.strip_suffix(']')))
        .unwrap_or_else(|| panic!("not an Erlang binary/byte list: {s:?}"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|p| {
            p.trim()
                .parse::<u8>()
                .unwrap_or_else(|_| panic!("bad byte {p:?} in {s:?}"))
        })
        .collect()
}

fn heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

/// Trailing bytes that follow the image; long enough that a re-inflating binary search
/// over the whole input would be a real (multi-step) search.
const JUNK: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

/// `C` (compressed) and `P` (plain) images of `lists:duplicate(500,hello)` from the
/// oracle, plus the `used` count it reports for each under trailing bytes.
struct OracleImages {
    compressed: Vec<u8>,
    plain: Vec<u8>,
    compressed_used: usize,
    plain_used: usize,
}

fn fetch_oracle_images(oracle: &Path) -> OracleImages {
    // One run prints, one `io:format` per line: the compressed image, the `used` count
    // for `<<C,JUNK>>`, the plain image, the `used` count for `<<P,JUNK>>`.
    let eval = format!(
        "T = lists:duplicate(500, hello), \
         C = term_to_binary(T, [compressed]), \
         P = term_to_binary(T), \
         J = <<{}>>, \
         {{_, UC}} = binary_to_term(<<C/binary, J/binary>>, [used]), \
         {{_, UP}} = binary_to_term(<<P/binary, J/binary>>, [used]), \
         io:format(\"~w~n\", [C]), \
         io:format(\"~w~n\", [UC]), \
         io:format(\"~w~n\", [P]), \
         io:format(\"~w~n\", [UP])",
        JUNK.iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    let lines = oracle_lines(oracle, &eval);
    assert_eq!(lines.len(), 4, "unexpected oracle output: {lines:?}");
    let compressed = parse_byte_list(&lines[0]);
    let compressed_used: usize = lines[1].parse().expect("a byte count");
    let plain = parse_byte_list(&lines[2]);
    let plain_used: usize = lines[3].parse().expect("a byte count");
    OracleImages {
        compressed,
        plain,
        compressed_used,
        plain_used,
    }
}

/// The oracle really did compress the term (so the test exercises the compressed
/// path), and, its own `[used]` count stops exactly at the image end.
#[test]
fn oracle_used_stops_at_the_image_for_compressed_and_plain() {
    let Some(oracle) = oracle_or_skip("oracle_used_stops_at_the_image_for_compressed_and_plain")
    else {
        return;
    };
    let images = fetch_oracle_images(&oracle);

    assert!(
        is_compressed_image(&images.compressed),
        "the oracle reverted to a plain image — the compressed case would be vacuous: {:?}",
        &images.compressed[..images.compressed.len().min(8)]
    );
    assert!(
        !is_compressed_image(&images.plain),
        "the plain image must not look compressed"
    );
    assert!(
        images.compressed.len() < images.plain.len(),
        "the compressed stream ({} bytes) must be shorter than the plain image ({} bytes)",
        images.compressed.len(),
        images.plain.len()
    );

    assert_eq!(
        images.compressed_used,
        images.compressed.len(),
        "the oracle's `used` for a compressed image under trailing junk must be byte_size(C)"
    );
    assert_eq!(
        images.plain_used,
        images.plain.len(),
        "the oracle's `used` for a plain image under trailing junk must be byte_size(P)"
    );
}

/// Our `decode_term_used` agrees with the oracle: the count stops at the image end
/// (NOT at the longer input length) both for a compressed and for a plain image, and
/// the decoded term re-encodes to the oracle's plain image.
#[test]
fn decode_term_used_matches_the_oracle_for_compressed_and_plain() {
    let Some(oracle) =
        oracle_or_skip("decode_term_used_matches_the_oracle_for_compressed_and_plain")
    else {
        return;
    };
    let oracle = fetch_oracle_images(&oracle);

    for (label, image, oracle_used) in [
        (
            "compressed",
            &oracle.compressed,
            oracle.compressed_used,
        ),
        ("plain", &oracle.plain, oracle.plain_used),
    ] {
        // The input offers the image followed by trailing junk; the term occupies only
        // the image.
        let mut input = image.clone();
        input.extend_from_slice(&JUNK);
        assert!(input.len() > image.len());

        let mut h = heap(1 << 16);
        let mut atoms = AtomTable::new();
        let (term, consumed) = decode_term_used(&input, &mut h, &mut atoms)
            .unwrap_or_else(|e| panic!("{label}: decode_term_used failed: {e:?}"));

        assert_eq!(
            consumed,
            oracle_used,
            "{label}: our consumed count must equal the oracle's `used`"
        );
        assert_eq!(
            consumed,
            image.len(),
            "{label}: the junk after the image must not be counted"
        );

        // The decoded term is the oracle's term: re-encoding gives its plain image.
        let reencoded = encode_term(term, &atoms).expect("re-encoding a decoded term");
        assert_eq!(
            reencoded, oracle.plain,
            "{label}: decode must reproduce the oracle's term"
        );
    }
}

/// The identity-carrying `used` entry point reports the same count on a compressed
/// image (the identity does not change how many source bytes the image occupies).
#[test]
fn decode_term_with_local_node_used_matches_on_compressed() {
    let Some(oracle) = oracle_or_skip("decode_term_with_local_node_used_matches_on_compressed")
    else {
        return;
    };
    let oracle = fetch_oracle_images(&oracle);
    assert!(is_compressed_image(&oracle.compressed));

    let mut input = oracle.compressed.clone();
    input.extend_from_slice(&JUNK);

    let mut h = heap(1 << 16);
    let mut atoms = AtomTable::new();
    // A node identity that never matches this term's contents (it has no identifiers),
    // so the decode is identity-inert but goes through the identity-aware entry point.
    let node = atoms.intern("nonode@nohost");
    let (term, consumed) =
        decode_term_with_local_node_used(&input, &mut h, &mut atoms, node, 0, None)
            .unwrap_or_else(|e| panic!("decode_term_with_local_node_used failed: {e:?}"));

    assert_eq!(
        consumed,
        oracle.compressed.len(),
        "the identity path must report the same consumed count as the plain `used` path"
    );
    let reencoded = encode_term(term, &atoms).expect("re-encoding a decoded term");
    assert_eq!(reencoded, oracle.plain, "the identity path must decode the same term");
}

/// The same invariant, oracle-free: our own compressed image, followed by junk, is
/// consumed exactly up to the image end. This always runs.
#[test]
fn our_own_compressed_image_with_trailing_junk_is_consumed_up_to_the_image() {
    // `lists:duplicate(500, hello)` — a large, highly compressible term.
    let mut atoms = AtomTable::new();
    let hello = make_atom(atoms.intern("hello"));
    let mut h = heap(4096);
    let mut acc = NIL;
    for _ in 0..500 {
        acc = h.cons(hello, acc).expect("heap sized for the list");
    }
    let term: Eterm = acc;

    let image = encode_term_compressed(term, &atoms, 6).expect("compressing a term");
    assert!(
        is_compressed_image(&image),
        "the term must actually be wrapped: {:?}",
        &image[..image.len().min(8)]
    );

    let mut input = image.clone();
    input.extend_from_slice(&JUNK);

    let mut d_heap = heap(1 << 16);
    let mut d_atoms = AtomTable::new();
    let (decoded, consumed) = decode_term_used(&input, &mut d_heap, &mut d_atoms)
        .expect("decoding our own compressed image");
    assert_eq!(
        consumed,
        image.len(),
        "the count must stop at the compressed image, not the longer input"
    );
    // Decode→re-encode is not byte-identical (zlib vs miniz_oxide), so compare the
    // re-encoded plain image instead.
    let plain = encode_term(term, &atoms).expect("plain encode");
    let reencoded = encode_term(decoded, &d_atoms).expect("re-encoding a decoded term");
    assert_eq!(reencoded, plain, "the decoded term must be the original term");
}
