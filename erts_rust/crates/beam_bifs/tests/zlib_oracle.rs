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
//! Byte identity with the **live** system oracle for the three `zlib` encoders, plus
//! **both** round-trip directions — over a bounded, deterministic corpus.
//!
//! What is pinned, per input:
//!
//! * `zlib:compress/1`, `zlib:zip/1` and `zlib:gzip/1` produce the oracle's bytes
//!   **exactly** (the whole point of the `beam_deflate` port they drive: block splits,
//!   block types, lazy-match decisions and code lengths are zlib's own);
//! * direction 1 — the oracle's `zlib:uncompress/1` / `zlib:unzip/1` / `zlib:gunzip/1`
//!   recover the input from **this crate's** streams;
//! * direction 2 — this crate's `zlib:uncompress/1` / `zlib:unzip/1` / `zlib:gunzip/1`
//!   recover the input from the **oracle's** streams.
//!
//! The corpus is **bounded and deterministic** (a fixed length × shape matrix, 42 inputs,
//! ~244 KiB of payload, a few seconds of runtime): the exhaustive 742-input corpus lives
//! in the task record, not in CI. The three encoders are cheaper here than the exhaustive
//! sweep, but they are the part byte identity has to hold for.
//!
//! The oracle is asked once per encoder (plus once for the round-trip check), so the whole
//! test costs four `erl` startups. The payloads and streams travel as files in a `$TMPDIR`
//! scratch directory rather than as `<<…>>` literals in the `-eval` source: Linux caps a
//! single `execve` argument at 128 KiB (`MAX_ARG_STRLEN`) and a 100 000-byte payload
//! spelled as a byte list is already ~400 KiB of text — which is also why the round-trip
//! check compares **inside** the oracle and prints one boolean per case instead of
//! echoing the decompressed payloads back.
//!
//! It is **fail-loud**: with no usable `erl`/`erlc` it prints a loud banner and returns
//! (that is *not* a pass), and every step that runs the oracle panics if the oracle
//! errors or prints the wrong number of results.
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{bitstring_bytes, make_binary};
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

/// A registry + heap + atom table the test drives BIF calls through.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    /// No optional seam is installed: none of these BIFs needs one.
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "hello"]);
        Fixture {
            registry: default_registry(),
            // The corpus holds ~244 KiB of payloads and every stream is on-heap, so the
            // arena sees a few megabytes of terms over the run.
            heap: Heap::new(1 << 22).expect("test heap"),
            atoms,
        }
    }

    /// Calls `module:function(Args)` through the real registry.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call(module, function, args.len() as u32, &mut ctx, args)
    }

    /// Calls `module:function(Args)` and unwraps the result.
    fn ok(&mut self, module: &str, function: &str, args: &[Eterm]) -> Eterm {
        self.call(module, function, args)
            .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
    }

    /// Builds the binary `<<bytes>>` on this fixture's heap.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        make_binary(&mut self.heap, bytes).expect("the binary fits")
    }

    /// The bytes of a byte-aligned binary result.
    fn bytes_of(&self, term: Eterm) -> Vec<u8> {
        let (bytes, offset, size_bits) = bitstring_bytes(term).expect("a bitstring result");
        assert_eq!(offset, 0, "the result is a whole-byte binary");
        assert_eq!(size_bits % 8, 0, "the result is whole-byte aligned");
        bytes[..size_bits / 8].to_vec()
    }

    /// `zlib:compress(Data)` through the registry.
    fn compress(&mut self, data: &[u8]) -> Vec<u8> {
        let term = self.binary(data);
        let packed = self.ok("zlib", "compress", &[term]);
        self.bytes_of(packed)
    }

    /// `zlib:uncompress(Data)` through the registry.
    fn uncompress(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let term = self.binary(data);
        let plain = self.call("zlib", "uncompress", &[term])?;
        Ok(self.bytes_of(plain))
    }

    /// `zlib:zip(Data)` through the registry.
    fn zip(&mut self, data: &[u8]) -> Vec<u8> {
        let term = self.binary(data);
        let zipped = self.ok("zlib", "zip", &[term]);
        self.bytes_of(zipped)
    }

    /// `zlib:unzip(Data)` through the registry.
    fn unzip(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let term = self.binary(data);
        let plain = self.call("zlib", "unzip", &[term])?;
        Ok(self.bytes_of(plain))
    }

    /// `zlib:gzip(Data)` through the registry.
    fn gzip(&mut self, data: &[u8]) -> Vec<u8> {
        let term = self.binary(data);
        let packed = self.ok("zlib", "gzip", &[term]);
        self.bytes_of(packed)
    }

    /// `zlib:gunzip(Data)` through the registry.
    fn gunzip(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let term = self.binary(data);
        let plain = self.call("zlib", "gunzip", &[term])?;
        Ok(self.bytes_of(plain))
    }
}

