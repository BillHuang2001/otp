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

//! **Live oracle cross-checks for the `COMPRESSED` (`80`) path** — the
//! `term_to_binary(T, [compressed])` / `binary_to_term/1` zlib wrapper.
//!
//! Unlike [`compressed_decode`](../compressed_decode.rs) (offline, hand-built
//! vectors) this file runs the *system* `erl` and compares the two implementations
//! directly. Ground truth is the legacy C `binary2term_prepare`
//! (`external.c:1744-1816`, decode) and the `erts_term_to_binary_simple` shrink rule
//! (`external.c:2276-2298`, encode):
//!
//! * a compressed image is `[131][80][inner_size:4 BE][zlib stream]`, the 4-byte size
//!   being the inner term's byte length **excluding** the version byte;
//! * the wrapper is emitted only when zlib fits the stream into `body_len - 5` bytes —
//!   otherwise the plain `[131][body]` image is emitted verbatim;
//! * `80` is accepted only as the outermost wrapper (a nested `80` is `badarg`);
//! * a declared size that disagrees with the inflated length, a truncated or corrupt
//!   zlib stream, or a non-zlib payload are all `badarg`.
//!
//! The deflate bytes of a compressed image are **not** byte-comparable across the two
//! implementations (the C uses zlib, we use `flate2`'s `miniz_oxide`), so the wrapper's
//! *choice* and the *decoded* term are compared, never the stream itself.
//!
//! Every test resolves the oracle the same way [`tools/differential`] does
//! (`$ERTS_ORACLE` if it is an existing executable, else `erl` on `$PATH`) and skips
//! **loudly** — never a silent pass — when no oracle is available.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_term::prelude::{make_atom, make_small, AtomTable, Eterm, NIL};

use beam_codec::compress::decompress;
use beam_codec::{decode_term, encode_term, encode_term_compressed, COMPRESSED_EXT, VERSION_MAGIC};

// ---------------------------------------------------------------------------
// Oracle plumbing (same resolution policy as tools/differential / interop.rs)
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
            "[beam_codec compressed_oracle] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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
            eprintln!("=============== beam_codec compressed_oracle: {test} SKIPPED ===============");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("=============================================================================");
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

// ---------------------------------------------------------------------------
// Byte-list <-> binary-literal helpers
// ---------------------------------------------------------------------------

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

/// Renders bytes as an Erlang binary literal `<<a,b,c>>` (decimal byte values), the
/// argument form used to feed an image to the oracle's `binary_to_term/1`.
fn binary_literal(bytes: &[u8]) -> String {
    let items: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("<<{}>>", items.join(","))
}

/// The 4-byte declared inner size of a compressed image.
fn declared_size(image: &[u8]) -> usize {
    assert!(image.len() >= 6, "truncated compressed header: {image:?}");
    u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize
}

fn is_compressed(image: &[u8]) -> bool {
    image.len() >= 6 && image[0] == VERSION_MAGIC && image[1] == COMPRESSED_EXT
}

// ---------------------------------------------------------------------------
// Term building (mirrors roundtrip.rs helpers)
// ---------------------------------------------------------------------------

fn heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

fn list(h: &mut Heap, elems: &[Eterm], tail: Eterm) -> Eterm {
    let mut acc = tail;
    for &e in elems.iter().rev() {
        acc = h.cons(e, acc).expect("heap sized for the list");
    }
    acc
}

/// `{ok, 17}` — the small term the shrink rule reverts to the plain image.
fn small_term() -> (Heap, AtomTable, Eterm) {
    let mut atoms = AtomTable::new();
    let ok = make_atom(atoms.intern("ok"));
    let mut h = heap(64);
    let t = h.tuple(&[ok, make_small(17)]).expect("a 2-tuple");
    (h, atoms, t)
}

/// `lists:duplicate(500, hello)` — a large, highly compressible term.
fn big_atom_list() -> (Heap, AtomTable, Eterm) {
    let mut atoms = AtomTable::new();
    let hello = make_atom(atoms.intern("hello"));
    let mut h = heap(4096);
    let t = list(&mut h, &[hello; 500], NIL);
    (h, atoms, t)
}

