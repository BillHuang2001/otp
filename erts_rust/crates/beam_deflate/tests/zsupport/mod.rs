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
//! Shared helpers for the `beam_deflate` integration tests: hex conversion, deterministic
//! test corpora, and the **oracle** — the system `erl`'s `zlib` module.
//!
//! The oracle is invoked in batches (one `erl` boot for a whole matrix of cases) because
//! booting the runtime costs a couple hundred milliseconds. When `erl` is not on `PATH`
//! every helper reports that and the caller prints `SKIP …` and returns, so CI without an
//! Erlang installation still stays green.

#![allow(dead_code)]

use std::io::Write;
use std::process::Command;

/// Lowercase hex, the form `binary:encode_hex/1` produces and `binary:decode_hex/1` eats.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 0xf), 16).unwrap_or('0'));
    }
    s
}

/// Decode lowercase or uppercase hex.
pub fn unhex(s: &str) -> Vec<u8> {
    let digits: Vec<u8> = s
        .bytes()
        .filter(|b| b.is_ascii_hexdigit())
        .map(|b| match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => b - b'A' + 10,
        })
        .collect();
    digits.chunks(2).map(|p| (p[0] << 4) | p[1]).collect()
}

/// One `deflateInit2` + `deflate(Z_FINISH)` case: the full parameter tuple plus input.
#[derive(Clone, Debug)]
pub struct Case {
    /// `deflateInit2`'s level (`-1` is `default`).
    pub level: i8,
    /// `deflateInit2`'s `windowBits`: `8..=15`, `-15..=-8`, or `16 + 8 ..= 16 + 15`.
    pub window_bits: i8,
    /// `deflateInit2`'s `memLevel`.
    pub mem_level: u8,
    /// `Z_DEFAULT_STRATEGY` (0) … `Z_RLE` (3). `Z_FIXED` (4) is not expressible through
    /// the `zlib` module, so it can never appear in an oracle case.
    pub strategy: u8,
    /// The uncompressed bytes.
    pub data: Vec<u8>,
}

impl Case {
    /// A zlib-wrapped (`windowBits` 15) case at `level`.
    pub fn zlib(level: i8, data: &[u8]) -> Case {
        Case {
            level,
            window_bits: 15,
            mem_level: 8,
            strategy: 0,
            data: data.to_vec(),
        }
    }

    /// A raw (`windowBits` -15) case at `level`.
    pub fn raw(level: i8, data: &[u8]) -> Case {
        Case {
            level,
            window_bits: -15,
            ..Case::zlib(level, data)
        }
    }

    /// A gzip (`windowBits` 16 + 15) case at `level`.
    pub fn gzip(level: i8, data: &[u8]) -> Case {
        Case {
            level,
            window_bits: 16 + 15,
            ..Case::zlib(level, data)
        }
    }
}

/// The name of the system Erlang runtime, honouring the `$ERTS_ORACLE` override the
/// workspace's differential harness also uses.
fn erl_program() -> String {
    std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string())
}