// ---------------------------------------------------------------------------
// The corpus: a fixed length × shape matrix, generated deterministically
// ---------------------------------------------------------------------------

/// The shapes a payload can take. Each one makes deflate take a different path:
/// `Zeros` finds maximum-distance matches, `Abc`/`Text` are ordinary text, `Random` is
/// incompressible (stored blocks), and `Mixed` alternates quiet islands with random noise
/// (a stored block *between* dynamic ones).
#[derive(Clone, Copy)]
enum Shape {
    Zeros,
    Abc,
    Text,
    Random,
    Mixed,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Shape::Zeros => "zeros",
            Shape::Abc => "abc",
            Shape::Text => "text",
            Shape::Random => "random",
            Shape::Mixed => "mixed",
        }
    }
}

/// Every shape is applied to every one of these lengths: the empty body, sub-literal
/// sizes, the 1 KiB fixed-Huffman/dynamic-Huffman boundary region, and a multi-block
/// body.
const SMALL_LENGTHS: [usize; 7] = [0, 1, 3, 10, 100, 1024, 8192];

const ALL_SHAPES: [Shape; 5] = [
    Shape::Zeros,
    Shape::Abc,
    Shape::Text,
    Shape::Random,
    Shape::Mixed,
];

/// The lengths that do not get the whole shape matrix, each with the shape that exercises
/// its interesting path. `32767`/`32768`/`32769` straddle `MAX_DIST` (32 KiB — the point
/// where a match distance stops fitting the 15-bit window), so they are **mandatory**;
/// `100000` is the largest body and `512`/`4096`/`5` fill the gaps between the small
/// lengths.
const EXTRA_CASES: [(Shape, usize); 7] = [
    (Shape::Zeros, 32767),
    (Shape::Random, 32768),
    (Shape::Mixed, 32769),
    (Shape::Random, 100000),
    (Shape::Mixed, 512),
    (Shape::Zeros, 4096),
    (Shape::Text, 5),
];

/// One corpus entry: a named generator plus the payload it produced.
struct Case {
    shape: Shape,
    payload: Vec<u8>,
}

impl Case {
    fn new(shape: Shape, len: usize) -> Case {
        Case {
            shape,
            payload: generate(shape, len),
        }
    }

    /// `shape [length]` — what the mismatch diagnostics and assertions name.
    fn label(&self) -> String {
        format!("{} [{}]", self.shape.name(), self.payload.len())
    }
}

/// The whole corpus, in a fixed order: the shape matrix first, then the extras.
fn corpus() -> Vec<Case> {
    let mut cases = Vec::new();
    for len in SMALL_LENGTHS {
        for shape in ALL_SHAPES {
            cases.push(Case::new(shape, len));
        }
    }
    for (shape, len) in EXTRA_CASES {
        cases.push(Case::new(shape, len));
    }
    cases
}

/// The English-ish filler `Shape::Text` repeats.
const TEXT: &[u8] = b"The quick brown fox jumps over the lazy dog, and then the dog \
                      barks at the fox while the brown cow watches from the meadow. ";

/// The block size `Shape::Mixed` alternates with: every third 64-byte block is zeros.
const MIXED_BLOCK: usize = 64;

/// `pattern` repeated to exactly `len` bytes.
fn cycle(pattern: &[u8], len: usize) -> Vec<u8> {
    pattern.iter().copied().cycle().take(len).collect()
}

/// xorshift32 — the corpus's whole PRNG, so the payloads need no dependency and are
/// identical on every run and platform.
fn xorshift32(state: &mut u32) -> u32 {
    let mut word = *state;
    word ^= word << 13;
    word ^= word >> 17;
    word ^= word << 5;
    *state = word;
    word
}