/// `lists:seq(1, 1000)` — a large term whose compression ratio is different.
fn seq_list() -> (Heap, AtomTable, Eterm) {
    let atoms = AtomTable::new();
    let mut h = heap(4096);
    let smalls: Vec<Eterm> = (1..=1000).map(make_small).collect();
    let t = list(&mut h, &smalls, NIL);
    (h, atoms, t)
}

const DUP_500_HELLO: &str = "lists:duplicate(500,hello)";
const SEQ_1_1000: &str = "lists:seq(1,1000)";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The oracle's own `term_to_binary(T, [compressed])` decodes here to a term that
/// re-encodes byte-identically to the oracle's **plain** `term_to_binary(T)`.
///
/// This pins the whole decode direction end-to-end: the wrapper is recognised, the
/// 4-byte size is honoured, the zlib stream is inflated, and the inner body decodes to
/// exactly the original term.
#[test]
fn oracle_compressed_images_decode_here_and_reencode_identically() {
    let Some(oracle) = oracle_or_skip("oracle_compressed_images_decode_here_and_reencode_identically")
    else {
        return;
    };

    let eval = format!(
        "io:format(\"~w~n\", [term_to_binary({DUP_500_HELLO})]), \
         io:format(\"~w~n\", [term_to_binary({DUP_500_HELLO},[compressed])]), \
         io:format(\"~w~n\", [term_to_binary({SEQ_1_1000})]), \
         io:format(\"~w~n\", [term_to_binary({SEQ_1_1000},[compressed])]), \
         io:format(\"~w~n\", [term_to_binary({{ok,17}},[compressed])])"
    );
    let lines = oracle_lines(&oracle, &eval);
    assert_eq!(lines.len(), 5, "unexpected oracle output: {lines:?}");
    let plain_big = parse_byte_list(&lines[0]);
    let comp_big = parse_byte_list(&lines[1]);
    let plain_seq = parse_byte_list(&lines[2]);
    let comp_seq = parse_byte_list(&lines[3]);
    let comp_small = parse_byte_list(&lines[4]);

    // The oracle really did compress the two large terms (and really did revert the
    // small one — see the shrink-rule test).
    assert!(is_compressed(&comp_big), "expected a wrapper: {comp_big:?}");
    assert!(is_compressed(&comp_seq), "expected a wrapper: {comp_seq:?}");
    assert!(!is_compressed(&comp_small), "expected a revert: {comp_small:?}");

    // Our decode of the oracle's compressed image must equal the oracle's plain term
    // byte-for-byte when we re-encode it (list/tuple atoms encode by name, so the
    // comparison is interning-order independent).
    for (comp, plain, label) in [
        (&comp_big, &plain_big, "duplicate(500,hello)"),
        (&comp_seq, &plain_seq, "seq(1,1000)"),
    ] {
        let mut h = heap(1 << 16);
        let mut atoms = AtomTable::new();
        let term = decode_term(comp, &mut h, &mut atoms)
            .unwrap_or_else(|e| panic!("{label}: decoding the oracle image failed: {e:?}"));
        let reencoded = encode_term(term, &atoms).expect("re-encoding a decoded term");
        assert_eq!(
            reencoded, *plain,
            "{label}: decode(oracle compressed) must re-encode to the oracle's plain image"
        );
    }
}

/// Our `encode_term_compressed` output is accepted by the oracle's `binary_to_term/1`
/// and equals the original term.
///
/// The deflate bytes need not match the oracle's; only the decoded term must.
#[test]
fn our_compressed_images_are_accepted_by_the_oracle() {
    let Some(oracle) = oracle_or_skip("our_compressed_images_are_accepted_by_the_oracle") else {
        return;
    };

    let cases: [(&str, fn() -> (Heap, AtomTable, Eterm), &str); 3] = [
        (
            "duplicate(500,hello)",
            big_atom_list,
            DUP_500_HELLO,
        ),
        ("seq(1,1000)", seq_list, SEQ_1_1000),
        ("{ok,17}", small_term, "{ok,17}"),
    ];

    for (label, build, erl_term) in cases {
        let (_h, atoms, term) = build();
        let ours = encode_term_compressed(term, &atoms, 6).expect("compressing a term");
        let eval = format!(
            "B = {}, io:format(\"~w\", [binary_to_term(B) =:= {}])",
            binary_literal(&ours),
            erl_term
        );
        let out = run_erl(&oracle, &eval);
        assert_eq!(
            out.trim(),
            "true",
            "{label}: the oracle rejected or altered our image {ours:?}"
        );
        // Sanity: a large term must actually have been wrapped, the small one reverted.
        if label == "{ok,17}" {
            assert!(!is_compressed(&ours), "{label}: unexpected wrapper");
        } else {
            assert!(is_compressed(&ours), "{label}: expected a wrapper");
        }
    }
}

