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
//! The **live-oracle** gate for the `zlib` **stream** surface: the 19 NIF-level keys
//! `zlib.erl` is written over, driven exactly the way that module's wrappers drive them
//! and compared with the system `erl` in **both** directions.
//!
//! # What this suite adds over the embedded goldens
//!
//! `src/zlib/stream/tests.rs` pins the recorded goldens as constants. This file
//! re-derives every one of them from a live `erl` instead, so a golden that drifted (or
//! was transcribed with a slip) cannot pass. It is the crate's usual live-oracle shape
//! (`tests/zlib_oracle.rs`): **fail-loud, skip loudly without an oracle** — with no
//! usable `erl` it prints a banner and returns, which is *not* a pass.
//!
//! # Why the Rust side re-implements the wrappers' loop
//!
//! `zlib.erl`'s public functions (`deflate/2,3`, `inflate/2,3`, `safeInflate/2`,
//! `deflateParams/3`, `deflateSetDictionary/2`, …) are pure composition over the 19 NIF
//! keys: they validate the arguments, then run `restore_progress` → `enqueue_input` →
//! `dequeue_all_chunks` (or one `dequeue_next_chunk`) with hard-coded chunk sizes
//! (`?DEFLATE_IN_CHUNKSIZE`/`?DEFLATE_OUT_CHUNKSIZE` 8192, `?INFLATE_IN_CHUNKSIZE` 8192,
//! `?INFLATE_OUT_CHUNKSIZE` 16384). This suite drives the **same** sequence over the same
//! keys with the same chunk sizes — see [`zlib_deflate`], [`zlib_inflate`],
//! [`zlib_safe_inflate`] and [`zlib_deflate_params`] — and compares the *concatenated*
//! bytes (plus the per-leaf sizes) with what the oracle's wrappers produced. The Erlang
//! side therefore calls the real public API: nothing about the wrappers is modelled on
//! the oracle side.
//!
//! # What is covered
//!
//! * levels `-1, 0, 1, 2, 6, 9` × the goldens' 4 × 256-byte `sync`-then-`finish` scenario
//!   **and** the one-shot `finish` scenario — leaf-size vector, total and md5 of the
//!   concatenation, plus the bytes themselves;
//! * the `windowBits` (`15` / `-15` / `31`) and strategy (`default`/`filtered`/
//!   `huffman_only`/`rle`) matrix at level 6;
//! * both round-trip directions: the oracle inflates **this crate's** streams, and this
//!   crate inflates the **oracle's** streams;
//! * `deflateParams/3` mid-stream (the `save_progress`/`restore_progress` hand-off);
//! * the preset-dictionary protocol (Adler `2662864480`, the 15-byte stream, the raise,
//!   the `{need_dictionary,…}` return, `inflateSetDictionary`, the resumed payload);
//! * the 100 000-byte `safeInflate/2` chunk sequence and its one-shot `inflate` round
//!   trip;
//! * the guard classes (`data_error`/`stream_error`/`not_initialized`/
//!   `already_initialized`) of the stream lifecycle.
//!
//! The three `*Stash_nif` keys are **not exported** by `zlib.beam` (they are local stubs
//! the module's own bodies call), so no `-eval` can reach them: their shapes are pinned
//! locally by [`the_stash_keys_keep_their_documented_shapes`] and *live* through the two
//! public paths that use them (`deflateParams/3` and `inflate/3`'s dictionary protocol).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{bitstring_bytes, make_binary, tuple_arity, tuple_element};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{is_list, make_atom, make_small, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// `zlib.erl`'s own constants
// ---------------------------------------------------------------------------

/// The sentence every fixture is built from — the source of `F1`.
const SENTENCE: &[u8] = b"It was the best of times, it was the worst of times, it was the age of wisdom, it was the age of foolishness, it was the epoch of belief, it was the epoch of incredulity.\n";

/// `F1` — the 1024-byte pump fixture, `binary:part(binary:copy(Sentence, 6), 0, 1024)`.
fn f1() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(1024);
    while bytes.len() < 1024 {
        bytes.extend_from_slice(SENTENCE);
    }
    bytes.truncate(1024);
    bytes
}

/// The 100 000-byte payload behind the `safeInflate/2` chunk-sequence golden.
fn payload_100k() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(100_000);
    while bytes.len() < 100_000 {
        bytes.extend_from_slice(SENTENCE);
    }
    bytes.truncate(100_000);
    bytes
}

/// `?DEFLATE_IN_CHUNKSIZE` and `?DEFLATE_OUT_CHUNKSIZE`.
const DEFLATE_IN_CHUNK: i64 = 8192;
const DEFLATE_OUT_CHUNK: i64 = 8192;
/// `?INFLATE_IN_CHUNKSIZE` and `?INFLATE_OUT_CHUNKSIZE`.
const INFLATE_IN_CHUNK: i64 = 8192;
const INFLATE_OUT_CHUNK: i64 = 16384;
/// `zlib.erl`'s `arg_flush/1` codes (`Z_NO_FLUSH`, `Z_SYNC_FLUSH`, `Z_FINISH`).
const NONE: i64 = 0;
const SYNC: i64 = 2;
const FINISH: i64 = 4;
/// `arg_eos_behavior/1`'s codes (`reset`, `cut`).
const EOS_RESET: i64 = 1;
const EOS_CUT: i64 = 2;
/// `arg_method(deflated)`.
const DEFLATED: i64 = 8;
/// `arg_strategy/1`'s codes.
const STRATEGY_DEFAULT: i64 = 0;
const STRATEGY_FILTERED: i64 = 1;
const STRATEGY_HUFFMAN_ONLY: i64 = 2;
const STRATEGY_RLE: i64 = 3;

/// The preset-dictionary protocol's dictionary, payload, its Adler-32 and the stream.
const DICT: &[u8] = b"a dictionary with repeated words words words and more words";
const DICT_PAYLOAD: &[u8] = b"words words words and more words words";
const DICT_ADLER: u32 = 2_662_864_480;
const DICT_STREAM: &str = "78BB9EB8166023240F21011D420E92";

// ---------------------------------------------------------------------------
// The fixture: the real registry, a heap, an atom table
// ---------------------------------------------------------------------------

/// A registry + heap + atom table, driven exactly the way `beam_rt` drives a BIF call.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
    pid: ProcessId,
}