/// The xorshift seed for a payload of `len` bytes — non-zero (a zero state is a fixed
/// point) and distinct for every length in this corpus, so no two lengths share a byte
/// stream.
fn seed_for(len: usize) -> u32 {
    0x9e37_79b9 ^ (len as u32).wrapping_mul(0x85eb_ca6b)
}

/// An endless pseudo-random byte stream (the little-endian bytes of successive xorshift32
/// words).
fn pseudo_random_bytes(seed: u32) -> impl Iterator<Item = u8> {
    let mut state = seed;
    std::iter::from_fn(move || Some(xorshift32(&mut state).to_le_bytes())).flatten()
}

/// The payload for `shape` at exactly `len` bytes.
fn generate(shape: Shape, len: usize) -> Vec<u8> {
    match shape {
        Shape::Zeros => vec![0u8; len],
        Shape::Abc => cycle(b"abc", len),
        Shape::Text => cycle(TEXT, len),
        Shape::Random => pseudo_random_bytes(seed_for(len)).take(len).collect(),
        Shape::Mixed => {
            let mut random = pseudo_random_bytes(seed_for(len));
            (0..len)
                .map(|index| {
                    if (index / MIXED_BLOCK).is_multiple_of(3) {
                        0
                    } else {
                        random.next().expect("an endless byte stream")
                    }
                })
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// A `$TMPDIR` scratch directory for the payload and stream files the oracle reads.
///
/// Removed on drop (so a panicking assertion still cleans up); a failed removal is
/// reported, never hidden.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "beam_bifs_zlib_oracle_{}_{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
        Scratch { dir }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, bytes)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the same
/// resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs zlib_oracle] $ERTS_ORACLE='{}' is not an existing executable \
             — treating it as no oracle.",
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

/// Runs one `erl -noshell -eval <eval> -s init stop` and returns its stdout.
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

/// `path` as an Erlang string literal (backslash and quote escaped).
fn erl_string(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// One `[b, b, …]` byte list per line, as `io:format("~w", [binary_to_list(Bin)])`
/// prints it.
fn parse_byte_lists(stdout: &str) -> Vec<Vec<u8>> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let inner = line.trim().trim_start_matches('[').trim_end_matches(']');
            if inner.is_empty() {
                return Vec::new();
            }
            inner
                .split(',')
                .map(|field| {
                    field
                        .trim()
                        .parse::<u8>()
                        .unwrap_or_else(|e| panic!("`{field}` in `{line}` is not a byte: {e}"))
                })
                .collect()
        })
        .collect()
}

/// `zlib:<function>/1` over every payload file, in **one** oracle invocation.
fn oracle_encode(oracle: &Path, function: &str, payloads: &[PathBuf]) -> Vec<Vec<u8>> {
    let mut eval = String::from("Ps = [");
    for (index, path) in payloads.iter().enumerate() {
        if index > 0 {
            eval.push(',');
        }
        eval.push_str(&erl_string(path));
    }
    write!(
        eval,
        "], [begin {{ok,B}} = file:read_file(P), \
         io:format(\"~w~n\", [binary_to_list(zlib:{function}(B))]) end || P <- Ps], halt()."
    )
    .expect("writing to a String cannot fail");
    let stdout = run_erl(oracle, &eval);
    let streams = parse_byte_lists(&stdout);
    assert_eq!(
        streams.len(),
        payloads.len(),
        "the oracle printed {} zlib:{function}/1 streams for {} payloads:\n{stdout}",
        streams.len(),
        payloads.len()
    );
    streams
}

