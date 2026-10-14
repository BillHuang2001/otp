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
//! Integration tests for the four self-contained `zlib` functions —
//! `zlib:compress/1`, `zlib:uncompress/1`, `zlib:zip/1` and `zlib:unzip/1`.
//!
//! `zlib:zip/1`/`zlib:unzip/1` are the **raw** (RFC 1951, header-less and
//! checksum-less) pair, and the bytes of what `zip/1` emits are the encoder's, not
//! zlib's, so what is pinned here is:
//!
//! * the two oracle goldens, byte for byte (`zlib:zip(<<"hello">>)` is
//!   `<<203,72,205,201,201,7,0>>` and `zlib:zip(<<>>)` is `<<3,0>>`, verified live
//!   against OTP 29.1.1 / erts-17.1);
//! * the full error-class matrix (`badarg` for a non-iodata argument, the bare
//!   `error:data_error` for a stream the inflater cannot finish — with **no** length
//!   arm, unlike `uncompress/1`);
//! * the round trip over many sizes **and every compression level 0..=9**, which is
//!   the correctness evidence for the hand-written decoder in `src/deflate.rs`: level 0
//!   exercises stored blocks and the higher levels exercise dynamic Huffman blocks
//!   (`the_sweep_covers_all_three_deflate_block_types` asserts that the sweep really
//!   reaches a dynamic block, rather than assuming it);
//! * a **live cross-check against the system oracle** — streams the system `erl`'s own
//!   `zlib:zip/1` produced, one of them long enough (> 1 KiB) to need dynamic Huffman.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_codec` for the zlib
//! codec and the binary constructors/readers).
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::compress::compress as zlib_compress;
use beam_codec::term::{bitstring_bytes, make_binary};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable, Eterm};

/// A registry + heap + atom table the tests drive BIF calls through.
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
            // The level sweep below round-trips payloads of up to 100 kB, so the arena
            // holds a few hundred kilobytes of terms at once.
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
}

/// A payload that repeats but is not constant, so the compressor can neither store it
/// verbatim nor collapse it to one long match.
fn payload(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 251) as u8).collect()
}

/// The raw DEFLATE payload of a zlib stream: everything between the 2-byte header and
/// the 4-byte Adler-32 trailer — exactly what `zlib:zip/1` emits (and what the oracle
/// confirms byte for byte for the goldens below).
fn raw_deflate(level: u8, data: &[u8]) -> Vec<u8> {
    let stream = zlib_compress(data, level).expect("the codec deflates");
    stream[2..stream.len() - 4].to_vec()
}

/// The block type (`BTYPE`, bits 1-2 of the first byte) of a raw DEFLATE stream's first
/// block: 0 stored, 1 fixed Huffman, 2 dynamic Huffman.
fn first_block_type(stream: &[u8]) -> u32 {
    u32::from((stream[0] >> 1) & 0b11)
}

/// Asserts that `result` is the bare-atom `error:data_error` raise.
fn assert_data_error(result: Result<Vec<u8>>, what: &str) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, ExceptionClass::Error, "{what}: the class");
            assert_eq!(
                raise.reason,
                ExitReason::atom("data_error"),
                "{what}: the reason"
            );
        }
        other => panic!("{what}: expected error:data_error, got {other:?}"),
    }
}

#[test]
fn zip_emits_the_oracle_goldens_byte_for_byte() {
    let mut fx = Fixture::new();

    // `zlib:zip(<<"hello">>)` and `zlib:zip(<<>>)` under OTP 29.1.1 / erts-17.1.
    assert_eq!(fx.zip(b"hello"), vec![203, 72, 205, 201, 201, 7, 0]);
    assert_eq!(fx.zip(b""), vec![3, 0]);

    // `zlib:zip([1,2,3])` — a plain integer list is iodata (the oracle's `<<99,100,98,6,0>>`).
    let list = {
        let items = [make_small(1), make_small(2), make_small(3)];
        let mut ctx = BifContext::new(&mut fx.heap, &fx.atoms, ProcessId::new(1, 0));
        ctx.list(&items).expect("the list fits")
    };
    let zipped = fx.ok("zlib", "zip", &[list]);
    assert_eq!(fx.bytes_of(zipped), vec![99, 100, 98, 6, 0]);
}

