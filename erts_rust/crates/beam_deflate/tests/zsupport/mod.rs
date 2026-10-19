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

/// Integration tests in one test binary run in parallel threads, but each oracle call
/// boots a whole `erl`; on a loaded host several boots at once fail sporadically (the
/// runner reports "the oracle batch failed to run"). One oracle call at a time.
pub fn oracle_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

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
    let _guard = oracle_lock();
    have_oracle_locked()
}

/// [`have_oracle`] for a caller that already holds [`oracle_lock`].
fn have_oracle_locked() -> bool {
    Command::new(erl_program())
        .args(["-noshell", "-eval", "halt(0)."])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The `-eval` script the batch oracle runs: one line per case on stdin (a file named by
/// `$BD_SPEC`), one line per case on stdout.
///
/// Each case is answered either `=<hex>` (the stream) or `ERR` (the oracle rejected the
/// parameter tuple). The per-case `try` matters: a case the oracle refuses (`windowBits`
/// `16 + 8` gets past `arg_bitsz/1` but then fails `deflateInit2`'s own `windowBits == 8 &&
/// wrap != 1` test) used to abort the whole script, which made every matrix containing such
/// a case silently report "the oracle batch failed to run" instead of checking anything. An
/// empty stream is `=` (not an empty line), so a case can never be lost to whitespace
/// filtering. Level `-1` is `zlib`'s `default` atom: `arg_level/1` accepts integers `0..=9`
/// only.
const BATCH_SCRIPT: &str = concat!(
    "{ok,B}=file:read_file(os:getenv(\"BD_SPEC\")),",
    "Ls=[L||L<-binary:split(B,<<\"\\n\">>,[global]),L=/=<<>>],",
    "St=fun(S)->element(S+1,{default,filtered,huffman_only,rle}) end,",
    "[begin ",
    "try ",
    "[Lv,Wv,Mv,Sv,H]=binary:split(L,<<\",\">>,[global]),",
    "Level=case binary_to_integer(Lv) of -1 -> default; N -> N end,",
    "Z=zlib:open(),",
    "ok=zlib:deflateInit(Z,Level,deflated,binary_to_integer(Wv),",
    "binary_to_integer(Mv),St(binary_to_integer(Sv))),",
    "O=iolist_to_binary(zlib:deflate(Z,binary:decode_hex(H),finish)),",
    "ok=zlib:deflateEnd(Z),zlib:close(Z),",
    "io:format(\"=~s~n\",[binary:encode_hex(O)]) ",
    "catch _:_ -> io:format(\"ERR~n\") end ",
    "end||L<-Ls],",
    "halt()."
);

/// A fresh, empty scratch directory for one oracle call.
///
/// Every call gets its **own** directory: the integration tests run in parallel threads of
/// one process, so a `process::id()`-only name would let one test delete another's spec
/// file mid-run.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_deflate_{tag}_{}_{n}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Run a whole matrix through the oracle in **one** `erl` boot.
///
/// `None` means "no oracle available" (the caller should print a SKIP line); `Some` holds
/// one answer per case, in order, where [`Err`] is a parameter tuple the oracle itself
/// refuses (`zlib:deflateInit` raised) so a caller can assert symmetric rejection.
pub fn oracle_batch(cases: &[Case]) -> Option<Vec<Result<Vec<u8>, String>>> {
    let _guard = oracle_lock();
    if !have_oracle_locked() {
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
    let dir = scratch_dir("oracle");
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

    // One answer line per case; a rejected case is `ERR`, an accepted one is `=<hex>`.
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
            .map(|l| match l.strip_prefix('=') {
                Some(hex_body) => Ok(unhex(hex_body)),
                None => Err((*l).to_string()),
            })
            .collect(),
    )
}

/// Run an arbitrary `-eval` script and return its stdout, or `None` when there is no
/// oracle (or it failed).
pub fn erl_eval(script: &str) -> Option<String> {
    let _guard = oracle_lock();
    if !have_oracle_locked() {
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

/// `zlib:uncompress/1` in the oracle — the zlib-framing convenience form of
/// [`oracle_inflate_wbits`].
pub fn oracle_inflate(stream: &[u8]) -> Option<Vec<u8>> {
    oracle_inflate_wbits(stream, 15)
}

/// `zlib:inflateInit(Z, WindowBits)` + `zlib:inflate/2` in the oracle: inflate `stream`
/// under the framing its `windowBits` selects.
///
/// The window bits are not optional: `zlib:inflateInit/1` is `inflateInit(Z, 15)`, so a raw
/// (`-15`) or gzip (`16 + 15`) stream cannot be inflated by it at all. A caller that built
/// the stream from a `DeflateConfig` must pass `config.window_bits` here.
///
/// Also the inverse check for a stream the oracle itself cannot produce (the `Z_FIXED`
/// strategy).
///
/// The stream is handed over through a file, not through `argv`: a 70 000-byte input
/// hex-encodes to 140 000 characters, past the kernel's `E2BIG` limit for a single
/// argument.
pub fn oracle_inflate_wbits(stream: &[u8], window_bits: i8) -> Option<Vec<u8>> {
    let _guard = oracle_lock();
    let dir = scratch_dir("inflate");
    let path = dir.join("stream.hex");
    std::fs::write(&path, hex(stream)).ok()?;
    let script = concat!(
        "{ok,B}=file:read_file(os:getenv(\"BD_HEX\")),",
        "Z=zlib:open(),",
        "ok=zlib:inflateInit(Z,list_to_integer(os:getenv(\"BD_WBITS\"))),",
        // `zlib:inflate/3` dequeues until its input is exhausted and returns a plain
        // iolist (the `{continue,_}`/`{finished,_}` tuples come from `safeInflate/2`),
        // so a single call is enough.
        "O=iolist_to_binary(zlib:inflate(Z, binary:decode_hex(B))),",
        "ok=zlib:inflateEnd(Z),zlib:close(Z),",
        "io:format(\"~s\",[binary:encode_hex(O)]),",
        "halt()."
    );
    let out = Command::new(erl_program())
        .args(["-noshell", "-eval", script])
        .env("BD_HEX", &path)
        .env("BD_WBITS", window_bits.to_string())
        .output()
        .ok()?;
    std::fs::remove_dir_all(&dir).ok();
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok().map(|s| unhex(&s))
}

/// The mid-stream flush mode of [`oracle_deflate_split`] — the two the `zlib` module can
/// name (`arg_flush/1` has no `partial` or `block` clause).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MidFlush {
    /// `zlib:deflate(Z, Part1, sync)` — `Z_SYNC_FLUSH`.
    Sync,
    /// `zlib:deflate(Z, Part1, full)` — `Z_FULL_FLUSH`.
    Full,
}

impl MidFlush {
    fn name(self) -> &'static str {
        match self {
            MidFlush::Sync => "sync",
            MidFlush::Full => "full",
        }
    }
}

/// The oracle's **split-flush** image: `deflateInit`, `deflate(Z, first, mid)`,
/// `deflate(Z, second, finish)`, concatenated.
///
/// This is the golden a mid-stream `Sync`/`Full` flush is checked against. Both pieces are
/// handed over in files (see [`oracle_inflate_wbits`] for why).
pub fn oracle_deflate_split(
    level: i8,
    window_bits: i8,
    first: &[u8],
    second: &[u8],
    mid: MidFlush,
) -> Option<Vec<u8>> {
    oracle_two_piece(
        level,
        window_bits,
        first,
        second,
        Some(("deflate", mid.name())),
        None,
    )
}

/// The oracle's **`deflateParams`** image: `deflateInit(Z, init_level)`,
/// `deflate(Z, first, sync)`, `deflateParams(Z, new_level, new_strategy)`,
/// `deflate(Z, rest, finish)`, concatenated.
///
/// `zlib:deflateParams/3` itself performs the `deflate(Z, <<>>, sync)` that puts the stream
/// on a byte boundary with nothing pending (the NIF refuses anything else — see
/// `zlib_nif.c:zlib_deflateParams`), and stashes its output for the next `deflate` call, so
/// the bytes between `first` and `rest` are part of the stream. `new_strategy` is the
/// `zlib` module's strategy index (0 `default` … 3 `rle`).
pub fn oracle_deflate_params(
    init_level: i8,
    first: &[u8],
    new_level: i8,
    new_strategy: u8,
    rest: &[u8],
) -> Option<Vec<u8>> {
    oracle_two_piece(
        init_level,
        15,
        first,
        rest,
        Some(("deflate", "sync")),
        Some((new_level, new_strategy)),
    )
}

/// The shared body of the two split-stream oracle calls: build the script from two input
/// files plus the optional mid-flush and `deflateParams` steps, then read the hex back.
fn oracle_two_piece(
    level: i8,
    window_bits: i8,
    first: &[u8],
    second: &[u8],
    mid: Option<(&str, &str)>,
    params: Option<(i8, u8)>,
) -> Option<Vec<u8>> {
    let _guard = oracle_lock();
    if !have_oracle_locked() {
        return None;
    }
    let dir = scratch_dir("split");
    let first_path = dir.join("first.bin");
    let second_path = dir.join("second.bin");
    std::fs::write(&first_path, first).ok()?;
    std::fs::write(&second_path, second).ok()?;
    let mid_step = match mid {
        Some((function, flush)) => format!(
            "O1=iolist_to_binary(zlib:{function}(Z,A,{flush})),"
        ),
        None => "O1=iolist_to_binary(zlib:deflate(Z,A,none)),".to_string(),
    };
    let param_step = match params {
        Some((new_level, new_strategy)) => format!(
            "ok=zlib:deflateParams(Z,{new_level},{}),",
            strategy_atom(new_strategy)
        ),
        None => String::new(),
    };
    let script = format!(
        concat!(
            "{{ok,A}}=file:read_file(os:getenv(\"BD_A\")),",
            "{{ok,B}}=file:read_file(os:getenv(\"BD_B\")),",
            "Z=zlib:open(),",
            "ok=zlib:deflateInit(Z,list_to_integer(os:getenv(\"BD_LEVEL\")),deflated,",
            "list_to_integer(os:getenv(\"BD_WBITS\")),8,default),",
            "{mid}",
            "{params}",
            "O2=iolist_to_binary(zlib:deflate(Z,B,finish)),",
            "ok=zlib:deflateEnd(Z),zlib:close(Z),",
            "io:format(\"~s\",[binary:encode_hex(<<O1/binary,O2/binary>>)]),",
            "halt()."
        ),
        mid = mid_step,
        params = param_step
    );
    let out = Command::new(erl_program())
        .args(["-noshell", "-eval", &script])
        .env("BD_A", &first_path)
        .env("BD_B", &second_path)
        .env("BD_LEVEL", level.to_string())
        .env("BD_WBITS", window_bits.to_string())
        .output()
        .ok()?;
    std::fs::remove_dir_all(&dir).ok();
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok().map(|s| unhex(&s))
}

/// `arg_strategy/1`'s atom for a strategy index (`zlib.erl:889`).
fn strategy_atom(strategy: u8) -> &'static str {
    match strategy {
        0 => "default",
        1 => "filtered",
        2 => "huffman_only",
        3 => "rle",
        _ => "default",
    }
}

/// The 70 000-byte large input: [`mixed_kib`] repeated with 37 zero bytes between the
/// copies, so the window (32 KiB) has to slide and matches can span a copy boundary.
pub fn large_input(n: usize) -> Vec<u8> {
    let mut data = Vec::new();
    while data.len() < n {
        data.extend_from_slice(&mixed_kib());
        data.extend_from_slice(&[0u8; 37]);
    }
    data.truncate(n);
    data
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