impl Fixture {
    /// The whole `default_registry()` (so the 19 keys are reached through the assembled
    /// roster rather than a test-only registration), no optional seam installed.
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "ok",
            "error",
            "empty",
            "continue",
            "finished",
            "need_dictionary",
            "stream_end",
            "stream_error",
            "data_error",
            "mem_error",
            "buf_error",
            "unknown_error",
            "not_initialized",
            "already_initialized",
            "not_on_controlling_process",
            "true",
            "false",
            "deflate",
            "inflate",
            "k",
            "v",
        ]);
        Fixture {
            registry: default_registry(),
            // The 100 KiB payload and its 16 KiB output chunks live on this arena.
            heap: Heap::new(1 << 22).expect("test heap"),
            atoms,
            pid: ProcessId::new(1, 0),
        }
    }

    /// Calls `zlib:function(Args)` through the assembled registry.
    fn call(&mut self, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, self.pid);
        self.registry
            .call("zlib", function, args.len() as u32, &mut ctx, args)
    }

    /// Calls `zlib:function(Args)` and unwraps the result.
    fn must(&mut self, function: &str, args: &[Eterm]) -> Eterm {
        self.call(function, args)
            .unwrap_or_else(|e| panic!("zlib:{function}/{} failed: {e:?}", args.len()))
    }

    /// A small integer argument.
    fn int(&self, value: i64) -> Eterm {
        make_small(value)
    }

    /// The atom `name`, which the fixture's vocabulary must already carry.
    fn atom(&self, name: &str) -> Eterm {
        match self.atoms.lookup(name) {
            Some(atom) => make_atom(atom),
            None => panic!("`{name}` is not in this fixture's atom vocabulary"),
        }
    }

    /// The binary `<<bytes>>` on this fixture's heap.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        make_binary(&mut self.heap, bytes).expect("the binary fits")
    }

    /// A proper list of `elems` on this fixture's heap.
    fn list(&mut self, elems: &[Eterm]) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, self.pid);
        ctx.list(elems).expect("the list fits")
    }

    /// The bytes of an iolist (`NIL` | a cons of binaries/integers | a binary) — the
    /// shape `zlib.erl`'s `append_iolist/2` builds.
    fn iolist_bytes(&self, term: Eterm) -> Vec<u8> {
        if term == NIL {
            return Vec::new();
        }
        match bitstring_bytes(term) {
            Ok((bytes, offset, size_bits)) => {
                assert_eq!(offset, 0, "an iolist element is a whole-byte binary");
                assert_eq!(size_bits % 8, 0, "an iolist element is byte-aligned");
                return bytes[..size_bits / 8].to_vec();
            }
            Err(_) => {}
        }
        assert!(is_list(term), "an iolist is NIL or a cons, not {term:?}");
        let mut out = Vec::new();
        let mut cursor = term;
        while cursor != NIL {
            assert!(is_list(cursor), "an iolist is a proper list");
            let head = beam_codec::term::cons_head(cursor).expect("a cons head");
            out.extend_from_slice(&self.iolist_bytes(head));
            cursor = beam_codec::term::cons_tail(cursor).expect("a cons tail");
        }
        out
    }

    /// The elements of a tuple term.
    fn tuple(&self, term: Eterm) -> Vec<Eterm> {
        let arity = tuple_arity(term).expect("a tuple");
        (0..arity)
            .map(|index| tuple_element(term, index).expect("an in-range element"))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The 19 keys, one call at a time
// ---------------------------------------------------------------------------

fn open(fx: &mut Fixture) -> Eterm {
    fx.must("open_nif", &[])
}

fn close(fx: &mut Fixture, handle: Eterm) -> Result<Eterm> {
    fx.call("close_nif", &[handle])
}

fn deflate_init(
    fx: &mut Fixture,
    handle: Eterm,
    level: i64,
    window_bits: i64,
    strategy: i64,
) -> Result<Eterm> {
    fx.call(
        "deflateInit_nif",
        &[
            handle,
            fx.int(level),
            fx.int(DEFLATED),
            fx.int(window_bits),
            fx.int(8),
            fx.int(strategy),
        ],
    )
}

fn inflate_init(fx: &mut Fixture, handle: Eterm, window_bits: i64, eos: i64) -> Result<Eterm> {
    fx.call(
        "inflateInit_nif",
        &[handle, fx.int(window_bits), fx.int(eos)],
    )
}

/// `enqueue_nif/2` over one binary — `zlib:enqueue_input/2` does nothing at all for
/// empty data (`enqueue_input_1(_Z, []) -> ok`), which the wrappers rely on.
fn enqueue(fx: &mut Fixture, handle: Eterm, data: &[u8]) -> Result<Eterm> {
    if data.is_empty() {
        return Ok(fx.atom("ok"));
    }
    let piece = fx.binary(data);
    let iovec = fx.list(&[piece]);
    fx.call("enqueue_nif", &[handle, iovec])
}

fn deflate_nif(
    fx: &mut Fixture,
    handle: Eterm,
    in_chunk: i64,
    out_chunk: i64,
    flush: i64,
) -> Result<Eterm> {
    fx.call(
        "deflate_nif",
        &[handle, fx.int(in_chunk), fx.int(out_chunk), fx.int(flush)],
    )
}

fn inflate_nif(
    fx: &mut Fixture,
    handle: Eterm,
    in_chunk: i64,
    out_chunk: i64,
    flush: i64,
) -> Result<Eterm> {
    fx.call(
        "inflate_nif",
        &[handle, fx.int(in_chunk), fx.int(out_chunk), fx.int(flush)],
    )
}

fn deflate_end(fx: &mut Fixture, handle: Eterm) -> Result<Eterm> {
    fx.call("deflateEnd_nif", &[handle])
}

fn inflate_end(fx: &mut Fixture, handle: Eterm) -> Result<Eterm> {
    fx.call("inflateEnd_nif", &[handle])
}

fn get_stash(fx: &mut Fixture, handle: Eterm) -> Result<Eterm> {
    fx.call("getStash_nif", &[handle])
}

fn set_stash(fx: &mut Fixture, handle: Eterm, term: Eterm) -> Result<Eterm> {
    fx.call("setStash_nif", &[handle, term])
}

fn clear_stash(fx: &mut Fixture, handle: Eterm) -> Result<Eterm> {
    fx.call("clearStash_nif", &[handle])
}

fn deflate_set_dictionary(fx: &mut Fixture, handle: Eterm, data: &[u8]) -> Result<Eterm> {
    let dict = fx.binary(data);
    fx.call("deflateSetDictionary_nif", &[handle, dict])
}

fn inflate_set_dictionary(fx: &mut Fixture, handle: Eterm, data: &[u8]) -> Result<Eterm> {
    let dict = fx.binary(data);
    fx.call("inflateSetDictionary_nif", &[handle, dict])
}

// ---------------------------------------------------------------------------
// The wrappers' own sequences (`zlib.erl`'s `deflate/3`, `inflate/3`, …)
// ---------------------------------------------------------------------------

/// What one wrapper call returned: the byte size of each iolist **leaf** (so the
/// golden's per-call vectors stay observable) and the concatenation.
struct Call {
    leaves: Vec<usize>,
    bytes: Vec<u8>,
}

/// One `deflate_nif/4`/`inflate_nif/4` answer, decoded from its tagged tuple.
enum Answer {
    Continue(Vec<usize>, Vec<u8>),
    Finished(Vec<usize>, Vec<u8>),
    NeedDictionary(u32, Vec<usize>, Vec<u8>),
    /// A bare atom — the C's `Z_MEM_ERROR` short-circuit (`zlib_return`). Only that arm
    /// answers a value; every other status either flows into the classification or
    /// raises. Carries the atom's name.
    Bare(String),
}

/// `zlib.erl`'s `dequeue_next_chunk/2`: exactly one `*_nif/4` call, decoded.
fn next_chunk(
    fx: &mut Fixture,
    handle: Eterm,
    deflating: bool,
    in_chunk: i64,
    out_chunk: i64,
    flush: i64,
) -> Result<Answer> {
    let answer = if deflating {
        deflate_nif(fx, handle, in_chunk, out_chunk, flush)?
    } else {
        inflate_nif(fx, handle, in_chunk, out_chunk, flush)?
    };
    let elements = tuple_arity(answer).ok().map(|_| fx.tuple(answer));
    let Some(elements) = elements else {
        // A bare atom (or anything untagged): report it as-is.
        let name = ["mem_error", "buf_error", "stream_end", "ok"]
            .iter()
            .find(|name| answer == fx.atom(name))
            .map(|name| (*name).to_string())
            .unwrap_or_else(|| format!("{answer:?}"));
        return Ok(Answer::Bare(name));
    };
    let tag = elements.first().copied().map(|tag| fx.atom_name(tag));
    let sizes = |out: Eterm| -> (Vec<usize>, Vec<u8>) {
        // `Out` is `[Binary]` or `[]`; one leaf per element.
        let mut leaves = Vec::new();
        let mut bytes = Vec::new();
        if out != NIL {
            let mut cursor = out;
            while cursor != NIL {
                let head = beam_codec::term::cons_head(cursor).expect("a cons head");
                let piece = fx.iolist_bytes(head);
                leaves.push(piece.len());
                bytes.extend_from_slice(&piece);
                cursor = beam_codec::term::cons_tail(cursor).expect("a cons tail");
            }
        }
        (leaves, bytes)
    };
    match (tag.as_deref(), elements.len()) {
        (Some("continue"), 2) => {
            let (leaves, bytes) = sizes(elements[1]);
            Ok(Answer::Continue(leaves, bytes))
        }
        (Some("finished"), 2) => {
            let (leaves, bytes) = sizes(elements[1]);
            Ok(Answer::Finished(leaves, bytes))
        }
        (Some("need_dictionary"), 3) => {
            let adler = beam_term::unsigned_value(elements[1]) as u32;
            let (leaves, bytes) = sizes(elements[2]);
            Ok(Answer::NeedDictionary(adler, leaves, bytes))
        }
        _ => panic!("unexpected stream answer for a {deflating} stream: {answer:?}"),
    }
}

impl Fixture {
    /// The atom `term` names, when it is one of the fixture's vocabulary.
    fn atom_name(&self, term: Eterm) -> String {
        for name in [
            "ok",
            "error",
            "empty",
            "continue",
            "finished",
            "need_dictionary",
            "stream_end",
            "stream_error",
            "data_error",
            "mem_error",
            "buf_error",
            "unknown_error",
        ] {
            if term == self.atom(name) {
                return name.to_string();
            }
        }
        format!("{term:?}")
    }
}

/// What a `dequeue_all_chunks/2` loop produced: the wrapper's iolist, or the
/// `{need_dictionary,…}` tuple it returns verbatim (dropping the output it had
/// accumulated — that is what `dequeue_all_chunks_1/3` does).
enum Chunks {
    Data(Call),
    NeedDictionary { adler: u32, out: Call },
}

/// `zlib.erl`'s `dequeue_all_chunks/2`.
fn dequeue_all_chunks(
    fx: &mut Fixture,
    handle: Eterm,
    deflating: bool,
    in_chunk: i64,
    out_chunk: i64,
    flush: i64,
) -> Result<Chunks> {
    let mut leaves = Vec::new();
    let mut bytes = Vec::new();
    loop {
        match next_chunk(fx, handle, deflating, in_chunk, out_chunk, flush)? {
            Answer::Continue(chunk, chunk_bytes) => {
                leaves.extend(chunk);
                bytes.extend_from_slice(&chunk_bytes);
            }
            Answer::Finished(chunk, chunk_bytes) => {
                leaves.extend(chunk);
                bytes.extend_from_slice(&chunk_bytes);
                return Ok(Chunks::Data(Call { leaves, bytes }));
            }
            Answer::NeedDictionary(adler, chunk, chunk_bytes) => {
                return Ok(Chunks::NeedDictionary {
                    adler,
                    out: Call {
                        leaves: chunk,
                        bytes: chunk_bytes,
                    },
                })
            }
            Answer::Bare(name) => {
                panic!("the pump answered the bare atom `{name}` — unreachable for a valid stream")
            }
        }
    }
}

/// The stash term `zlib.erl`'s `save_progress/3` stores: `{Kind, Output}`.
fn progress_term(fx: &mut Fixture, kind: &str, bytes: &[u8]) -> Eterm {
    let kind = fx.atom(kind);
    let output = if bytes.is_empty() {
        NIL
    } else {
        let piece = fx.binary(bytes);
        fx.list(&[piece])
    };
    let mut ctx = BifContext::new(&mut fx.heap, &fx.atoms, fx.pid);
    ctx.tuple(&[kind, output]).expect("the tuple fits")
}

/// `zlib.erl`'s `save_progress/3`.
fn save_progress(fx: &mut Fixture, handle: Eterm, kind: &str, bytes: &[u8]) -> Result<()> {
    let term = progress_term(fx, kind, bytes);
    let ok = set_stash(fx, handle, term)?;
    assert_eq!(ok, fx.atom("ok"), "save_progress stores the progress");
    Ok(())
}

/// `zlib.erl`'s `restore_progress/2`.
fn restore_progress(fx: &mut Fixture, handle: Eterm, kind: &str) -> Result<Vec<u8>> {
    let stash = get_stash(fx, handle)?;
    if stash == fx.atom("empty") {
        return Ok(Vec::new());
    }
    let elements = fx.tuple(stash);
    assert_eq!(elements.len(), 2, "`getStash` answers `{{ok, Term}}`");
    assert_eq!(elements[0], fx.atom("ok"), "`getStash` answers `{{ok, Term}}`");
    let stored = fx.tuple(elements[1]);
    assert_eq!(stored.len(), 2, "the stash holds `{{Kind, Output}}`");
    assert_eq!(
        stored[0],
        fx.atom(kind),
        "the stash holds this direction's progress"
    );
    let output = fx.iolist_bytes(stored[1]);
    let ok = clear_stash(fx, handle)?;
    assert_eq!(ok, fx.atom("ok"), "restore_progress clears the stash");
    Ok(output)
}

/// `zlib.erl`'s `append_iolist/2`, expressed as the leaf sizes it produces: `D` is one
/// leaf when the progress is empty, and the result is the two-element `[IO, D]`
/// otherwise.
fn append(progress: Vec<u8>, chunk: Call) -> Call {
    if progress.is_empty() {
        return chunk;
    }
    let mut leaves = vec![progress.len()];
    if !chunk.leaves.is_empty() {
        leaves.push(chunk.bytes.len());
    }
    let mut bytes = progress;
    bytes.extend_from_slice(&chunk.bytes);
    Call { leaves, bytes }
}

/// `zlib:deflate(Z, Data, Flush)`.
fn zlib_deflate(fx: &mut Fixture, handle: Eterm, data: &[u8], flush: i64) -> Result<Call> {
    let progress = restore_progress(fx, handle, "deflate")?;
    enqueue(fx, handle, data)?;
    match dequeue_all_chunks(
        fx,
        handle,
        true,
        DEFLATE_IN_CHUNK,
        DEFLATE_OUT_CHUNK,
        flush,
    )? {
        Chunks::Data(call) => Ok(append(progress, call)),
        Chunks::NeedDictionary { adler, .. } => {
            panic!("a deflate stream answered need_dictionary (adler {adler})")
        }
    }
}

/// What `zlib:inflate/3` answered: the wrapper's iolist, or the `{need_dictionary,…}`
/// tuple it hands back when `exception_on_need_dict` is `false`.
enum Inflated {
    Data(Vec<u8>),
    NeedDictionary { adler: u32, out: Vec<u8> },
}

/// `zlib:inflate(Z, Data, Options)`.
///
/// With `exception_on_need_dict` the wrapper does what `zlib.erl:520-524` does — restores
/// the progress, saves it back with the tuple's own output appended, and then calls
/// `erlang:error({need_dictionary, Adler})` (Erlang code this crate does not run, so the
/// outcome is reported rather than raised).
fn zlib_inflate(
    fx: &mut Fixture,
    handle: Eterm,
    data: &[u8],
    exception_on_need_dict: bool,
) -> Result<Inflated> {
    enqueue(fx, handle, data)?;
    match dequeue_all_chunks(
        fx,
        handle,
        false,
        INFLATE_IN_CHUNK,
        INFLATE_OUT_CHUNK,
        NONE,
    )? {
        Chunks::Data(call) => {
            let progress = restore_progress(fx, handle, "inflate")?;
            Ok(Inflated::Data(append(progress, call).bytes))
        }
        Chunks::NeedDictionary { adler, out } => {
            if exception_on_need_dict {
                let progress = restore_progress(fx, handle, "inflate")?;
                let mut saved = progress;
                saved.extend_from_slice(&out.bytes);
                save_progress(fx, handle, "inflate", &saved)?;
            }
            Ok(Inflated::NeedDictionary {
                adler,
                out: out.bytes,
            })
        }
    }
}

/// `zlib:safeInflate(Z, Data)` — one `dequeue_next_chunk/2`, untagged.
fn zlib_safe_inflate(fx: &mut Fixture, handle: Eterm, data: &[u8]) -> Result<Answer> {
    enqueue(fx, handle, data)?;
    next_chunk(
        fx,
        handle,
        false,
        INFLATE_IN_CHUNK,
        INFLATE_OUT_CHUNK,
        NONE,
    )
}

/// `zlib:deflateParams(Z, Level, Strategy)` — including the `deflate(Z, <<>>, sync)`
/// progress hand-off the wrapper performs.
fn zlib_deflate_params(
    fx: &mut Fixture,
    handle: Eterm,
    level: i64,
    strategy: i64,
) -> Result<Call> {
    let progress = zlib_deflate(fx, handle, &[], SYNC)?;
    let ok = fx.call(
        "deflateParams_nif",
        &[handle, fx.int(level), fx.int(strategy)],
    )?;
    assert_eq!(ok, fx.atom("ok"), "deflateParams_nif answers ok");
    save_progress(fx, handle, "deflate", &progress.bytes)?;
    Ok(progress)
}

// ---------------------------------------------------------------------------
// Guard tokens — the class of an outcome, as both sides spell it
// ---------------------------------------------------------------------------

/// How both sides spell one outcome: the value's shape, or the raise's reason.
fn value_token(fx: &Fixture, result: Result<Eterm>) -> String {
    match result {
        Ok(term) => {
            if term == fx.atom("ok") {
                "ok".to_string()
            } else if term == fx.atom("empty") {
                "empty".to_string()
            } else if term == fx.atom("error") {
                "error".to_string()
            } else if term == NIL || is_list(term) {
                "data".to_string()
            } else {
                format!("value:{term:?}")
            }
        }
        Err(error) => raise_token(error),
    }
}

/// How both sides spell a raise.
fn raise_token(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => match raise.reason.as_atom() {
            Some(name) => format!("raise:{name}"),
            None => "raise:<structured>".to_string(),
        },
        BeamError::BadArg => "raise:badarg".to_string(),
        other => format!("error:{other:?}"),
    }
}