/// Direction 1: the oracle decodes **this crate's** streams and compares them with the
/// input, one oracle invocation for all three decoders.
///
/// Returns one `(uncompress, unzip, gunzip)` triple per case.
fn oracle_recovers_inputs(
    oracle: &Path,
    scratch: &Scratch,
    payload_paths: &[PathBuf],
    ours_compress: &[Vec<u8>],
    ours_zip: &[Vec<u8>],
    ours_gzip: &[Vec<u8>],
) -> Vec<(bool, bool, bool)> {
    let mut eval = String::from("Files = [");
    for (index, payload) in payload_paths.iter().enumerate() {
        if index > 0 {
            eval.push(',');
        }
        let compress = scratch.write(&format!("ours_c_{index}.bin"), &ours_compress[index]);
        let zip = scratch.write(&format!("ours_z_{index}.bin"), &ours_zip[index]);
        let gzip = scratch.write(&format!("ours_g_{index}.bin"), &ours_gzip[index]);
        write!(
            eval,
            "{{{},{},{},{}}}",
            erl_string(payload),
            erl_string(&compress),
            erl_string(&zip),
            erl_string(&gzip)
        )
        .expect("writing to a String cannot fail");
    }
    // No `try`/`catch` here on purpose: a second `try … catch error:R -> R end` in one
    // `-eval` string re-binds `R` (the first binding leaks out of the `try`), which the
    // oracle's `-eval` parser rejects. A boolean per case needs neither.
    eval.push_str(
        "], [begin \
           {ok,P} = file:read_file(I), \
           {ok,C} = file:read_file(Cp), \
           {ok,Z} = file:read_file(Zp), \
           {ok,G} = file:read_file(Gp), \
           io:format(\"~p ~p ~p~n\", \
                     [zlib:uncompress(C) =:= P, zlib:unzip(Z) =:= P, zlib:gunzip(G) =:= P]) \
         end || {I,Cp,Zp,Gp} <- Files], halt().",
    );
    let stdout = run_erl(oracle, &eval);
    let verdicts: Vec<(bool, bool, bool)> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let mut parsed = fields.iter().map(|field| match *field {
                "true" => true,
                "false" => false,
                other => panic!("`{other}` in `{line}` is not an Erlang boolean"),
            });
            let triple = (
                parsed.next().expect("a `zlib:uncompress/1` verdict"),
                parsed.next().expect("a `zlib:unzip/1` verdict"),
                parsed.next().expect("a `zlib:gunzip/1` verdict"),
            );
            assert!(
                parsed.next().is_none(),
                "`{line}` carries more than three verdicts"
            );
            triple
        })
        .collect();
    assert_eq!(
        verdicts.len(),
        payload_paths.len(),
        "the oracle printed {} round-trip verdicts for {} payloads:\n{stdout}",
        verdicts.len(),
        payload_paths.len()
    );
    verdicts
}

/// Prints the loud "no oracle, not a pass" banner.
fn skip_banner() {
    eprintln!();
    eprintln!("=========== beam_bifs zlib_oracle: live oracle SKIPPED ===========");
    eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
    eprintln!("executable, and no `erl` on $PATH).");
    eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
    eprintln!("Byte identity of zlib:compress/1, zlib:zip/1 and zlib:gzip/1 is then");
    eprintln!("UNVERIFIED, as are both round-trip directions.");
    eprintln!("==================================================================");
    eprintln!();
}

/// Panics with everything needed to diagnose a byte difference without rerunning.
fn assert_same_bytes(function: &str, case: &Case, ours: &[u8], oracle: &[u8]) {
    if ours == oracle {
        return;
    }
    let payload = &case.payload;
    let prefix = &payload[..payload.len().min(16)];
    panic!(
        "{function} differs from the oracle for input {} ({} bytes; first 16 = {prefix:02x?})\n\
         \x20 this crate: {} bytes\n\
         \x20 oracle:     {} bytes\n\
         \x20 this crate = {ours:?}\n\
         \x20 oracle     = {oracle:?}",
        case.label(),
        payload.len(),
        ours.len(),
        oracle.len(),
    );
}