/// The shrink rule, byte-for-byte: a small term stays UNCOMPRESSED and our image is
/// byte-identical to the oracle's `term_to_binary(T, [compressed])` **and** to the
/// plain `term_to_binary(T)`; a large compressible term gets the wrapper on both
/// sides.
#[test]
fn shrink_rule_matches_the_oracle() {
    let Some(oracle) = oracle_or_skip("shrink_rule_matches_the_oracle") else {
        return;
    };

    // `{ok,17}`: 9 body bytes; zlib cannot fit the stream into `9 - 5 == 4` bytes, so
    // the C (and we) revert to the plain image.
    let eval = "T={ok,17}, io:format(\"~w~n\", [term_to_binary(T)]), \
                io:format(\"~w~n\", [term_to_binary(T,[compressed])]), \
                io:format(\"~w~n\", [term_to_binary(T,[{compressed,0}])]), \
                io:format(\"~w~n\", [term_to_binary(T,[{compressed,9}])])";
    let lines = oracle_lines(&oracle, &eval);
    assert_eq!(lines.len(), 4, "unexpected oracle output: {lines:?}");
    let plain = parse_byte_list(&lines[0]);
    for (i, line) in lines.iter().enumerate().skip(1) {
        let image = parse_byte_list(line);
        assert_eq!(
            image, plain,
            "line {i}: the oracle reverted a small term to the plain image"
        );
    }

    // Our `{ok,17}` image is byte-identical to the oracle's plain image at every level.
    let (_h, atoms, term) = small_term();
    let ours_plain = encode_term(term, &atoms).expect("plain encode");
    assert_eq!(ours_plain, plain, "our plain image must match the oracle's");
    for level in 0..=9u8 {
        let ours = encode_term_compressed(term, &atoms, level).expect("compressed encode");
        assert_eq!(
            ours, plain,
            "level {level}: a small term must revert to the oracle's plain image"
        );
    }

    // A large term: the wrapper appears on both sides.
    let eval_big = format!(
        "io:format(\"~w~n\", [term_to_binary({DUP_500_HELLO},[compressed])])"
    );
    let oracle_big = parse_byte_list(&oracle_lines(&oracle, &eval_big)[0]);
    assert!(is_compressed(&oracle_big), "oracle: expected a wrapper");
    let (_h, atoms, term) = big_atom_list();
    let ours_big = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert!(is_compressed(&ours_big), "ours: expected a wrapper");
}

/// The 4-byte size field is the inner term's byte length **excluding** the version
/// byte — the same value the oracle writes, for both a large and a minimal wrapper.
#[test]
fn declared_size_is_the_inner_body_length() {
    let Some(oracle) = oracle_or_skip("declared_size_is_the_inner_body_length") else {
        return;
    };

    let eval = format!(
        "io:format(\"~w~n\", [term_to_binary({DUP_500_HELLO})]), \
         io:format(\"~w~n\", [term_to_binary({DUP_500_HELLO},[compressed])])"
    );
    let lines = oracle_lines(&oracle, &eval);
    let plain = parse_byte_list(&lines[0]);
    let comp = parse_byte_list(&lines[1]);
    assert!(is_compressed(&comp), "expected a wrapper: {comp:?}");

    // The oracle's declared size == its plain image length minus the version byte.
    assert_eq!(
        declared_size(&comp),
        plain.len() - 1,
        "oracle declared size must exclude the version byte"
    );
    // …and the inflated stream is exactly that body.
    assert_eq!(
        decompress(&comp[6..]).expect("inflating the oracle stream"),
        plain[1..],
        "the inflated body must equal the plain body (version byte excluded)"
    );

    // Our encoder writes the same declared size, and our own decoder accepts it.
    let (_h, atoms, term) = big_atom_list();
    let ours = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert!(is_compressed(&ours));
    assert_eq!(declared_size(&ours), plain.len() - 1);

    let mut h2 = heap(1 << 16);
    let mut a2 = AtomTable::new();
    let decoded = decode_term(&ours, &mut h2, &mut a2).expect("decode our own image");
    assert_eq!(encode_term(decoded, &a2).expect("re-encode"), plain);
}