/// How both sides spell a wrapper call's outcome.
fn call_token(result: Result<Call>) -> String {
    match result {
        Ok(call) => {
            assert!(
                call.bytes.len() >= call.leaves.iter().sum::<usize>(),
                "the leaves cover the concatenation"
            );
            "data".to_string()
        }
        Err(error) => raise_token(error),
    }
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// One `erl` boot at a time: the oracle is a shared machine resource and the tests run in
/// parallel threads.
static ORACLE_LOCK: Mutex<()> = Mutex::new(());

/// A `$TMPDIR` scratch directory for the payload and stream files the oracle reads;
/// removed on drop, and a failed removal is reported rather than hidden.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "beam_bifs_zlib_stream_{}_{tag}",
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

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs zlib_stream] $ERTS_ORACLE='{}' is not an existing executable \
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
    let _guard = ORACLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

/// The Erlang prologue every program starts with: hex/md5/`~w` helpers.
///
/// Every helper's local bindings use variable names that are **not** bound in the enclosing
/// `-eval` scope: Erlang has no shadowing, so re-binding an already-bound name inside a `fun`
/// is a match (e.g. `Hex = <list>` against the `Hex` *fun* raises `badmatch` at boot).
const ORACLE_PRELUDE: &str = "\
Hex = fun(B) -> lists:flatten([io_lib:format(\"~2.16.0b\", [X]) || <<X>> <= B]) end, \
Md5 = fun(B) -> H = lists:flatten([io_lib:format(\"~2.16.0b\", [X]) || <<X>> <= erlang:md5(B)]), H end, \
Sizes = fun(Out) -> [byte_size(iolist_to_binary([X])) || X <- Out] end, \
Val = fun(F) -> try F() of V -> V catch error:R -> {error, R} end end, ";
/// `path` as an Erlang string literal.
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

/// Prints the loud "no oracle, not a pass" banner.
fn skip_banner(what: &str) {
    eprintln!();
    eprintln!("========= beam_bifs zlib_stream: live oracle SKIPPED ({what}) =========");
    eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
    eprintln!("executable, and no `erl` on $PATH).");
    eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
    eprintln!("The zlib stream surface's byte identity with the oracle is then");
    eprintln!("UNVERIFIED.");
    eprintln!("======================================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------
// Small text helpers
// ---------------------------------------------------------------------------

/// Uppercase hex.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0').to_ascii_uppercase());
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0').to_ascii_uppercase());
    }
    out
}

/// Decodes a hex string (non-hex digits are skipped).
fn unhex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|b| b.is_ascii_hexdigit())
        .map(|b| match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => b - b'A' + 10,
        })
        .collect();
    assert_eq!(digits.len() % 2, 0, "hex must have an even digit count");
    digits
        .chunks(2)
        .map(|pair| (pair[0] << 4) | pair[1])
        .collect()
}

/// A compact RFC 1321 MD5 as uppercase hex.
///
/// The crate's own digest lives behind the `erlang:md5/*` BIFs (`src/checksum.rs`), whose
/// digest function is private, and the transcripts are compared as md5s — so the test
/// carries the reference implementation, exactly as `src/zlib/stream/tests.rs` does.
fn md5(bytes: &[u8]) -> String {
    #[rustfmt::skip]
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];
    #[rustfmt::skip]
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in message.chunks(64) {
        let mut words = [0u32; 16];
        for (index, word) in words.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for round in 0..64 {
            let (f, g) = match round / 16 {
                0 => ((b & c) | (!b & d), round),
                1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
                2 => (b ^ c ^ d, (3 * round + 5) % 16),
                _ => (c ^ (b | !d), (7 * round) % 16),
            };
            let swapped = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[round])
                    .wrapping_add(words[g])
                    .rotate_left(S[round]),
            );
            a = swapped;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0]
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