/// Direction 2's decode, with the case named on failure.
fn decoded(what: &str, case: &Case, result: Result<Vec<u8>>) -> Vec<u8> {
    match result {
        Ok(bytes) => bytes,
        Err(error) => panic!(
            "{what} of the oracle's stream for input {} failed: {error:?}",
            case.label()
        ),
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// Byte identity for all three encoders, and both round-trip directions, against the live
/// oracle — over the bounded corpus. See the module doc for what is pinned and why the
/// payloads travel as files.
#[test]
fn the_bounded_corpus_is_byte_identical_to_the_live_oracle_both_ways() {
    let Some(oracle) = resolve_oracle() else {
        skip_banner();
        return;
    };

    let cases = corpus();
    let total: usize = cases.iter().map(|case| case.payload.len()).sum();
    assert!(
        (40..=60).contains(&cases.len()),
        "the corpus stays bounded at 40..=60 inputs, not {}",
        cases.len()
    );
    assert!(
        total <= 256 * 1024,
        "the corpus stays bounded at 256 KiB, not {total} bytes"
    );

    let scratch = Scratch::new("corpus");
    let payload_paths: Vec<PathBuf> = cases
        .iter()
        .enumerate()
        .map(|(index, case)| scratch.write(&format!("in_{index}.bin"), &case.payload))
        .collect();

    // ---- direction 0: the encoders' bytes, one oracle invocation per encoder.
    let oracle_compress = oracle_encode(&oracle, "compress", &payload_paths);
    let oracle_zip = oracle_encode(&oracle, "zip", &payload_paths);
    let oracle_gzip = oracle_encode(&oracle, "gzip", &payload_paths);

    let mut fx = Fixture::new();
    let mut ours_compress = Vec::with_capacity(cases.len());
    let mut ours_zip = Vec::with_capacity(cases.len());
    let mut ours_gzip = Vec::with_capacity(cases.len());
    let mut stream_bytes = (0usize, 0usize, 0usize);
    for (index, case) in cases.iter().enumerate() {
        let compress = fx.compress(&case.payload);
        assert_same_bytes("zlib:compress/1", case, &compress, &oracle_compress[index]);
        stream_bytes.0 += compress.len();
        ours_compress.push(compress);

        let zip = fx.zip(&case.payload);
        assert_same_bytes("zlib:zip/1", case, &zip, &oracle_zip[index]);
        stream_bytes.1 += zip.len();
        ours_zip.push(zip);

        let gzip = fx.gzip(&case.payload);
        assert_same_bytes("zlib:gzip/1", case, &gzip, &oracle_gzip[index]);
        stream_bytes.2 += gzip.len();
        ours_gzip.push(gzip);
    }

    // ---- direction 2: this crate decodes the oracle's streams.
    for (index, case) in cases.iter().enumerate() {
        assert_eq!(
            decoded(
                "zlib:uncompress/1",
                case,
                fx.uncompress(&oracle_compress[index])
            ),
            case.payload,
            "zlib:uncompress/1 must inflate the oracle's zlib:compress/1 stream back for {}",
            case.label()
        );
        assert_eq!(
            decoded("zlib:unzip/1", case, fx.unzip(&oracle_zip[index])),
            case.payload,
            "zlib:unzip/1 must inflate the oracle's zlib:zip/1 stream back for {}",
            case.label()
        );
        assert_eq!(
            decoded("zlib:gunzip/1", case, fx.gunzip(&oracle_gzip[index])),
            case.payload,
            "zlib:gunzip/1 must inflate the oracle's zlib:gzip/1 stream back for {}",
            case.label()
        );
    }

    // ---- direction 1: the oracle decodes this crate's streams.
    let recovered = oracle_recovers_inputs(
        &oracle,
        &scratch,
        &payload_paths,
        &ours_compress,
        &ours_zip,
        &ours_gzip,
    );
    for (case, (uncompress, unzip, gunzip)) in cases.iter().zip(&recovered) {
        assert!(
            *uncompress,
            "the oracle's zlib:uncompress/1 must recover {} from this crate's \
             zlib:compress/1 stream",
            case.label()
        );
        assert!(
            *unzip,
            "the oracle's zlib:unzip/1 must recover {} from this crate's zlib:zip/1 stream",
            case.label()
        );
        assert!(
            *gunzip,
            "the oracle's zlib:gunzip/1 must recover {} from this crate's zlib:gzip/1 stream",
            case.label()
        );
    }

    let inputs = cases.len();
    println!(
        "zlib_oracle: {inputs} inputs / {total} payload bytes — byte identity \
         compress {inputs}/{inputs} ({} B), zip {inputs}/{inputs} ({} B), \
         gzip {inputs}/{inputs} ({} B); round trips both ways {}/{} per decoder",
        stream_bytes.0,
        stream_bytes.1,
        stream_bytes.2,
        recovered.len(),
        inputs,
    );
}
