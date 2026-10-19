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
//! **Every image below is compared byte for byte.** The deflate behind the wrapper is
//! `beam_deflate`, the workspace's safe-Rust port of the zlib 1.3.2 the oracle itself
//! runs, so `term_to_binary(T, [{compressed,Level}])` produced here is not merely
//! decodable — it is the oracle's image, stream included. What the tests pin is the
//! whole image (wrapper choice *and* stream) for binaries, lists, tuples, the boundary
//! witnesses and payloads past 32 KiB, plus the derived rule behind the choice: `Z <=
//! body - 5`, where `body` is the plain body length (plain image minus the version byte)
//! and `Z` is the zlib stream over exactly those bytes. The equality case lands on the
//! COMPRESSED side, `body < 5` never compresses, and level `0` never compresses at all
//! (`[{compressed,0}]` returns the plain image exactly).
//!
//! Every test resolves the oracle the same way [`tools/differential`] does
//! (`$ERTS_ORACLE` if it is an existing executable, else `erl` on `$PATH`) and skips
//! **loudly** — never a silent pass — when no oracle is available. A large image never
//! travels through `argv` or stdout: the oracle `file:write_file/2`s it into a unique
//! path under `std::env::temp_dir()` and the test reads it back.
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
            skip_note(
                test,
                "No usable system `erl` oracle found ($ERTS_ORACLE unset or not executable, \
                 and no `erl` on $PATH).",
            );
            None
        }
    }
}

/// The loud "this run is NOT a pass" banner a skipped case prints.
fn skip_note(test: &str, why: &str) {
    eprintln!();
    eprintln!("=============== beam_codec compressed_oracle: {test} SKIPPED ===============");
    eprintln!("{why}");
    eprintln!("This test is NOT a pass — install what it needs (or set $ERTS_ORACLE) and re-run.");
    eprintln!("=============================================================================");
    eprintln!();
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
// Byte-identity plumbing: hex, whole-image comparison, temp files, the LCG corpus
// ---------------------------------------------------------------------------

/// Lowercase hex — the rendering of the goldens and of every failure message (the
/// oracle prints the same via `binary:encode_hex(Image, lowercase)`).
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses lowercase hex (`binary:encode_hex/2`).
fn parse_hex(s: &str) -> Vec<u8> {
    let t = s.trim();
    assert!(t.len().is_multiple_of(2), "odd-length hex string: {t:?}");
    t.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hex = std::str::from_utf8(pair).unwrap_or_else(|_| panic!("bad hex {t:?}"));
            u8::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("bad hex {t:?}"))
        })
        .collect()
}

/// Asserts two images are **byte-identical**, reporting the first differing offset and a
/// hex window around it (`assert_eq!` on a `Vec<u8>` would print thousands of decimals).
fn assert_image_eq(label: &str, ours: &[u8], oracle: &[u8]) {
    if ours == oracle {
        return;
    }
    let at = ours
        .iter()
        .zip(oracle)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| ours.len().min(oracle.len()));
    let lo = at.saturating_sub(8);
    let window = |image: &[u8]| to_hex(&image[lo.min(image.len())..(at + 24).min(image.len())]);
    panic!(
        "{label}: our image is not the oracle's — first difference at byte {at}\n  \
         ours   ({} B) [{lo}..]: {}\n  oracle ({} B) [{lo}..]: {}",
        ours.len(),
        window(ours),
        oracle.len(),
        window(oracle),
    );
}

/// A unique path under `std::env::temp_dir()` — process id, nanos and a per-process
/// counter, so concurrent runs and concurrent test binaries never collide.
fn unique_temp_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "beam_codec_{tag}_{}_{seq}_{nanos}.tmp",
        std::process::id()
    ))
}

/// `path` rendered as an Erlang string literal (backslash and quote escaped).
fn erl_string(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Reads the image the oracle wrote and removes it.
fn read_oracle_image(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("reading the oracle image {}: {e}", path.display()));
    let _ = std::fs::remove_file(path);
    bytes
}