/// Parses one `~w`-printed list of integers — `[268]`, `[96,9]` or `[]`.
fn parse_sizes(field: &str) -> Vec<usize> {
    let inner = field.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<usize>()
                .unwrap_or_else(|e| panic!("`{value}` in `{field}` is not a list size: {e}"))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The deflate transcript (levels, window bits, strategy) and both round-trip directions
// ---------------------------------------------------------------------------

/// How a deflate case feeds `F1` to the wrapper.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 4 × 256 bytes with `sync`, then `[]` with `finish`.
    SyncThenFinish,
    /// `F1` in one `finish` call.
    OneShotFinish,
}

impl Mode {
    /// The Erlang atom the generated program matches on.
    fn erl(self) -> &'static str {
        match self {
            Mode::SyncThenFinish => "sync",
            Mode::OneShotFinish => "oneshot",
        }
    }

    /// The label used in diagnostics.
    fn tag(self) -> &'static str {
        match self {
            Mode::SyncThenFinish => "sync",
            Mode::OneShotFinish => "oneshot",
        }
    }
}

/// One deflate transcript case.
struct DeflateCase {
    level: i64,
    window_bits: i64,
    strategy: i64,
    mode: Mode,
}

impl DeflateCase {
    /// The eos behaviour the matching `inflateInit` uses: `zlib:gunzip/1` inflates a gzip
    /// framing with `reset`, every other framing with the default `cut`.
    fn eos(&self) -> i64 {
        if self.window_bits == 31 {
            EOS_RESET
        } else {
            EOS_CUT
        }
    }

    /// The level as `zlib:deflateInit/6` takes it: its `arg_level/1` accepts the atom
    /// `default` (which is zlib's `-1`) or an integer `0..9`, and nothing else.
    fn level_erl(&self) -> String {
        if self.level < 0 {
            "default".to_string()
        } else {
            self.level.to_string()
        }
    }

    /// The strategy as `zlib:deflateInit/6` takes it — `arg_strategy/1` is atom-only.
    fn strategy_erl(&self) -> &'static str {
        match self.strategy {
            STRATEGY_FILTERED => "filtered",
            STRATEGY_HUFFMAN_ONLY => "huffman_only",
            STRATEGY_RLE => "rle",
            _ => "default",
        }
    }

    /// The diagnostics label.
    fn label(&self) -> String {
        format!(
            "{} level {} wbits {} strategy {}",
            self.mode.tag(),
            self.level,
            self.window_bits,
            self.strategy
        )
    }
}

/// The case matrix: the golden level sweep in both scenarios, then the window-bits and
/// strategy matrix at level 6.
fn deflate_cases() -> Vec<DeflateCase> {
    let mut cases = Vec::new();
    for level in [-1, 0, 1, 2, 6, 9] {
        for mode in [Mode::SyncThenFinish, Mode::OneShotFinish] {
            cases.push(DeflateCase {
                level,
                window_bits: 15,
                strategy: STRATEGY_DEFAULT,
                mode,
            });
        }
    }
    for (window_bits, strategy) in [
        (15, STRATEGY_DEFAULT),
        (-15, STRATEGY_DEFAULT),
        (31, STRATEGY_DEFAULT),
        (15, STRATEGY_FILTERED),
        (15, STRATEGY_HUFFMAN_ONLY),
        (15, STRATEGY_RLE),
    ] {
        cases.push(DeflateCase {
            level: 6,
            window_bits,
            strategy,
            mode: Mode::OneShotFinish,
        });
    }
    cases
}

/// Drives one case on this crate's side: the wrapper calls, in the wrapper's own order.
///
/// Answers `(the per-call byte totals, the concatenation)` — the pair the oracle's
/// `Sizes(Out)` and `iolist_to_binary(Out)` report.
fn run_case(fx: &mut Fixture, case: &DeflateCase) -> Result<(Vec<usize>, Vec<u8>)> {
    let handle = open(fx);
    deflate_init(fx, handle, case.level, case.window_bits, case.strategy)?;
    let payload = f1();
    let calls = match case.mode {
        Mode::SyncThenFinish => {
            let mut calls = Vec::new();
            for index in 0..4 {
                calls.push(zlib_deflate(
                    fx,
                    handle,
                    &payload[index * 256..(index + 1) * 256],
                    SYNC,
                )?);
            }
            calls.push(zlib_deflate(fx, handle, &[], FINISH)?);
            calls
        }
        Mode::OneShotFinish => vec![zlib_deflate(fx, handle, &payload, FINISH)?],
    };
    deflate_end(fx, handle)?;
    close(fx, handle)?;

    // Fold the calls into the pair the oracle prints.
    let sizes: Vec<usize> = calls.iter().map(|call| call.bytes.len()).collect();
    let mut bytes = Vec::new();
    for call in &calls {
        for leaf in &call.leaves {
            assert!(
                *leaf <= DEFLATE_OUT_CHUNK as usize,
                "every output leaf fits the 8 KiB out chunk"
            );
        }
        bytes.extend_from_slice(&call.bytes);
    }
    Ok((sizes, bytes))
}

/// The generated Erlang program: one line per case, `Sizes Total <md5> <hex>`, then the
/// `deflateParams/3` transcript.
///
/// The payload travels as a file (`F1`'s bytes are read with `file:read_file/1`) rather
/// than as a `<<…>>` literal: it contains a newline, and keeping the escaping out of the
/// `-eval` source keeps the two sides' fixtures literally the same bytes.
fn eval_deflate_transcript(cases: &[DeflateCase], payload: &Path) -> String {
    let mut eval = String::from(ORACLE_PRELUDE);
    eval.push_str(&format!(
        "{{ok, P}} = file:read_file({}),\
         \nChunks = [binary:part(P, I * 256, 256) || I <- lists:seq(0, 3)],\
         \nRun = fun(Level, WBits, Strategy, Mode) ->\
         \n        Z = zlib:open(),\
         \n        ok = zlib:deflateInit(Z, Level, deflated, WBits, 8, Strategy),\
         \n        Out = case Mode of\
         \n                  sync -> [zlib:deflate(Z, C, sync) || C <- Chunks] ++ [zlib:deflate(Z, [], finish)];\
         \n                  oneshot -> [zlib:deflate(Z, P, finish)]\
         \n              end,\
         \n        ok = zlib:deflateEnd(Z),\
         \n        ok = zlib:close(Z),\
         \n        Out\
         \n      end,\
         \nCases = [",
        erl_string(payload)
    ));
    for (index, case) in cases.iter().enumerate() {
        if index > 0 {
            eval.push(',');
        }
        eval.push_str(&format!(
            "{{{},{},{},{}}}",
            case.level_erl(),
            case.window_bits,
            case.strategy_erl(),
            case.mode.erl()
        ));
    }
    eval.push_str(
        "],\
         \n[begin\
         \n   Out = Run(Level, WBits, Strategy, Mode),\
         \n   B = iolist_to_binary(Out),\
         \n   io:format(\"~w ~w ~s ~s~n\", [Sizes(Out), byte_size(B), Md5(B), Hex(B)])\
         \n end || {Level, WBits, Strategy, Mode} <- Cases],\
         \nZP = zlib:open(),\
         \nok = zlib:deflateInit(ZP, 0),\
         \nP1 = zlib:deflate(ZP, binary:copy(<<\"A\">>, 16), none),\
         \nok = zlib:deflateParams(ZP, 9, default),\
         \nP3 = zlib:deflate(ZP, <<\"BBBB\">>, finish),\
         \nok = zlib:deflateEnd(ZP),\
         \nok = zlib:close(ZP),\
         \nPB = iolist_to_binary([P1, P3]),\
         \nio:format(\"PARAMS ~w ~w ~w ~s ~s ~w~n\",\
         \n          [[byte_size(iolist_to_binary(P1)), byte_size(iolist_to_binary(P3))],\
         \n           Sizes(P3), byte_size(PB), Md5(PB), Hex(PB),\
         \n           zlib:uncompress(PB) =:= <<\"AAAAAAAAAAAAAAAABBBB\">>]),\
         \nhalt().",
    );
    eval
}

/// One decoded transcript line.
#[derive(Debug)]
struct Transcript {
    sizes: Vec<usize>,
    total: usize,
    md5: String,
    bytes: Vec<u8>,
}

impl Transcript {
    /// The stream's bytes — the direction-2 input.
    fn stream(&self) -> &[u8] {
        &self.bytes
    }
}

/// Parses the per-case transcript lines.
fn parse_transcripts(stdout: &str, expected: usize) -> Vec<Transcript> {
    let mut transcripts = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("PARAMS") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(
            fields.len(),
            4,
            "a transcript line is `Sizes Total Md5 Hex`: `{line}`"
        );
        transcripts.push(Transcript {
            sizes: parse_sizes(fields[0]),
            total: fields[1]
                .parse()
                .unwrap_or_else(|e| panic!("`{}` is not a byte total: {e}", fields[1])),
            md5: fields[2].to_ascii_uppercase(),
            bytes: unhex(fields[3]),
        });
    }
    assert_eq!(
        transcripts.len(),
        expected,
        "the oracle printed {} transcripts for {expected} cases:\n{stdout}",
        transcripts.len()
    );
    transcripts
}

/// The oracle's transcripts, one per case, plus its `deflateParams/3` line.
fn oracle_deflate_transcript(
    oracle: &Path,
    cases: &[DeflateCase],
    scratch: &Scratch,
) -> (Vec<Transcript>, String) {
    let payload = scratch.write("transcript_payload.bin", &f1());
    let stdout = run_erl(oracle, &eval_deflate_transcript(cases, &payload));
    let transcripts = parse_transcripts(&stdout, cases.len());
    let params = stdout
        .lines()
        .find(|line| line.starts_with("PARAMS"))
        .unwrap_or_else(|| panic!("the oracle printed no `deflateParams` line:\n{stdout}"))
        .to_string();
    (transcripts, params)
}