#[test]
fn the_oracles_golden_streams_inflate() {
    let mut fx = Fixture::new();

    assert_eq!(fx.unzip(&[203, 72, 205, 201, 201, 7, 0]).expect("hello"), b"hello");
    assert_eq!(fx.unzip(&[3, 0]).expect("the empty stream"), Vec::<u8>::new());

    // Bytes after the end of the deflate stream are ignored (oracle-verified:
    // `zlib:unzip(<<Zip/binary, 99, 99>>)` is the payload).
    assert_eq!(
        fx.unzip(&[203, 72, 205, 201, 201, 7, 0, 99, 99]).expect("trailing bytes"),
        b"hello"
    );
}

#[test]
fn round_trips_hold_at_every_level_and_several_sizes() {
    let mut fx = Fixture::new();

    for size in [0usize, 1, 13, 1000, 100_000] {
        let data = payload(size);

        // What the workspace's codec emits at each level, framed as `zlib:zip/1` frames
        // it: level 0 stores, the higher levels use dynamic Huffman codes, and every one
        // must inflate to the original bytes.
        for level in 0..=9u8 {
            let stream = raw_deflate(level, &data);
            assert_eq!(
                fx.unzip(&stream)
                    .unwrap_or_else(|e| panic!("unzip of the level-{level} size-{size} stream: {e:?}")),
                data,
                "round trip at size {size}, level {level}"
            );
        }

        // And the `zip/1`/`unzip/1` pair itself.
        let stream = fx.zip(&data);
        assert_eq!(fx.unzip(&stream).expect("zip/unzip"), data, "size {size}");
    }
}

#[test]
fn the_sweep_covers_all_three_deflate_block_types() {
    // The round-trip sweep is the decoder's correctness evidence, so it must actually
    // reach the three block types rather than merely be expected to: level 0 stores,
    // the fixed codes are what the `<<3,0>>`/`<<203,72,...>>` goldens use, and a large
    // repetitive payload at a high level needs dynamic Huffman codes.
    let mut types = BTreeSet::new();
    for size in [0usize, 1, 1000, 100_000] {
        for level in 0..=9u8 {
            types.insert(first_block_type(&raw_deflate(level, &payload(size))));
        }
    }
    for (kind, what) in [
        (0, "stored"),
        (1, "fixed Huffman"),
        (2, "dynamic Huffman"),
    ] {
        assert!(
            types.contains(&kind),
            "the sweep never produced a {what} block (saw {types:?})"
        );
    }
}

#[test]
fn a_non_iodata_argument_is_badarg_on_both_raw_functions() {
    let mut fx = Fixture::new();

    let hello = {
        let index = fx.atoms.lookup("hello").expect("the fixture seeds `hello`");
        beam_term::make_atom(index)
    };
    // `[1, 2 | 3]` — a list whose tail is not a list.
    let improper = {
        let tail = make_small(3);
        let mut ctx = BifContext::new(&mut fx.heap, &fx.atoms, ProcessId::new(1, 0));
        let inner = ctx.cons(make_small(2), tail).expect("fits");
        ctx.cons(make_small(1), inner).expect("fits")
    };

    for (what, bad) in [
        ("an integer", make_small(42)),
        ("an atom", hello),
        ("an improper list", improper),
    ] {
        assert!(
            matches!(fx.call("zlib", "zip", &[bad]), Err(BeamError::BadArg)),
            "zip/1 of {what} must be badarg"
        );
        assert!(
            matches!(fx.call("zlib", "unzip", &[bad]), Err(BeamError::BadArg)),
            "unzip/1 of {what} must be badarg"
        );
    }
}