/// The 31-bit LCG corpus — `s_{n+1} = (s_n * 1103515245 + 12345) band 16#7FFFFFFF`,
/// `byte = (s_n bsr 16) band 255`, `s_0 = 12345`.
///
/// It is *incompressible*, so its zlib stream costs `N + 11` bytes (a stored block) and
/// the wrapper only ever pays off against a leading zero run — which is exactly what
/// makes the second C path's boundary (`body` past 64 bytes) observable.
fn lcg_bytes(n: usize) -> Vec<u8> {
    let mut state: u32 = 12345;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        state = state.wrapping_mul(1103515245).wrapping_add(12345) & 0x7FFF_FFFF;
        out.push(((state >> 16) & 0xFF) as u8);
    }
    out
}

/// Resolve the system `erlc`: `$ERLC` if it is an existing executable, else the `erlc`
/// next to the resolved oracle, else `erlc` on `$PATH`.
fn resolve_erlc(oracle: &Path) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERLC") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
    }
    if let Some(sibling) = oracle.parent().map(|dir| dir.join("erlc")) {
        if is_executable(&sibling) {
            return Some(sibling);
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erlc");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// A compact RFC 1321 MD5 (test-only): the large-payload golden is stated as a digest so
/// it is readable in a failure message without 190 hex characters of context.
fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for offset in (0..message.len()).step_by(64) {
        let block = &message[offset..offset + 64];
        let mut words = [0u32; 16];
        for (i, word) in words.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        for i in 0..64 {
            let (mix, index) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(mix)
                .wrapping_add(K[i])
                .wrapping_add(words[index])
                .rotate_left(S[i]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    state
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

/// A test case: a label, a builder for the term, and the term's Erlang source text.
type Case = (&'static str, fn() -> (Heap, AtomTable, Eterm), &'static str);

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

/// Our `encode_term_compressed` output is **the oracle's image, byte for byte**, for
/// every one of three terms at levels `0, 1, 6, 9` — and the oracle still accepts it and
/// returns the original term.
///
/// The `[{compressed,L}]` images are compared whole (`wrapper + stream + trailer`), so
/// this pins the deflate itself, not just the wrapper's choice: level `0` must reproduce
/// the plain image exactly (the C never compresses at level 0) and the small `{ok,17}`
/// term must revert at every level.
#[test]
fn our_compressed_images_are_byte_identical_to_the_oracle() {
    let Some(oracle) = oracle_or_skip("our_compressed_images_are_byte_identical_to_the_oracle")
    else {
        return;
    };

    const LEVELS: [u8; 4] = [0, 1, 6, 9];
    let cases: [Case; 3] = [
        ("duplicate(500,hello)", big_atom_list, DUP_500_HELLO),
        ("seq(1,1000)", seq_list, SEQ_1_1000),
        ("{ok,17}", small_term, "{ok,17}"),
    ];

    // One oracle boot: the image of every (term, level) pair as lowercase hex, in the
    // comprehension's order (term outer, level inner).
    let erl_terms: Vec<&str> = cases.iter().map(|(_, _, erl)| *erl).collect();
    let eval = format!(
        "Ts = [{}], Ls = [0,1,6,9], \
         [io:format(\"~s~n\", [binary:encode_hex(term_to_binary(T, [{{compressed,L}}]), \
          lowercase)]) || T <- Ts, L <- Ls]",
        erl_terms.join(",")
    );
    let lines = oracle_lines(&oracle, &eval);
    assert_eq!(
        lines.len(),
        cases.len() * LEVELS.len(),
        "unexpected oracle output: {lines:?}"
    );

    let mut line = 0;
    let mut our_level6: Vec<Vec<u8>> = Vec::new();
    for (label, build, _) in cases {
        let (_h, atoms, term) = build();
        let plain = encode_term(term, &atoms).expect("plain encode");
        for level in LEVELS {
            let oracle_image = parse_hex(&lines[line]);
            line += 1;
            let ours = encode_term_compressed(term, &atoms, level).expect("compressed encode");
            assert_image_eq(&format!("{label} level {level}"), &ours, &oracle_image);
            // Derived from the rule, independently of the bytes above: level 0 never
            // compresses, and otherwise the wrapper appears iff the zlib stream fits the
            // `body - 5` bytes the wrapper costs.
            let body = plain.len() - 1;
            let wrapper_pays = level != 0
                && body >= 5
                && beam_codec::compress::compress(&plain[1..], level)
                    .expect("zlib")
                    .len()
                    <= body - 5;
            assert_eq!(
                is_compressed(&oracle_image),
                wrapper_pays,
                "{label} level {level}: the oracle's wrapper choice contradicts \
                 `Z <= body - 5` (body {body}, image {} bytes)",
                oracle_image.len()
            );
            if level == 0 {
                assert_image_eq(
                    &format!("{label} level 0 (plain)"),
                    &oracle_image,
                    &plain,
                );
            }
            if level == 6 {
                // Sanity: a large term really is wrapped, the small one really reverts.
                assert_eq!(
                    is_compressed(&ours),
                    label != "{ok,17}",
                    "{label}: unexpected wrapper choice"
                );
                our_level6.push(ours);
            }
        }
    }

    // The oracle still accepts our images and reconstructs the original terms.
    let pairs: Vec<String> = our_level6
        .iter()
        .zip(&erl_terms)
        .map(|(image, erl)| format!("{{{},{erl}}}", binary_literal(image)))
        .collect();
    let eval = format!(
        "io:format(\"~w\", [[binary_to_term(B) =:= T || {{B, T}} <- [{}]]])",
        pairs.join(",")
    );
    assert_eq!(
        run_erl(&oracle, &eval).trim(),
        "[true,true,true]",
        "the oracle rejected or altered one of our images"
    );
}

/// The shrink rule, byte-for-byte: a small term stays UNCOMPRESSED and our image is
/// byte-identical to the oracle's `term_to_binary(T, [compressed])` **and** to the
/// plain `term_to_binary(T)`; a large compressible term gets the wrapper on both
/// sides — with the same stream, so the two compressed images are compared whole.
#[test]
fn shrink_rule_matches_the_oracle() {
    let Some(oracle) = oracle_or_skip("shrink_rule_matches_the_oracle") else {
        return;
    };

    // `{ok,17}`: 9 body bytes; zlib cannot fit the stream into `9 - 5 == 4` bytes, so
    // the C (and we) revert to the plain image.
    let eval = "T={ok,17}, io:format(\"~s~n\", [binary:encode_hex(term_to_binary(T), lowercase)]), \
                io:format(\"~s~n\", [binary:encode_hex(term_to_binary(T,[compressed]), lowercase)]), \
                io:format(\"~s~n\", [binary:encode_hex(term_to_binary(T,[{compressed,0}]), lowercase)]), \
                io:format(\"~s~n\", [binary:encode_hex(term_to_binary(T,[{compressed,9}]), lowercase)])";
    let lines = oracle_lines(&oracle, eval);
    assert_eq!(lines.len(), 4, "unexpected oracle output: {lines:?}");
    let plain = parse_hex(&lines[0]);
    for (i, line) in lines.iter().enumerate().skip(1) {
        let image = parse_hex(line);
        assert_eq!(
            image, plain,
            "line {i}: the oracle reverted a small term to the plain image"
        );
    }

    // Our `{ok,17}` image is byte-identical to the oracle's plain image at every level.
    let (_h, atoms, term) = small_term();
    let ours_plain = encode_term(term, &atoms).expect("plain encode");
    assert_image_eq("{ok,17} plain", &ours_plain, &plain);
    for level in 0..=9u8 {
        let ours = encode_term_compressed(term, &atoms, level).expect("compressed encode");
        assert_eq!(
            ours, plain,
            "level {level}: a small term must revert to the oracle's plain image"
        );
    }

    // A large term: the wrapper appears on both sides — and the whole image agrees.
    let eval_big = format!(
        "io:format(\"~s~n\", [binary:encode_hex(term_to_binary({DUP_500_HELLO},[compressed]), \
         lowercase)])"
    );
    let oracle_big = parse_hex(&oracle_lines(&oracle, &eval_big)[0]);
    assert!(is_compressed(&oracle_big), "oracle: expected a wrapper");
    let (_h, atoms, term) = big_atom_list();
    let ours_big = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert!(is_compressed(&ours_big), "ours: expected a wrapper");
    assert_image_eq("duplicate(500,hello) [compressed]", &ours_big, &oracle_big);
}

/// The wrapper boundary, witnessed by `binary:copy(<<"a">>, N)` — plain and
/// `[compressed]`, every image compared whole.
///
/// `N = 15` is the **equality case**: `body = 20` and `Z = 15`, so `6 + Z == 1 + body` and
/// the oracle keeps the COMPRESSED image (same total size as the plain one — which is why
/// the rule is not "strictly smaller"). `N = 14` reverts (`Z = 15 > body - 5 = 14`), at
/// `N = 16` the compressed image is a byte *shorter*, and `N = 1024` is the ordinary,
/// clearly-profitable case.
#[test]
fn boundary_witnesses_are_byte_identical() {
    let Some(oracle) = oracle_or_skip("boundary_witnesses_are_byte_identical") else {
        return;
    };

    const SIZES: [usize; 4] = [14, 15, 16, 1024];
    let eval = "[begin T = binary:copy(<<\"a\">>, N), \
                 io:format(\"~s~n\", [binary:encode_hex(term_to_binary(T), lowercase)]), \
                 io:format(\"~s~n\", [binary:encode_hex(term_to_binary(T, [compressed]), \
                  lowercase)]) end || N <- [14,15,16,1024]]";
    let lines = oracle_lines(&oracle, eval);
    assert_eq!(
        lines.len(),
        2 * SIZES.len(),
        "unexpected oracle output: {lines:?}"
    );

    let mut witnesses: Vec<(usize, Vec<u8>, Vec<u8>)> = Vec::new();
    for (k, &n) in SIZES.iter().enumerate() {
        let oracle_plain = parse_hex(&lines[2 * k]);
        let oracle_comp = parse_hex(&lines[2 * k + 1]);
        let data = vec![b'a'; n];
        let (_h, atoms, term) = {
            let mut h = heap(n / 8 + 64);
            let t = beam_codec::term::make_binary(&mut h, &data).expect("an on-heap binary");
            (h, AtomTable::new(), t)
        };
        let our_plain = encode_term(term, &atoms).expect("plain encode");
        let our_comp = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
        assert_image_eq(
            &format!("binary:copy(<<\"a\">>, {n}) plain"),
            &our_plain,
            &oracle_plain,
        );
        assert_image_eq(
            &format!("binary:copy(<<\"a\">>, {n}) [compressed]"),
            &our_comp,
            &oracle_comp,
        );

        // The rule, recomputed from the plain body with our own zlib rather than read off
        // the compared bytes.
        let body = our_plain.len() - 1;
        let z = beam_codec::compress::compress(&our_plain[1..], 6)
            .expect("zlib")
            .len();
        let pays = body >= 5 && z <= body - 5;
        assert_eq!(
            is_compressed(&our_comp),
            pays,
            "N={n}: our wrapper choice contradicts `Z ({z}) <= body - 5 ({})`",
            body as i64 - 5
        );
        assert_eq!(
            is_compressed(&oracle_comp),
            pays,
            "N={n}: the oracle's wrapper choice contradicts `Z <= body - 5`"
        );
        witnesses.push((n, our_plain, our_comp));
    }

    let pick = |n: usize| {
        witnesses
            .iter()
            .find(|(size, _, _)| *size == n)
            .expect("a sized witness")
            .clone()
    };

    // N = 14: `Z = 15` cannot fit `body - 5 = 14`, so the plain image comes back verbatim.
    let (_, p14, c14) = pick(14);
    assert!(!is_compressed(&c14), "N=14 must NOT be compressed");
    assert_eq!(c14, p14, "N=14 must revert to the plain image exactly");

    // N = 15: the equality case, and it lands on the COMPRESSED side.
    let (_, p15, c15) = pick(15);
    assert!(is_compressed(&c15), "N=15 is the equality case and must be COMPRESSED");
    assert_eq!(
        c15.len(),
        p15.len(),
        "N=15: the compressed image is the same total size as the plain one"
    );
    assert_eq!(
        c15.len() - 6,
        p15.len() - 1 - 5,
        "N=15: Z must equal body - 5 exactly (the equality case)"
    );
    assert_eq!(
        to_hex(&c15),
        "835000000014789ccb656060e04f4405003700062c",
        "N=15: the equality-case image is the oracle's, byte for byte"
    );

    // N = 16: strictly shorter than the plain image.
    let (_, p16, c16) = pick(16);
    assert!(is_compressed(&c16), "N=16 must be compressed");
    assert!(
        c16.len() < p16.len(),
        "N=16: the compressed image must be shorter ({} vs {})",
        c16.len(),
        p16.len()
    );

    // N = 1024: the ordinary case — wrapped, the oracle's 27-byte image.
    let (_, p1024, c1024) = pick(1024);
    assert!(is_compressed(&c1024), "N=1024 must be compressed");
    assert_eq!(c1024.len(), 27, "N=1024: the image is {} bytes", c1024.len());
    assert_eq!(
        md5_hex(&c1024),
        "29a8ebee49118a2c8d79005621b3627a",
        "N=1024: the image digest must be the oracle's"
    );
    assert!(p1024.len() > c1024.len());
}

/// The level matrix on one body (`binary:copy(<<"a">>,100)`): levels `0, 1, 2, 6, 9`, each
/// image byte-identical to the oracle's — level `0` returning the plain image *exactly* —
/// plus the out-of-range levels, which are `badarg` on both sides.
#[test]
fn level_matrix_is_byte_identical() {
    let Some(oracle) = oracle_or_skip("level_matrix_is_byte_identical") else {
        return;
    };

    const LEVELS: [u8; 5] = [0, 1, 2, 6, 9];
    let eval = "T = binary:copy(<<\"a\">>,100), \
                [io:format(\"~s~n\", [binary:encode_hex(term_to_binary(T, [{compressed,L}]), \
                  lowercase)]) || L <- [0,1,2,6,9]], \
                V = fun(L) -> try term_to_binary(T, [{compressed,L}]) of _ -> ok \
                              catch error:badarg -> badarg end end, \
                io:format(\"~w ~w~n\", [V(10), V(-1)])";
    let lines = oracle_lines(&oracle, eval);
    assert_eq!(
        lines.len(),
        LEVELS.len() + 1,
        "unexpected oracle output: {lines:?}"
    );
    assert_eq!(
        lines[LEVELS.len()],
        "badarg badarg",
        "the oracle must reject levels 10 and -1"
    );

    let data = vec![b'a'; 100];
    let (_h, atoms, term) = {
        let mut h = heap(data.len() / 8 + 64);
        let t = beam_codec::term::make_binary(&mut h, &data).expect("an on-heap binary");
        (h, AtomTable::new(), t)
    };
    let plain = encode_term(term, &atoms).expect("plain encode");
    let mut images: Vec<Vec<u8>> = Vec::new();
    for (i, level) in LEVELS.iter().copied().enumerate() {
        let oracle_image = parse_hex(&lines[i]);
        let ours = encode_term_compressed(term, &atoms, level).expect("compressed encode");
        assert_image_eq(
            &format!("binary:copy(<<\"a\">>, 100) level {level}"),
            &ours,
            &oracle_image,
        );
        if level == 0 {
            assert_eq!(ours, plain, "level 0 must return the plain image exactly");
            assert!(!is_compressed(&ours), "level 0 must never compress");
        } else {
            assert!(is_compressed(&ours), "level {level}: expected a wrapper");
        }
        images.push(ours);
    }
    // The zlib header's FLEVEL bits (and the block choice) differ per level: these are
    // not one stream repeated.
    for pair in 1..LEVELS.len() - 1 {
        assert_ne!(
            images[pair], images[pair + 1],
            "levels {} and {} must not be conflated",
            LEVELS[pair], LEVELS[pair + 1]
        );
    }

    // Out of range on our side. The API takes a `u8`, so the representable half of the C's
    // `0 =< L < 10` rule is the upper end; `-1` (`Z_DEFAULT_COMPRESSION`, which the BIF
    // layer maps to level 6) cannot be expressed through this signature.
    for level in [10u8, 200] {
        match encode_term_compressed(term, &atoms, level) {
            Err(BeamError::BadArg) => {}
            other => panic!("level {level}: expected BadArg, got {other:?}"),
        }
    }
}

/// The **second** C path — a body whose plain image exceeds 64 bytes, so the wrapper
/// decision is taken after the whole image is built — accepts equality too.
///
/// The payload is a run of `P` zero bytes followed by an incompressible 300-byte LCG tail
/// (an LCG byte string costs `N + 11` bytes as a stored zlib block). The images flip at
/// exactly `P = 33`: at `P = 32` the compressed image is the same size as the plain one
/// but still reverts (`Z = body - 4`), at `P = 33` it is the same size and IS compressed
/// (`Z = body - 5`, equality). Two larger incompressible payloads (1024 and 10240 bytes)
/// stay plain on both sides.
#[test]
fn the_second_path_boundary_is_byte_identical() {
    let Some(oracle) = oracle_or_skip("the_second_path_boundary_is_byte_identical") else {
        return;
    };

    let dir = unique_temp_path("lcg");
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("creating the temp dir {}: {e}", dir.display()));
    let tail = lcg_bytes(300);
    let tail_path = dir.join("lcg300.bin");
    std::fs::write(&tail_path, &tail)
        .unwrap_or_else(|e| panic!("writing {}: {e}", tail_path.display()));
    let big_paths: Vec<PathBuf> = [1024usize, 10240]
        .iter()
        .map(|&n| {
            let path = dir.join(format!("lcg{n}.bin"));
            std::fs::write(&path, lcg_bytes(n))
                .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
            path
        })
        .collect();

    // One oracle boot: it reads the corpora from disk and writes each image back to disk,
    // so only sizes travel over stdout.
    let eval = format!(
        "{{ok, Tail}} = file:read_file({tail}), \
         Dir = {dir}, \
         C = fun(P) -> <<(binary:copy(<<0>>,P))/binary, Tail/binary>> end, \
         lists:foreach(fun(P) -> Plain = term_to_binary(C(P)), \
                           Img = term_to_binary(C(P), [compressed]), \
                           ok = file:write_file(filename:join(Dir, \"p\" ++ \
                                integer_to_list(P) ++ \".img\"), Img), \
                           io:format(\"~p ~p ~p~n\", [P, byte_size(Plain), byte_size(Img)]) \
                       end, lists:seq(30,34)), \
         lists:foreach(fun({{Tag, Path}}) -> \
                           {{ok, B}} = file:read_file(Path), \
                           Plain = term_to_binary(B), \
                           Img = term_to_binary(B, [compressed]), \
                           ok = file:write_file(filename:join(Dir, Tag ++ \".img\"), Img), \
                           ok = file:write_file(filename:join(Dir, Tag ++ \".plain\"), Plain), \
                           io:format(\"~s ~p ~p~n\", [Tag, byte_size(Plain), byte_size(Img)]) \
                       end, [{{\"b1024\", {b1024}}}, {{\"b10240\", {b10240}}}])",
        tail = erl_string(&tail_path),
        dir = erl_string(&dir),
        b1024 = erl_string(&big_paths[0]),
        b10240 = erl_string(&big_paths[1]),
    );
    let lines = oracle_lines(&oracle, &eval);
    assert_eq!(lines.len(), 7, "unexpected oracle output: {lines:?}");
    let sizes: Vec<Vec<&str>> = lines
        .iter()
        .map(|line| line.split_whitespace().collect())
        .collect();

    let mut compressed_at: Vec<(usize, bool, usize, usize)> = Vec::new();
    for (i, p) in (30..=34).enumerate() {
        let plain_len: usize = sizes[i][1].parse().expect("a plain size");
        let image_len: usize = sizes[i][2].parse().expect("an image size");
        let mut payload = vec![0u8; p];
        payload.extend_from_slice(&tail);
        let (_h, atoms, term) = {
            let mut h = heap(payload.len() / 8 + 64);
            let t = beam_codec::term::make_binary(&mut h, &payload).expect("an on-heap binary");
            (h, AtomTable::new(), t)
        };
        let ours = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
        let oracle_image = read_oracle_image(&dir.join(format!("p{p}.img")));
        assert_image_eq(&format!("zeros({p}) ++ lcg300"), &ours, &oracle_image);
        assert_eq!(
            ours.len(),
            image_len,
            "P={p}: our image size disagrees with the oracle's reported size"
        );
        compressed_at.push((p, is_compressed(&ours), plain_len, ours.len()));
    }

    // The flip is exactly at P = 33, and both sides of it are the *same total size* as the
    // plain image — the difference is only that the equality case is compressed.
    for &(p, compressed, plain_len, image_len) in &compressed_at {
        let expected = p >= 33;
        assert_eq!(
            compressed, expected,
            "P={p}: the wrapper must appear iff P >= 33 (plain {plain_len}, image {image_len})"
        );
        if p == 32 || p == 33 {
            assert_eq!(
                plain_len, image_len,
                "P={p}: the two images have the same total size — only the choice differs"
            );
        }
    }

    // Two incompressible payloads: stored-block streams cost `N + 11`, so both stay plain
    // on both sides and the whole image is byte-identical.
    for (tag, n) in [("b1024", 1024usize), ("b10240", 10240)] {
        let payload = lcg_bytes(n);
        let (_h, atoms, term) = {
            let mut h = heap(n / 8 + 256);
            let t = beam_codec::term::make_binary(&mut h, &payload).expect("an on-heap binary");
            (h, AtomTable::new(), t)
        };
        let our_plain = encode_term(term, &atoms).expect("plain encode");
        let our_comp = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
        let oracle_plain = read_oracle_image(&dir.join(format!("{tag}.plain")));
        let oracle_comp = read_oracle_image(&dir.join(format!("{tag}.img")));
        assert_image_eq(&format!("incompressible lcg {n} plain"), &our_plain, &oracle_plain);
        assert_image_eq(&format!("incompressible lcg {n} [compressed]"), &our_comp, &oracle_comp);
        assert!(!is_compressed(&our_comp), "lcg {n}: must not be wrapped");
        assert_eq!(our_comp, our_plain, "lcg {n}: must revert to the plain image");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Payloads past 32 KiB: a 60000-byte body (`binary:copy(<<"abc">>,20000)`, a 95-byte
/// image) and a 200000-element list (a 425831-byte image).
///
/// These images are far too large for `argv` (and hex over stdout would be slow), so the
/// oracle writes them to unique temp files and the test reads them back.
#[test]
fn large_payloads_are_byte_identical() {
    let Some(oracle) = oracle_or_skip("large_payloads_are_byte_identical") else {
        return;
    };

    let dir = unique_temp_path("large");
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("creating the temp dir {}: {e}", dir.display()));
    let eval = format!(
        "Dir = {dir}, \
         I1 = term_to_binary(binary:copy(<<\"abc\">>,20000), [compressed]), \
         ok = file:write_file(filename:join(Dir, \"abc.img\"), I1), \
         I2 = term_to_binary(lists:seq(1,200000), [compressed]), \
         ok = file:write_file(filename:join(Dir, \"seq.img\"), I2), \
         io:format(\"~p ~p ~s~n\", [byte_size(I1), byte_size(I2), \
          binary:encode_hex(erlang:md5(I1), lowercase)])",
        dir = erl_string(&dir),
    );
    let lines = oracle_lines(&oracle, &eval);
    assert_eq!(lines.len(), 1, "unexpected oracle output: {lines:?}");
    let fields: Vec<&str> = lines[0].split_whitespace().collect();
    assert_eq!(fields.len(), 3, "unexpected oracle output: {lines:?}");
    assert_eq!(fields[0], "95", "the abc image is {} bytes", fields[0]);
    assert_eq!(fields[1], "425831", "the seq image is {} bytes", fields[1]);
    assert_eq!(
        fields[2], "86a489f3c4120dbb7fa73f5539d8821c",
        "the oracle's abc image digest"
    );

    // The 60000-byte binary.
    let abc = b"abc".repeat(20000);
    let (_h, atoms, term) = {
        let mut h = heap(abc.len() / 8 + 1024);
        let t = beam_codec::term::make_binary(&mut h, &abc).expect("an on-heap binary");
        (h, AtomTable::new(), t)
    };
    let our_abc = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert_eq!(our_abc.len(), 95, "the abc image is {} bytes", our_abc.len());
    assert_eq!(
        md5_hex(&our_abc),
        "86a489f3c4120dbb7fa73f5539d8821c",
        "the abc image digest must be the oracle's"
    );
    assert_image_eq(
        "binary:copy(<<\"abc\">>,20000)",
        &our_abc,
        &read_oracle_image(&dir.join("abc.img")),
    );

    // The 200000-element list.
    let smalls: Vec<Eterm> = (1..=200_000).map(make_small).collect();
    let (_h, atoms, term) = {
        let mut h = heap(1 << 20);
        let t = list(&mut h, &smalls, NIL);
        (h, AtomTable::new(), t)
    };
    let our_seq = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert_eq!(
        our_seq.len(),
        425_831,
        "the seq image is {} bytes",
        our_seq.len()
    );
    assert_image_eq(
        "lists:seq(1,200000)",
        &our_seq,
        &read_oracle_image(&dir.join("seq.img")),
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A genuine `.beam` — the compiler's own output, and the input format the runtime itself
/// eats — is byte-identical too: this is an arbitrary binary payload, not a text-shaped
/// one.
///
/// The `.beam` is compiled here with the system `erlc` (never read from a sibling node's
/// fixtures), so both sides encode exactly the same bytes. Skips loudly when `erlc` or the
/// oracle is missing.
#[test]
fn a_real_beam_payload_is_byte_identical() {
    let Some(oracle) = oracle_or_skip("a_real_beam_payload_is_byte_identical") else {
        return;
    };
    let Some(erlc) = resolve_erlc(&oracle) else {
        skip_note(
            "a_real_beam_payload_is_byte_identical",
            "No usable `erlc` found ($ERLC unset or not executable, none next to the \
             oracle, and none on $PATH).",
        );
        return;
    };

    let dir = unique_temp_path("beam");
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("creating the temp dir {}: {e}", dir.display()));
    let source = dir.join("compressed_oracle_probe.erl");
    std::fs::write(
        &source,
        "-module(compressed_oracle_probe).\n-export([f/1]).\nf(X) -> {ok, X}.\n",
    )
    .unwrap_or_else(|e| panic!("writing {}: {e}", source.display()));

    let compiled = Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn erlc '{}': {e}", erlc.display()));
    assert!(
        compiled.status.success(),
        "erlc '{}' failed with {:?}\n--- stderr ---\n{}",
        erlc.display(),
        compiled.status.code(),
        String::from_utf8_lossy(&compiled.stderr),
    );

    let beam_path = dir.join("compressed_oracle_probe.beam");
    let beam = std::fs::read(&beam_path)
        .unwrap_or_else(|e| panic!("reading the compiled {}: {e}", beam_path.display()));
    assert!(!beam.is_empty(), "erlc produced an empty .beam");

    // The oracle encodes exactly those bytes and writes both images out.
    let comp_path = dir.join("beam.img");
    let plain_path = dir.join("beam.plain");
    let eval = format!(
        "{{ok, B}} = file:read_file({beam}), \
         ok = file:write_file({comp}, term_to_binary(B, [compressed])), \
         ok = file:write_file({plain}, term_to_binary(B)), \
         io:format(\"~p~n\", [byte_size(B)])",
        beam = erl_string(&beam_path),
        comp = erl_string(&comp_path),
        plain = erl_string(&plain_path),
    );
    let lines = oracle_lines(&oracle, &eval);
    assert_eq!(lines.len(), 1, "unexpected oracle output: {lines:?}");
    assert_eq!(
        lines[0],
        beam.len().to_string(),
        "the oracle must read exactly the {} bytes erlc wrote",
        beam.len()
    );

    let (_h, atoms, term) = {
        let mut h = heap(beam.len() / 8 + 256);
        let t = beam_codec::term::make_binary(&mut h, &beam).expect("an on-heap binary");
        (h, AtomTable::new(), t)
    };
    let our_plain = encode_term(term, &atoms).expect("plain encode");
    let our_comp = encode_term_compressed(term, &atoms, 6).expect("compressed encode");
    assert_image_eq(
        "a real .beam — plain",
        &our_plain,
        &read_oracle_image(&plain_path),
    );
    assert_image_eq(
        "a real .beam — [compressed]",
        &our_comp,
        &read_oracle_image(&comp_path),
    );
    assert!(
        is_compressed(&our_comp),
        "a .beam must compress well enough to be wrapped ({} vs {} bytes)",
        our_comp.len(),
        our_plain.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
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