/// Is there a usable `erl` on this machine?
pub fn have_oracle() -> bool {
    Command::new(erl_program())
        .args(["-noshell", "-eval", "halt(0)."])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The `-eval` script the batch oracle runs: one line per case on stdin (a file named by
/// `$BD_SPEC`), one line of hex per case on stdout.
const BATCH_SCRIPT: &str = concat!(
    "{ok,B}=file:read_file(os:getenv(\"BD_SPEC\")),",
    "Ls=[L||L<-binary:split(B,<<\"\\n\">>,[global]),L=/=<<>>],",
    "St=fun(S)->element(S+1,{default,filtered,huffman_only,rle}) end,",
    "[begin ",
    "[Lv,Wv,Mv,Sv,H]=binary:split(L,<<\",\">>,[global]),",
    "Z=zlib:open(),",
    "ok=zlib:deflateInit(Z,binary_to_integer(Lv),deflated,binary_to_integer(Wv),",
    "binary_to_integer(Mv),St(binary_to_integer(Sv))),",
    "O=iolist_to_binary(zlib:deflate(Z,binary:decode_hex(H),finish)),",
    "ok=zlib:deflateEnd(Z),zlib:close(Z),",
    "io:format(\"~s~n\",[binary:encode_hex(O)]) ",
    "end||L<-Ls],",
    "halt()."
);

/// Run a whole matrix through the oracle in **one** `erl` boot.
///
/// `None` means "no oracle available" (the caller should print a SKIP line); `Some` holds
/// one compressed stream per case, in order. A case the oracle rejects is reported as the
/// (empty) `Err` variant so a caller can assert symmetric rejection.
pub fn oracle_batch(cases: &[Case]) -> Option<Vec<Result<Vec<u8>, String>>> {
    if !have_oracle() {
        return None;
    }
    let mut spec = String::new();
    for c in cases {
        spec.push_str(&format!(
            "{},{},{},{},{}\n",
            c.level,
            c.window_bits,
            c.mem_level,
            c.strategy,
            hex(&c.data)
        ));
    }
    let dir = std::env::temp_dir().join(format!("beam_deflate_oracle_{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("spec.csv");
    {
        let mut f = std::fs::File::create(&path).ok()?;
        f.write_all(spec.as_bytes()).ok()?;
    }
    let out = Command::new(erl_program())
        .args(["-noshell", "-eval", BATCH_SCRIPT])
        .env("BD_SPEC", &path)
        .output()
        .ok()?;
    std::fs::remove_dir_all(&dir).ok();

    // A case the oracle rejected aborts the whole script, so a non-empty exit means the
    // batch is unusable; callers that want rejection symmetry spell those cases out
    // individually.
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    if lines.len() != cases.len() {
        return None;
    }
    Some(
        lines
            .iter()
            .map(|l| Ok(unhex(l)))
            .collect::<Vec<Result<Vec<u8>, String>>>(),
    )
}

/// Run an arbitrary `-eval` script and return its stdout, or `None` when there is no
/// oracle (or it failed).
pub fn erl_eval(script: &str) -> Option<String> {
    if !have_oracle() {
        return None;
    }
    let out = Command::new(erl_program())
        .args(["-noshell", "-eval", script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// `zlib:uncompress/1` in the oracle: the inverse check for a stream the oracle itself
/// cannot produce (the `Z_FIXED` strategy).
pub fn oracle_inflate(stream: &[u8]) -> Option<Vec<u8>> {
    let script = format!(
        concat!(
            "Z=zlib:open(),",
            "ok=zlib:inflateInit(Z),",
            // `zlib:inflate/2` can return before Z_STREAM_END when its internal output
            // buffer fills, so keep passing `finish` until `inflate` reports `finished`.
            "L=fun F(In, Acc) ->",
            "    case zlib:inflate(Z, In, finish) of",
            "        {{finished, Out}} -> iolist_to_binary(lists:reverse([Out|Acc]));",
            "        Out -> F(<<>>, [Out|Acc])",
            "    end end,",
            "O=L(binary:decode_hex(<<\"{}\">>), []),",
            "ok=zlib:inflateEnd(Z),zlib:close(Z),",
            "io:format(\"~s\",[binary:encode_hex(O)]),",
            "halt()."
        ),
        hex(stream)
    );
    erl_eval(&script).map(|s| unhex(&s))
}

/// The inputs the golden matrix uses. Small enough to inline the expected bytes, varied
/// enough to exercise every branch of the block emitter: an empty block, a literal-only
/// body, runs (which `Z_RLE` and `deflate_slow`'s lazy path treat differently), and a
/// few KiB of mixed repetitive text (which spans several blocks and slides the window).
pub fn corpus_small() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("abc", b"abc".to_vec()),
        ("zeros16", vec![0u8; 16]),
        ("a300", vec![b'a'; 300]),
        ("mixed", mixed_kib()),
    ]
}

/// A deterministically generated few KiB of mixed, partly repetitive bytes.
///
/// Repetition matters more than randomness here: it is what makes the level-0 stored
/// path, the greedy path and the lazy path all emit *different* streams, so a golden per
/// level actually distinguishes the levels.
pub fn mixed_kib() -> Vec<u8> {
    let words: [&[u8]; 8] = [
        b"the ", b"quick ", b"brown ", b"fox ", b"jumps ", b"over ", b"lazy ", b"dog ",
    ];
    let mut out = Vec::with_capacity(4096);
    let mut state = 0x1234_5678u32;
    while out.len() < 4096 {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let w = words[(state >> 16) as usize % words.len()];
        out.extend_from_slice(w);
        if state & 0x10 != 0 {
            out.extend_from_slice(&[0u8; 3]);
        }
    }
    out.truncate(4096);
    out
}