/// The error contract agrees with the oracle: a nested `COMPRESSED`, a wrong declared
/// size, a truncated or corrupt zlib stream, and a non-zlib payload are all `badarg`
/// on both sides.
#[test]
fn malformed_compressed_images_are_badarg_in_both() {
    let Some(oracle) = oracle_or_skip("malformed_compressed_images_are_badarg_in_both") else {
        return;
    };

    // A valid image to mutate (ours; the oracle accepts it — see the acceptance test).
    let (_h, atoms, term) = big_atom_list();
    let valid = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert!(is_compressed(&valid), "the base image must be wrapped");

    // 1. A nested `80` inside a term: a 1-tuple whose only element is the wrapper tag.
    let nested = vec![VERSION_MAGIC, 104, 1, COMPRESSED_EXT, 0, 0, 0, 0];
    // 2. A declared size off by one.
    let mut wrong_size = valid.clone();
    let bumped = (declared_size(&valid) as u32).wrapping_add(1);
    wrong_size[2..6].copy_from_slice(&bumped.to_be_bytes());
    // 3. Truncated: the deflate stream / Adler-32 trailer is incomplete.
    let truncated = valid[..valid.len() - 1].to_vec();
    // 4. Corrupt: a flipped byte in the Adler-32 trailer.
    let mut corrupt = valid.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    // 5. Not a zlib stream at all.
    let not_zlib = vec![VERSION_MAGIC, COMPRESSED_EXT, 0, 0, 0, 4, 1, 2, 3, 4];
    // 6. A valid zlib stream whose inflated body is not a term.
    let bad_body = vec![200u8];
    let zlib = beam_codec::compress::compress(&bad_body, 6).expect("zlib");
    let mut bad_inner = vec![VERSION_MAGIC, COMPRESSED_EXT];
    bad_inner.extend_from_slice(&(bad_body.len() as u32).to_be_bytes());
    bad_inner.extend_from_slice(&zlib);

    let named: [(&str, Vec<u8>); 6] = [
        ("nested COMPRESSED", nested),
        ("wrong declared size", wrong_size),
        ("truncated stream", truncated),
        ("corrupt trailer", corrupt),
        ("non-zlib payload", not_zlib),
        ("non-term inner body", bad_inner),
    ];

    // The oracle's verdict for all six.
    let list_items: Vec<String> = named
        .iter()
        .map(|(_, b)| binary_literal(b))
        .collect();
    let eval = format!(
        "F = fun(B) -> try {{ok, binary_to_term(B)}} catch _:_ -> badarg end end, \
         io:format(\"~w\", [[F(B) || B <- [{}]]])",
        list_items.join(",")
    );
    let out = run_erl(&oracle, &eval);
    let oracle_verdicts = out.trim().to_string();

    // Our verdict for all six.
    let mut our_verdicts = Vec::new();
    for (label, bytes) in &named {
        let mut h = heap(1 << 16);
        let mut a = AtomTable::new();
        let verdict = match decode_term(bytes, &mut h, &mut a) {
            Err(BeamError::BadArg) => "badarg".to_string(),
            Err(other) => panic!("{label}: expected BadArg, got {other:?}"),
            Ok(t) => format!("ok({t:?})"),
        };
        our_verdicts.push(verdict);
    }

    assert_eq!(
        oracle_verdicts,
        "[badarg,badarg,badarg,badarg,badarg,badarg]",
        "the oracle did not reject one of the malformed images"
    );
    assert_eq!(
        our_verdicts,
        vec!["badarg"; 6],
        "we did not reject a malformed image the oracle rejects: {named:?}"
    );
}