/// One case's complete comparison against the oracle.
fn assert_transcript(case: &DeflateCase, transcript: &Transcript, sizes: &[usize], bytes: &[u8]) {
    let label = case.label();
    assert_eq!(
        sizes, transcript.sizes,
        "the per-call output sizes differ from the oracle for {label}"
    );
    assert_eq!(
        bytes.len(),
        transcript.total,
        "the concatenated size differs from the oracle for {label}"
    );
    assert_eq!(
        md5(bytes),
        transcript.md5,
        "the md5 of the concatenation differs from the oracle for {label}"
    );
    assert_eq!(
        bytes,
        transcript.stream(),
        "the bytes differ from the oracle for {label}"
    );
}

#[test]
fn the_deflate_transcript_matches_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        skip_banner("deflate transcript");
        return;
    };
    let cases = deflate_cases();
    assert_eq!(cases.len(), 18, "12 level cases + 6 wbits/strategy cases");
    let scratch = Scratch::new("transcript");
    let (transcripts, params_line) = oracle_deflate_transcript(&oracle, &cases, &scratch);

    let mut fx = Fixture::new();
    for (case, transcript) in cases.iter().zip(&transcripts) {
        let (sizes, bytes) =
            run_case(&mut fx, case).unwrap_or_else(|e| panic!("{} failed: {e:?}", case.label()));
        assert_transcript(case, transcript, &sizes, &bytes);
    }

    // `deflateParams/3`'s mid-stream transcript.
    let params = run_params_case(&mut fx);
    assert!(
        params_line.ends_with(" true"),
        "the oracle round-trips {params_line}"
    );
    let fields: Vec<&str> = params_line.split_whitespace().collect();
    assert_eq!(fields.len(), 7, "the PARAMS line's shape: {params_line}");
    assert_eq!(
        parse_sizes(fields[1]),
        params
            .iter()
            .map(|call| call.bytes.len())
            .collect::<Vec<usize>>(),
        "the per-call sizes of the `deflateParams` sequence"
    );
    assert_eq!(
        parse_sizes(fields[2]),
        params[1].leaves.clone(),
        "the second call's leaf sizes (the golden's `[26, 8]`)"
    );
    let total: usize = params.iter().map(|call| call.bytes.len()).sum();
    assert_eq!(total, fields[3].parse::<usize>().expect("a byte total"));
    let mut bytes = Vec::new();
    for call in &params {
        bytes.extend_from_slice(&call.bytes);
    }
    assert_eq!(md5(&bytes), fields[4].to_ascii_uppercase(), "the md5");
    assert_eq!(hex(&bytes), fields[5].to_ascii_uppercase(), "the bytes");
    assert_eq!(total, 36, "the golden's 36-byte total");
    println!(
        "zlib_stream deflate transcript: {} cases agree with the oracle",
        cases.len()
    );
}

/// The `deflateParams/3` sequence on this crate's side.
fn run_params_case(fx: &mut Fixture) -> Vec<Call> {
    let handle = open(fx);
    deflate_init(fx, handle, 0, 15, STRATEGY_DEFAULT).expect("deflateInit");
    let sixteen = vec![b'A'; 16];
    let first = zlib_deflate(fx, handle, &sixteen, NONE).expect("the first deflate");
    zlib_deflate_params(fx, handle, 9, STRATEGY_DEFAULT).expect("deflateParams");
    let last = zlib_deflate(fx, handle, b"BBBB", FINISH).expect("the last deflate");
    deflate_end(fx, handle).expect("deflateEnd");
    close(fx, handle).expect("close");
    assert_eq!(first.leaves, vec![2], "the golden's `[2]`");
    assert_eq!(last.leaves, vec![26, 8], "the golden's `[26, 8]`");
    vec![first, last]
}

// ---------------------------------------------------------------------------
// Direction 2: this crate inflates the oracle's streams
// ---------------------------------------------------------------------------

#[test]
fn the_oracle_s_streams_inflate_here() {
    let Some(oracle) = resolve_oracle() else {
        skip_banner("inflate of the oracle's streams");
        return;
    };
    let cases = deflate_cases();
    let scratch = Scratch::new("dir2");
    let (transcripts, _) = oracle_deflate_transcript(&oracle, &cases, &scratch);

    let payload = f1();
    let mut fx = Fixture::new();
    for (case, transcript) in cases.iter().zip(&transcripts) {
        let handle = open(&mut fx);
        inflate_init(&mut fx, handle, case.window_bits, case.eos())
            .unwrap_or_else(|e| panic!("inflateInit for {} failed: {e:?}", case.label()));
        match zlib_inflate(&mut fx, handle, transcript.stream(), true)
            .unwrap_or_else(|e| panic!("inflate for {} failed: {e:?}", case.label()))
        {
            Inflated::Data(bytes) => assert_eq!(
                bytes,
                payload,
                "this crate must inflate the oracle's stream for {}",
                case.label()
            ),
            Inflated::NeedDictionary { adler, .. } => {
                panic!("{}: unexpected need_dictionary {adler}", case.label())
            }
        }
        inflate_end(&mut fx, handle)
            .unwrap_or_else(|e| panic!("inflateEnd for {} failed: {e:?}", case.label()));
        close(&mut fx, handle).expect("close");
    }
    println!(
        "zlib_stream direction 2: {} oracle streams inflate here",
        cases.len()
    );
}

// ---------------------------------------------------------------------------
// Direction 1: the oracle inflates this crate's streams
// ---------------------------------------------------------------------------

/// The generated program: one `Bool EndTag` line per case.
fn eval_oracle_inflates(entries: &[(PathBuf, PathBuf, i64, i64)]) -> String {
    let mut eval = String::from(ORACLE_PRELUDE);
    eval.push_str("L = [");
    for (index, (payload, stream, wbits, eos)) in entries.iter().enumerate() {
        if index > 0 {
            eval.push(',');
        }
        eval.push_str(&format!(
            "{{{},{},{},{}}}",
            erl_string(payload),
            erl_string(stream),
            wbits,
            if *eos == EOS_RESET { "reset" } else { "cut" }
        ));
    }
    eval.push_str(
        "],\
         \n[begin\
         \n   {ok, P} = file:read_file(PP),\
         \n   {ok, S} = file:read_file(SP),\
         \n   Z = zlib:open(), ok = zlib:inflateInit(Z, WBits, Eos),\
         \n   R = Val(fun() -> iolist_to_binary(zlib:inflate(Z, S)) end),\
         \n   E = Val(fun() -> zlib:inflateEnd(Z) end),\
         \n   ok = zlib:close(Z),\
         \n   io:format(\"~w ~w~n\", [R =:= P, E])\
         \n end || {PP, SP, WBits, Eos} <- L],\
         \nhalt().",
    );
    eval
}

#[test]
fn the_oracle_inflates_this_crates_streams() {
    let Some(oracle) = resolve_oracle() else {
        skip_banner("oracle inflates this crate's streams");
        return;
    };
    let cases = deflate_cases();
    let payload = f1();
    let scratch = Scratch::new("dir1");
    let payload_path = scratch.write("payload.bin", &payload);

    let mut fx = Fixture::new();
    let mut entries = Vec::with_capacity(cases.len());
    for (index, case) in cases.iter().enumerate() {
        let (_, bytes) =
            run_case(&mut fx, case).unwrap_or_else(|e| panic!("{} failed: {e:?}", case.label()));
        let stream_path = scratch.write(&format!("ours_{index}.bin"), &bytes);
        entries.push((payload_path.clone(), stream_path, case.window_bits, case.eos()));
    }

    let stdout = run_erl(&oracle, &eval_oracle_inflates(&entries));
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        cases.len(),
        "the oracle printed {} verdicts for {} cases:\n{stdout}",
        lines.len(),
        cases.len()
    );
    for (line, case) in lines.iter().zip(&cases) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 2, "a `Bool EndTag` line: `{line}`");
        assert_eq!(
            fields[0],
            "true",
            "the oracle must recover the payload from this crate's stream for {}",
            case.label()
        );
        assert_eq!(
            fields[1],
            "ok",
            "the oracle's `inflateEnd` must succeed for {}",
            case.label()
        );
    }
}

// ---------------------------------------------------------------------------
// The preset-dictionary protocol
// ---------------------------------------------------------------------------

fn eval_dictionary() -> String {
    let mut eval = String::from(ORACLE_PRELUDE);
    eval.push_str(
        "Nd = fun(V) -> case V of\
         \n        {need_dictionary, Ad, Out} -> {need_dictionary, Ad, Sizes(Out)};\
         \n        {finished, Out} -> {finished, Sizes(Out)};\
         \n        Other -> Other\
         \n    end end,\
         \nDict = <<\"",
    );
    eval.push_str("a dictionary with repeated words words words and more words");
    eval.push_str(
        "\">>, Payload = <<\"words words words and more words words\">>,\
         \nZ = zlib:open(), ok = zlib:deflateInit(Z, default),\
         \nA = Val(fun() -> zlib:deflateSetDictionary(Z, Dict) end),\
         \nS = iolist_to_binary(zlib:deflate(Z, Payload, finish)),\
         \nok = zlib:deflateEnd(Z), ok = zlib:close(Z),\
         \nio:format(\"ADLER ~w~n\", [A]),\
         \nio:format(\"STREAM ~s ~w~n\", [Hex(S), byte_size(S)]),\
         \nZ1 = zlib:open(), ok = zlib:inflateInit(Z1, 15),\
         \nR1 = Val(fun() -> zlib:inflate(Z1, S) end),\
         \nio:format(\"RAISE ~w~n\", [R1]),\
         \nok = zlib:inflateSetDictionary(Z1, Dict),\
         \nP1 = iolist_to_binary(zlib:inflate(Z1, [])),\
         \nio:format(\"RESUMED ~s~n\", [Hex(P1)]),\
         \nE1 = Val(fun() -> zlib:inflateEnd(Z1) end),\
         \nok = zlib:close(Z1),\
         \nio:format(\"END1 ~w~n\", [E1]),\
         \nZ2 = zlib:open(), ok = zlib:inflateInit(Z2, 15),\
         \nR2 = Val(fun() -> Nd(zlib:inflate(Z2, S, [{exception_on_need_dict, false}])) end),\
         \nio:format(\"NDFALSE ~w~n\", [R2]),\
         \nok = zlib:inflateSetDictionary(Z2, Dict),\
         \nP2 = iolist_to_binary(zlib:inflate(Z2, [])),\
         \nio:format(\"RESUMED2 ~s~n\", [Hex(P2)]),\
         \nZ3 = zlib:open(), ok = zlib:inflateInit(Z3, 15),\
         \nR3 = Val(fun() -> Nd(zlib:safeInflate(Z3, S)) end),\
         \nio:format(\"NDSAFE ~w~n\", [R3]),\
         \nok = zlib:inflateSetDictionary(Z3, Dict),\
         \nR4 = Val(fun() -> Nd(zlib:safeInflate(Z3, [])) end),\
         \nio:format(\"SAFEFIN ~w~n\", [R4]),\
         \nok = zlib:inflateEnd(Z3), ok = zlib:close(Z3),\
         \nhalt().",
    );
    eval
}