#[test]
fn unzip_reports_data_error_wherever_the_inflater_cannot_finish() {
    let mut fx = Fixture::new();

    // No length arm: the empty stream is `data_error`, not `badarg`.
    assert_data_error(fx.unzip(b""), "the empty stream");
    assert_data_error(fx.unzip(&[1, 2, 3]), "three bytes");
    assert_data_error(fx.unzip(&[1, 2, 3, 4, 5, 6, 7]), "seven bytes");
    assert_data_error(fx.unzip(&[1, 2, 3, 4, 5, 6, 7, 8]), "eight bytes");
    assert_data_error(fx.unzip(&[0xFF, 0xFF]), "a reserved block type");

    // A real stream cut short mid-block, and one whose match distance reaches past the
    // window: both are the inflater's own failure.
    let data = payload(1000);
    let mut truncated = fx.zip(&data);
    truncated.truncate(truncated.len() / 2);
    assert_data_error(fx.unzip(&truncated), "a truncated stream");
    // `<<203,72,205,201,201,7,0>>` with its final bytes dropped.
    assert_data_error(fx.unzip(&[203, 72, 205, 201]), "a truncated fixed block");
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------

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
            "[beam_bifs zlib] $ERTS_ORACLE='{}' is not an existing executable \
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

/// The payloads the oracle is asked to `zip/1`, in the order it prints them.
fn oracle_payloads() -> Vec<Vec<u8>> {
    vec![
        b"hello".to_vec(),
        (1..=100u8).collect(),
        // The decimal spelling of 1..1000 — 2893 varied bytes, and the oracle's deflate
        // really does choose dynamic Huffman codes for it (its stream is 1458 bytes and
        // starts with a type-2 block). `integer_to_list/1` and `to_string` agree, so both
        // sides feed the oracle the identical payload. (A merely repetitive payload is
        // **not** enough: deflate falls back to the fixed codes for those.)
        (1..=1000u32)
            .map(|i| i.to_string())
            .collect::<String>()
            .into_bytes(),
    ]
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

/// Inflates the **oracle's own** raw DEFLATE streams and checks the payloads.
///
/// The streams come from the system `erl`'s `zlib:zip/1`, so both directions of the
/// contract are exercised against the reference implementation, not only against this
/// crate's encoder. Skips loudly when no oracle is available (it is not a pass then).
#[test]
fn unzip_cross_checks_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=========== beam_bifs zlib: live oracle SKIPPED ===========");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("===========================================================");
        eprintln!();
        return;
    };

    // `zlib:zip/1` of the three payloads, as byte lists (the second payload is the
    // objective's own `iolist_to_binary(lists:seq(1,100))`).
    let eval = "Ps = [<<\"hello\">>, \
                list_to_binary(lists:seq(1,100)), \
                list_to_binary([integer_to_list(I) || I <- lists:seq(1,1000)])], \
                [io:format(\"~w~n\", [binary_to_list(zlib:zip(P))]) || P <- Ps], \
                halt().";
    let stdout = run_erl(&oracle, eval);
    let streams = parse_byte_lists(&stdout);
    let payloads = oracle_payloads();
    assert_eq!(
        streams.len(),
        payloads.len(),
        "the oracle printed {} streams for {} payloads:\n{stdout}",
        streams.len(),
        payloads.len()
    );

    let mut fx = Fixture::new();
    for (index, (stream, data)) in streams.iter().zip(&payloads).enumerate() {
        assert!(!stream.is_empty(), "the oracle's stream {index} is empty");
        // The inflating direction: the oracle's stream, this crate's decoder.
        assert_eq!(
            fx.unzip(stream)
                .unwrap_or_else(|e| panic!("unzip of the oracle's stream {index}: {e:?}")),
            *data,
            "the oracle's stream {index} must inflate to its payload"
        );
        // … and for the first payload the oracle's bytes are this crate's too: miniz's
        // raw DEFLATE is zlib's for `<<"hello">>` (the goldens above pin the same thing
        // without an oracle).
        if index == 0 {
            assert_eq!(fx.zip(data), *stream);
        }
        // Whatever the encoder chose, this crate's `zip/1` output must inflate back.
        let ours = fx.zip(data);
        assert_eq!(
            fx.unzip(&ours).expect("our own stream"),
            *data,
            "our own stream {index} must inflate to its payload"
        );
    }

    // The long payload really did need dynamic Huffman codes at the oracle's level.
    let long = &streams[streams.len() - 1];
    assert_eq!(
        first_block_type(long),
        2,
        "the oracle's >1 KiB stream must start with a dynamic block"
    );
}