/// A compressed image followed by trailing bytes after the zlib stream is **accepted**
/// by both: the C inflates the stream and ignores the rest (the oracle agrees).
#[test]
fn trailing_bytes_after_the_zlib_stream_are_ignored_by_both() {
    let Some(oracle) = oracle_or_skip("trailing_bytes_after_the_zlib_stream_are_ignored_by_both")
    else {
        return;
    };

    let (_h, atoms, term) = big_atom_list();
    let mut image = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert!(is_compressed(&image));

    // The oracle's plain image is the reference term encoding.
    let plain = parse_byte_list(&oracle_lines(
        &oracle,
        &format!("io:format(\"~w~n\", [term_to_binary({DUP_500_HELLO})])"),
    )[0]);

    image.extend_from_slice(&[1, 2, 3, 4, 5]);

    // The oracle accepts it and returns the original term.
    let eval = format!(
        "B = {}, io:format(\"~w\", [binary_to_term(B) =:= {DUP_500_HELLO}])",
        binary_literal(&image)
    );
    assert_eq!(run_erl(&oracle, &eval).trim(), "true");

    // We accept it too and decode the same term.
    let mut h = heap(1 << 16);
    let mut a = AtomTable::new();
    let decoded = decode_term(&image, &mut h, &mut a).expect("trailing bytes are ignored");
    assert_eq!(
        encode_term(decoded, &a).expect("re-encode"),
        plain,
        "trailing bytes after the zlib stream must not change the decoded term"
    );
}

/// A compressed payload whose inflated size exceeds 32 MiB is **accepted by the oracle**
/// and must therefore be accepted here too.
///
/// The C's `dest_len > 32*1024*1024` test (`external.c:1768`) is *not* a rejection: it
/// only decides that the declared size must be verified against the real uncompressed
/// length (`binary2term_uncomp_size`, `external.c:1741`) before the buffer is allocated.
/// A 32 MiB ceiling on our side would make us reject an image our *own*
/// `encode_term_compressed` just produced — a self-inconsistency and a divergence from
/// the oracle. `decompress_bounded` instead bounds inflation by the declared size, which
/// is exactly what the C requires (`dlen != dest_len`, `external.c:1805-1810`).
#[test]
fn a_payload_larger_than_32_mib_round_trips_on_both_sides() {
    let Some(oracle) = oracle_or_skip("a_payload_larger_than_32_mib_round_trips_on_both_sides")
    else {
        return;
    };

    // One byte past the historical 32 MiB ceiling, so the case is genuinely over it.
    const N: usize = 32 * 1024 * 1024 + 1;
    let data = vec![0u8; N];

    // Our image of that binary.
    let (_h, atoms, term) = {
        let mut h = heap(N / 8 + 8192);
        let t = beam_codec::term::make_binary(&mut h, &data).expect("a large on-heap binary");
        (h, AtomTable::new(), t)
    };
    let body = encode_term(term, &atoms).expect("plain encode");
    assert!(body.len() > 32 * 1024 * 1024, "body is {} bytes", body.len());
    let ours = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert!(is_compressed(&ours), "expected a wrapper: {} bytes", ours.len());
    assert_eq!(
        declared_size(&ours),
        body.len() - 1,
        "our declared size must be the inner body length (version byte excluded)"
    );

    // The oracle accepts our image and reconstructs the same binary — and it declares the
    // same size for *its* image of the same term.
    let eval = format!(
        "B = {}, T = binary_to_term(B), io:format(\"~w ~w\", \
         [byte_size(T), binary:decode_unsigned(binary:part(term_to_binary(T, [compressed]), 2, 4))])",
        binary_literal(&ours)
    );
    let out = run_erl(&oracle, &eval);
    let fields: Vec<&str> = out.split_whitespace().collect();
    let expected_size = N.to_string();
    assert_eq!(
        fields.first().copied(),
        Some(expected_size.as_str()),
        "the oracle did not reconstruct the {N}-byte binary: {out:?}"
    );
    assert_eq!(
        declared_size(&ours),
        N + 5,
        "our declared size must match the oracle's ({out:?})"
    );

    // And *we* accept our own image — the regression this test pins.
    let mut h2 = heap(N / 8 + 8192);
    let mut a2 = AtomTable::new();
    let decoded = decode_term(&ours, &mut h2, &mut a2)
        .expect("a >32 MiB compressed payload must not be rejected by an artificial cap");
    assert_eq!(
        beam_codec::term::bitstring_size_bits(decoded).expect("a binary"),
        N * 8
    );
    assert_eq!(encode_term(decoded, &a2).expect("re-encode"), body);
}