/// The `K V` line named `key` from the oracle's output.
fn oracle_line<'a>(stdout: &'a str, key: &str) -> &'a str {
    stdout
        .lines()
        .find(|line| line.starts_with(key) && line[key.len()..].starts_with(' '))
        .unwrap_or_else(|| panic!("the oracle printed no `{key}` line:\n{stdout}"))
}

/// The fields of the oracle's `key …` line, after the key.
fn oracle_fields<'a>(stdout: &'a str, key: &str) -> Vec<&'a str> {
    let line = oracle_line(stdout, key);
    line.split_whitespace().skip(1).collect()
}

#[test]
fn the_dictionary_protocol_matches_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        skip_banner("preset dictionary");
        return;
    };
    let stdout = run_erl(&oracle, &eval_dictionary());

    // The deflate side: the Adler-32 and the stream's bytes.
    let adler: u32 = oracle_fields(&stdout, "ADLER")[0]
        .parse()
        .unwrap_or_else(|e| panic!("the oracle's Adler is not an integer: {e}"));
    assert_eq!(adler, DICT_ADLER, "the golden's dictionary Adler");
    let stream = unhex(oracle_fields(&stdout, "STREAM")[0]);
    assert_eq!(hex(&stream), DICT_STREAM, "the golden's 15-byte stream");
    assert_eq!(oracle_fields(&stdout, "STREAM")[1], "15");

    let mut fx = Fixture::new();
    let handle = open(&mut fx);
    deflate_init(&mut fx, handle, -1, 15, STRATEGY_DEFAULT).expect("deflateInit");
    let ours = deflate_set_dictionary(&mut fx, handle, DICT).expect("deflateSetDictionary");
    assert_eq!(
        beam_term::unsigned_value(ours) as u32,
        adler,
        "deflateSetDictionary answers the oracle's Adler-32"
    );
    let packed = zlib_deflate(&mut fx, handle, DICT_PAYLOAD, FINISH).expect("deflate");
    assert_eq!(packed.bytes, stream, "the dictionary stream's bytes");
    deflate_end(&mut fx, handle).expect("deflateEnd");
    close(&mut fx, handle).expect("close");

    // (1) The raising form: `error:{need_dictionary, Adler}`, then the resumed payload.
    // The eval wraps the call in the `Val` thunk, so the raised error comes back as
    // `{error,{need_dictionary,Adler}}` — that (not the bare `{need_dictionary,…}` token) is
    // what the oracle prints.
    let raise = oracle_fields(&stdout, "RAISE");
    assert_eq!(
        raise.join(" "),
        format!("{{error,{{need_dictionary,{adler}}}}}"),
        "the oracle raises the dictionary's Adler"
    );

    let resumed = unhex(oracle_fields(&stdout, "RESUMED")[0]);
    assert_eq!(resumed, DICT_PAYLOAD, "the oracle's resumed payload");

    let handle = open(&mut fx);
    inflate_init(&mut fx, handle, 15, EOS_CUT).expect("inflateInit");
    match zlib_inflate(&mut fx, handle, &stream, true).expect("inflate") {
        Inflated::NeedDictionary {
            adler: raised,
            out,
        } => {
            assert_eq!(raised, adler, "the `{{need_dictionary,…}}` Adler");
            assert_eq!(out, Vec::<u8>::new(), "the `{{need_dictionary,…}}` output");
        }
        Inflated::Data(bytes) => panic!("expected need_dictionary, got {} bytes", bytes.len()),
    }
    // The wrapper stashed the progress; the raise itself is `zlib.erl`'s own code.
    let ok = inflate_set_dictionary(&mut fx, handle, DICT).expect("inflateSetDictionary");
    assert_eq!(ok, fx.atom("ok"));
    match zlib_inflate(&mut fx, handle, &[], true).expect("the resumed inflate") {
        Inflated::Data(bytes) => assert_eq!(bytes, resumed, "the resumed payload"),
        Inflated::NeedDictionary { adler, .. } => panic!("a second need_dictionary {adler}"),
    }
    inflate_end(&mut fx, handle).expect("inflateEnd");
    close(&mut fx, handle).expect("close");
    assert_eq!(
        oracle_fields(&stdout, "END1")[0],
        "ok",
        "the oracle's `inflateEnd` after the resumed payload"
    );

    // (2) `{exception_on_need_dict, false}`: the three-tuple instead of a raise.
    let nd_false = oracle_fields(&stdout, "NDFALSE");
    assert_eq!(
        nd_false.join(" "),
        format!("{{need_dictionary,{adler},[]}}"),
        "the oracle's `{{need_dictionary,Adler,[]}}`"
    );
    let resumed2 = unhex(oracle_fields(&stdout, "RESUMED2")[0]);
    assert_eq!(resumed2, DICT_PAYLOAD, "the oracle's second resumed payload");

    let handle = open(&mut fx);
    inflate_init(&mut fx, handle, 15, EOS_CUT).expect("inflateInit");
    match zlib_inflate(&mut fx, handle, &stream, false).expect("inflate") {
        Inflated::NeedDictionary {
            adler: raised,
            out,
        } => {
            assert_eq!(raised, adler, "the `[exception_on_need_dict]` Adler");
            assert!(out.is_empty(), "the `[exception_on_need_dict]` output is `[]`");
        }
        Inflated::Data(bytes) => panic!("expected need_dictionary, got {} bytes", bytes.len()),
    }
    let ok = inflate_set_dictionary(&mut fx, handle, DICT).expect("inflateSetDictionary");
    assert_eq!(ok, fx.atom("ok"));
    match zlib_inflate(&mut fx, handle, &[], false).expect("the resumed inflate") {
        Inflated::Data(bytes) => assert_eq!(bytes, resumed2, "the resumed payload"),
        Inflated::NeedDictionary { adler, .. } => panic!("a second need_dictionary {adler}"),
    }
    inflate_end(&mut fx, handle).expect("inflateEnd");
    close(&mut fx, handle).expect("close");

    // (3) `safeInflate/2`: the same tuple, then `{finished, …}` after the dictionary.
    let nd_safe = oracle_fields(&stdout, "NDSAFE");
    assert_eq!(
        nd_safe.join(" "),
        format!("{{need_dictionary,{adler},[]}}"),
        "the oracle's `safeInflate` tuple"
    );
    let safe_fin = oracle_fields(&stdout, "SAFEFIN");
    assert_eq!(safe_fin.len(), 1, "one token per `safeInflate` verdict");
    // `~w` prints `{finished,[38]}`, i.e. no space after the comma, so the verdict is ONE token.
    let token = safe_fin[0];
    let inner = token
        .strip_prefix("{finished,")
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or_else(|| {
            panic!("the oracle's resumed `safeInflate` is `{{finished,…}}`: `{token}`")
        });
    assert_eq!(
        parse_sizes(inner),
        vec![DICT_PAYLOAD.len()],
        "the oracle's resumed `safeInflate` output"
    );
    let handle = open(&mut fx);
    inflate_init(&mut fx, handle, 15, EOS_CUT).expect("inflateInit");
    match zlib_safe_inflate(&mut fx, handle, &stream).expect("safeInflate") {
        Answer::NeedDictionary(raised, leaves, bytes) => {
            assert_eq!(raised, adler, "the `safeInflate` Adler");
            assert!(leaves.is_empty() && bytes.is_empty(), "the `safeInflate` output is `[]`");
        }
        _ => panic!("expected `safeInflate` to answer need_dictionary"),
    }
    let ok = inflate_set_dictionary(&mut fx, handle, DICT).expect("inflateSetDictionary");
    assert_eq!(ok, fx.atom("ok"));
    match zlib_safe_inflate(&mut fx, handle, &[]).expect("the resumed safeInflate") {
        Answer::Finished(leaves, bytes) => {
            assert_eq!(leaves, vec![DICT_PAYLOAD.len()], "the resumed leaves");
            assert_eq!(bytes, DICT_PAYLOAD, "the resumed payload");
        }
        _ => panic!("expected `safeInflate` to finish after the dictionary"),
    }
    inflate_end(&mut fx, handle).expect("inflateEnd");
    close(&mut fx, handle).expect("close");
}

// ---------------------------------------------------------------------------
// The 100 000-byte `safeInflate/2` chunk sequence
// ---------------------------------------------------------------------------

fn eval_safe_inflate(payload: &Path) -> String {
    let mut eval = String::from(ORACLE_PRELUDE);
    eval.push_str(&format!(
        "{{ok, P}} = file:read_file({}),\
         \nS = zlib:compress(P),\
         \nio:format(\"STREAM ~s ~w~n\", [Hex(S), byte_size(S)]),\
         \nZ = zlib:open(), ok = zlib:inflateInit(Z),\
         \nLoop = fun F(First, Acc) ->\
         \n        R = case First of true -> zlib:safeInflate(Z, S); false -> zlib:safeInflate(Z, []) end,\
         \n        case R of\
         \n            {{continue, Out}} -> F(false, [{{continue, Sizes(Out)}} | Acc]);\
         \n            {{finished, Out}} -> lists:reverse([{{finished, Sizes(Out)}} | Acc])\
         \n        end\
         \n      end,\
         \nSeq = Loop(true, []),\
         \n[io:format(\"CHUNK ~w ~w~n\", [Tag, Sizes]) || {{Tag, Sizes}} <- Seq],\
         \nExtra = zlib:safeInflate(Z, []),\
         \nio:format(\"EXTRA ~w ~w~n\", [element(1, Extra), Sizes(element(2, Extra))]),\
         \nok = zlib:inflateEnd(Z), ok = zlib:close(Z),\
         \nZ2 = zlib:open(), ok = zlib:inflateInit(Z2),\
         \nFull = iolist_to_binary(zlib:inflate(Z2, S)),\
         \nok = zlib:inflateEnd(Z2), ok = zlib:close(Z2),\
         \nio:format(\"FULL ~w ~s ~w~n\", [byte_size(Full), Md5(Full), Full =:= P]),\
         \nhalt().",
        erl_string(payload)
    ));
    eval
}

#[test]
fn the_safe_inflate_chunk_sequence_matches_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        skip_banner("safeInflate chunk sequence");
        return;
    };
    let payload = payload_100k();
    let scratch = Scratch::new("safeinflate");
    let payload_path = scratch.write("payload.bin", &payload);
    let stdout = run_erl(&oracle, &eval_safe_inflate(&payload_path));

    let stream = unhex(oracle_fields(&stdout, "STREAM")[0]);
    assert_eq!(
        oracle_fields(&stdout, "STREAM")[1],
        "494",
        "the golden's 494-byte compressed payload"
    );

    // The oracle's chunk sequence.
    let oracle_sequence: Vec<(String, Vec<usize>)> = stdout
        .lines()
        .filter(|line| line.starts_with("CHUNK "))
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields[1].to_string(), parse_sizes(fields[2]))
        })
        .collect();
    assert_eq!(
        oracle_sequence,
        [
            ("continue".to_string(), vec![16384]),
            ("continue".to_string(), vec![16384]),
            ("continue".to_string(), vec![16384]),
            ("continue".to_string(), vec![16384]),
            ("continue".to_string(), vec![16384]),
            ("continue".to_string(), vec![16384]),
            ("finished".to_string(), vec![1696]),
        ]
        .to_vec(),
        "the golden's `{{continue,[16384]}} ×6` then `{{finished,[1696]}}`"
    );
    let extra = oracle_fields(&stdout, "EXTRA");
    assert_eq!(extra[0], "finished", "the trailing `safeInflate(Z, [])`");
    assert_eq!(parse_sizes(extra[1]), Vec::<usize>::new(), "and its `[]`");

    // This crate's stream and chunk sequence.
    let mut fx = Fixture::new();
    let handle = open(&mut fx);
    deflate_init(&mut fx, handle, -1, 15, STRATEGY_DEFAULT).expect("deflateInit");
    let packed = zlib_deflate(&mut fx, handle, &payload, FINISH).expect("deflate");
    deflate_end(&mut fx, handle).expect("deflateEnd");
    close(&mut fx, handle).expect("close");
    assert_eq!(
        packed.bytes, stream,
        "the 100 KiB payload compresses to the oracle's bytes"
    );

    let handle = open(&mut fx);
    inflate_init(&mut fx, handle, 15, EOS_CUT).expect("inflateInit");
    let mut ours: Vec<(String, Vec<usize>)> = Vec::new();
    let mut replied = false;
    let mut first = true;
    while !replied {
        let data: &[u8] = if first { &stream } else { &[] };
        first = false;
        match zlib_safe_inflate(&mut fx, handle, data).expect("safeInflate") {
            Answer::Continue(leaves, _) => ours.push(("continue".to_string(), leaves)),
            Answer::Finished(leaves, _) => {
                ours.push(("finished".to_string(), leaves));
                replied = true;
            }
            Answer::NeedDictionary(..) | Answer::Bare(_) => {
                panic!("the 100 KiB sequence has no dictionary and no bare atom")
            }
        }
        assert!(ours.len() <= 16, "the loop terminates");
    }
    assert_eq!(ours, oracle_sequence, "the whole chunk sequence");
    match zlib_safe_inflate(&mut fx, handle, &[]).expect("the trailing safeInflate") {
        Answer::Finished(leaves, bytes) => {
            assert!(leaves.is_empty() && bytes.is_empty(), "the trailing `{{finished,[]}}`");
        }
        _ => panic!("the trailing call answers `{{finished,[]}}`"),
    }
    inflate_end(&mut fx, handle).expect("inflateEnd");
    close(&mut fx, handle).expect("close");

    // Its one-shot `inflate/2` round trip.
    let full = oracle_fields(&stdout, "FULL");
    assert_eq!(full[0], "100000", "the round trip's byte count");
    assert_eq!(full[2], "true", "the oracle recovers the payload");
    let handle = open(&mut fx);
    inflate_init(&mut fx, handle, 15, EOS_CUT).expect("inflateInit");
    match zlib_inflate(&mut fx, handle, &stream, true).expect("inflate") {
        Inflated::Data(bytes) => {
            assert_eq!(bytes.len(), 100_000, "the round trip's byte count");
            assert_eq!(md5(&bytes), full[1].to_ascii_uppercase(), "its md5");
            assert_eq!(bytes, payload, "and the payload itself");
        }
        Inflated::NeedDictionary { adler, .. } => panic!("unexpected need_dictionary {adler}"),
    }
    inflate_end(&mut fx, handle).expect("inflateEnd");
    close(&mut fx, handle).expect("close");
}

// ---------------------------------------------------------------------------
// The guard classes
// ---------------------------------------------------------------------------

/// The guard steps, in the order both sides perform them, indexed by [`GUARD_STEPS`].
const GUARD_STEPS: &[&str] = &[
    "deflateEnd after a `none` flush",
    "deflateEnd with no deflate call",
    "a `sync` flush after `finish`",
    "a repeated `finish` after `finish`",
    "deflate on an inflate stream",
    "a second deflateInit",
    "inflateEnd on a truncated stream",
];

fn eval_guards() -> String {
    let mut eval = String::from(ORACLE_PRELUDE);
    eval.push_str(
        "G = fun(V) -> case V of\
         \n        ok -> \"ok\";\
         \n        empty -> \"empty\";\
         \n        error -> \"error\";\
         \n        A when is_atom(A) -> \"atom:\" ++ atom_to_list(A);\
         \n        _ when is_list(V); is_binary(V) -> \"data\";\
         \n        Other -> \"other:\" ++ lists:flatten(io_lib:format(\"~w\", [Other]))\
         \n    end end,\
         \nT = fun(F) -> try F() of V -> G(V) catch error:R -> case R of\
         \n        A when is_atom(A) -> \"raise:\" ++ atom_to_list(A);\
         \n        _ -> \"raise:\" ++ lists:flatten(io_lib:format(\"~w\", [R]))\
         \n    end end end,\
         \nZero = fun() -> Z = zlib:open(), ok = zlib:deflateInit(Z, default), Z end,\
         \nOne = fun() -> Z = zlib:open(), ok = zlib:inflateInit(Z), Z end,\
         \nZ1 = Zero(), _ = zlib:deflate(Z1, <<\"hello\">>, none),\
         \nio:format(\"G0 ~s~n\", [T(fun() -> zlib:deflateEnd(Z1) end)]),\
         \nZ2 = Zero(),\
         \nio:format(\"G1 ~s~n\", [T(fun() -> zlib:deflateEnd(Z2) end)]),\
         \nZ3 = Zero(), _ = zlib:deflate(Z3, <<\"hello\">>, finish),\
         \nio:format(\"G2 ~s~n\", [T(fun() -> zlib:deflate(Z3, <<\"more\">>, sync) end)]),\
         \nZ4 = Zero(), _ = zlib:deflate(Z4, <<\"hello\">>, finish),\
         \nio:format(\"G3 ~s~n\", [T(fun() -> zlib:deflate(Z4, [], finish) end)]),\
         \nZ5 = One(),\
         \nio:format(\"G4 ~s~n\", [T(fun() -> zlib:deflate(Z5, <<\"x\">>, none) end)]),\
         \nZ6 = Zero(),\
         \nio:format(\"G5 ~s~n\", [T(fun() -> zlib:deflateInit(Z6, default) end)]),\
         \nZ7 = One(), S7 = zlib:compress(<<\"hello hello hello\">>),\
         \n_ = zlib:inflate(Z7, binary:part(S7, 0, byte_size(S7) - 4)),\
         \nio:format(\"G6 ~s~n\", [T(fun() -> zlib:inflateEnd(Z7) end)]),\
         \nhalt().",
    );
    eval
}

#[test]
fn the_guard_classes_match_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        skip_banner("guard classes");
        return;
    };
    let stdout = run_erl(&oracle, &eval_guards());
    let mut oracle_tokens = Vec::new();
    for index in 0..GUARD_STEPS.len() {
        let key = format!("G{index}");
        let fields = oracle_fields(&stdout, &key);
        assert_eq!(fields.len(), 1, "one token per guard step: `{key}`");
        oracle_tokens.push(fields[0].to_string());
    }

    let mut fx = Fixture::new();
    let mut ours = Vec::new();

    // `deflateEnd` after a `none` flush.
    let handle = open(&mut fx);
    deflate_init(&mut fx, handle, -1, 15, STRATEGY_DEFAULT).expect("deflateInit");
    zlib_deflate(&mut fx, handle, b"hello", NONE).expect("deflate");
    let ended = deflate_end(&mut fx, handle);
    ours.push(value_token(&fx, ended));
    close(&mut fx, handle).expect("close");

    // `deflateEnd` with no deflate call at all.
    let handle = open(&mut fx);
    deflate_init(&mut fx, handle, -1, 15, STRATEGY_DEFAULT).expect("deflateInit");
    let ended = deflate_end(&mut fx, handle);
    ours.push(value_token(&fx, ended));
    close(&mut fx, handle).expect("close");

    // A `sync` flush after `finish`.
    let handle = open(&mut fx);
    deflate_init(&mut fx, handle, -1, 15, STRATEGY_DEFAULT).expect("deflateInit");
    zlib_deflate(&mut fx, handle, b"hello", FINISH).expect("deflate finish");
    ours.push(call_token(zlib_deflate(&mut fx, handle, b"more", SYNC)));
    close(&mut fx, handle).expect("close");

    // A repeated `finish` after `finish` — not an error: zlib stays in `Z_STREAM_END`.
    // The oracle's probe uses its own fresh stream here, so mirror that.
    let handle = open(&mut fx);
    deflate_init(&mut fx, handle, -1, 15, STRATEGY_DEFAULT).expect("deflateInit");
    zlib_deflate(&mut fx, handle, b"hello", FINISH).expect("deflate finish");
    ours.push(call_token(zlib_deflate(&mut fx, handle, &[], FINISH)));
    close(&mut fx, handle).expect("close");

    // `deflate` on an inflate stream.
    let handle = open(&mut fx);
    inflate_init(&mut fx, handle, 15, EOS_CUT).expect("inflateInit");
    ours.push(call_token(zlib_deflate(&mut fx, handle, b"x", NONE)));
    close(&mut fx, handle).expect("close");

    // A second `deflateInit` without an end.
    let handle = open(&mut fx);
    deflate_init(&mut fx, handle, -1, 15, STRATEGY_DEFAULT).expect("deflateInit");
    let again = deflate_init(&mut fx, handle, -1, 15, STRATEGY_DEFAULT);
    ours.push(value_token(&fx, again));
    close(&mut fx, handle).expect("close");

    // `inflateEnd` on a truncated stream: the only place a short stream is detected.
    let handle = open(&mut fx);
    inflate_init(&mut fx, handle, 15, EOS_CUT).expect("inflateInit");
    let compressed = {
        // The oracle's own `zlib:compress` bytes, taken from the boot above is not
        // available here, so the stream is built with this crate's wrapper sequence.
        let inner = open(&mut fx);
        deflate_init(&mut fx, inner, -1, 15, STRATEGY_DEFAULT).expect("deflateInit");
        let packed = zlib_deflate(&mut fx, inner, b"hello hello hello", FINISH).expect("deflate");
        deflate_end(&mut fx, inner).expect("deflateEnd");
        close(&mut fx, inner).expect("close");
        packed.bytes
    };
    let truncated = &compressed[..compressed.len() - 4];
    match zlib_inflate(&mut fx, handle, truncated, true).expect("inflate of a truncated stream") {
        Inflated::Data(_) => {}
        Inflated::NeedDictionary { adler, .. } => panic!("unexpected need_dictionary {adler}"),
    }
    let ended = inflate_end(&mut fx, handle);
    ours.push(value_token(&fx, ended));
    close(&mut fx, handle).expect("close");

    assert_eq!(
        ours.len(),
        GUARD_STEPS.len(),
        "every guard step produced a token"
    );
    for ((step, ours), oracle) in GUARD_STEPS.iter().zip(&ours).zip(&oracle_tokens) {
        assert_eq!(
            ours, oracle,
            "the guard class of `{step}` differs from the oracle"
        );
    }
    println!("zlib_stream guards: {} steps agree with the oracle", ours.len());
    println!(
        "zlib_stream guard tokens: {}",
        oracle_tokens.join(" ")
    );
}

// ---------------------------------------------------------------------------
// The three stash keys (local to `zlib.beam`, so oracle-unreachable by design)
// ---------------------------------------------------------------------------

/// The `*Stash_nif` keys are **not exported** by `zlib.beam` (`zlib.erl` declares them as
/// local stubs), so no `-eval` can call them and no live transcript exists for their
/// shapes. Their contract is the one the two live paths above exercise —
/// `deflateParams/3` (`save_progress` → `setStash_nif` on an empty stash) and the
/// dictionary protocol (`save_progress`/`restore_progress` → `getStash_nif` +
/// `clearStash_nif`) — plus the violation classes the C returns for a second
/// `setStash` / a `clearStash` of an empty stash.
///
/// **These expectations are C-derived, not oracle-checked.** The three keys are
/// non-exported NIFs of the preloaded `zlib.beam`, so neither an `-eval` nor a compiled
/// module outside `zlib` can reach them (`erlang:apply(zlib, getStash_nif, [H])` is
/// `undef`), and the shapes below are read off `zlib_nif.c` plus the state machine:
///
/// * `zlib_getStash`/`zlib_setStash`/`zlib_clearStash` (`zlib_nif.c:473-522`) test only
///   the handle (`get_zlib_data`), the controlling process (`zlib_process_check`) and
///   their own **stash** state — there is **no** `d->state` test, unlike `zlib_close`
///   (`:574`) or the codec keys. So a **closed** stream answers the three keys exactly
///   as a live one does; only a second `close` is `not_initialized` (the tombstone).
/// * `zlib_internal_close` (`:280-292`) drops `d->stash_env` with the rest of the
///   stream, so after `close` the stash is empty again: `getStash_nif` ⇒ `empty`,
///   `clearStash_nif` ⇒ the bare-atom `error` raise (`:496-498`), and `setStash_nif`
///   ⇒ `ok` (a *fresh* stash, no second-stash `error`) with the later `getStash_nif`
///   ⇒ `{ok, Term}`.
#[test]
fn the_stash_keys_keep_their_documented_shapes() {
    let mut fx = Fixture::new();
    let handle = open(&mut fx);

    // A fresh stream has no stash.
    assert_eq!(get_stash(&mut fx, handle).expect("getStash"), fx.atom("empty"));

    // `clearStash` of an empty stash is the bare atom `error`.
    assert_eq!(
        raise_token(clear_stash(&mut fx, handle).expect_err("clearStash of an empty stash")),
        "raise:error"
    );

    // `setStash` stores the term, and `getStash` hands it back as `{ok, Term}`.
    let stored = progress_term(&mut fx, "deflate", b"progress");
    assert_eq!(set_stash(&mut fx, handle, stored).expect("setStash"), fx.atom("ok"));
    let fetched = get_stash(&mut fx, handle).expect("getStash");
    let outer = fx.tuple(fetched);
    assert_eq!(outer.len(), 2, "`getStash` answers `{{ok, Term}}`");
    assert_eq!(outer[0], fx.atom("ok"));
    let inner = fx.tuple(outer[1]);
    assert_eq!(inner.len(), 2, "the stash holds `{{Kind, Output}}`");
    assert_eq!(inner[0], fx.atom("deflate"));
    assert_eq!(fx.iolist_bytes(inner[1]), b"progress");

    // A second `setStash` on an occupied stash is the bare atom `error`.
    assert_eq!(
        raise_token(set_stash(&mut fx, handle, stored).expect_err("a second setStash")),
        "raise:error"
    );

    // `clearStash` releases it; the next `getStash` is `empty` again.
    assert_eq!(clear_stash(&mut fx, handle).expect("clearStash"), fx.atom("ok"));
    assert_eq!(get_stash(&mut fx, handle).expect("getStash"), fx.atom("empty"));

    // `close` drops the stash with the rest of the stream (`zlib_internal_close`), and
    // the three stash keys carry **no** state precondition (`zlib_nif.c:473-522`), so a
    // closed stream keeps answering them: the stash reads back `empty`, `clearStash` of
    // an empty stash is the bare atom `error`, and a fresh `setStash` still works.
    let stored = progress_term(&mut fx, "inflate", b"progress");
    assert_eq!(set_stash(&mut fx, handle, stored).expect("setStash"), fx.atom("ok"));
    assert_eq!(close(&mut fx, handle).expect("close"), fx.atom("ok"));
    assert_eq!(
        get_stash(&mut fx, handle).expect("getStash after close"),
        fx.atom("empty"),
        "`close` emptied the stash, and `getStash` has no state precondition"
    );
    assert_eq!(
        raise_token(clear_stash(&mut fx, handle).expect_err("clearStash after close")),
        "raise:error",
        "an empty stash is the bare atom `error`, not a state raise"
    );
    let restored = progress_term(&mut fx, "inflate", b"again");
    assert_eq!(
        set_stash(&mut fx, handle, restored).expect("setStash after close"),
        fx.atom("ok"),
        "a closed stream still stashes (the C stores it in its own env)"
    );
    let fetched = get_stash(&mut fx, handle).expect("getStash of the fresh stash");
    let outer = fx.tuple(fetched);
    assert_eq!(outer.len(), 2, "`getStash` answers `{{ok, Term}}`");
    assert_eq!(outer[0], fx.atom("ok"));
    assert_eq!(fx.iolist_bytes(fx.tuple(outer[1])[1]), b"again");
    assert_eq!(
        clear_stash(&mut fx, handle).expect("clearStash of the fresh stash"),
        fx.atom("ok")
    );
    assert_eq!(get_stash(&mut fx, handle).expect("getStash"), fx.atom("empty"));
    // Only `close` itself carries the state test (`zlib_nif.c:574`).
    assert_eq!(
        raise_token(close(&mut fx, handle).expect_err("a second close")),
        "raise:not_initialized"
    );
}
