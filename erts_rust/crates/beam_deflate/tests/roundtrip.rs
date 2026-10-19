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
//! **The pump loop — the `zlib` NIF layer's shape, reproduced in test code.**
//!
//! The second consumer of this crate is `beam_bifs`: the `zlib` module
//! (`erts/preloaded/src/zlib.erl`) over the NIF surface in
//! `erts/emulator/nifs/common/zlib_nif.c`. Its observable behaviour is not just "the same
//! bytes" — the *chunking* of its `{continue, Out}` / `{finished, Out}` /
//! `{need_dictionary, Adler, Out}` answers is part of the Erlang API, and at level 0 the
//! deflate/LZ77 decisions depend on the call schedule itself (zlib's `deflate_stored` sizes
//! its blocks from `avail_in`/`avail_out`).
//!
//! So this file re-implements the NIF's scheduler faithfully (`Nif`, below) — the input
//! queue, the two chunk sizes, the `Z_NO_FLUSH`-per-piece loop, the drain-then-flush call
//! and the C's exact classification — and drives it through both directions of this crate:
//!
//! ```text
//! zlib_codec(env, d, in_chunk, out_chunk, flush):          zlib_nif.c:419-468
//!   out = alloc_binary(out_chunk)
//!   zlib_flush_queue(...):                                 zlib_nif.c:339-404
//!     while res == Z_OK && more pieces && consumed < in_chunk:
//!         block = min(piece_len, in_chunk)
//!         res = codec(strm, Z_NO_FLUSH)          # next_in = block
//!     deq(consumed)                              # the queue keeps the tail
//!     if res == Z_OK && nothing remains && avail_out > 0:
//!         res = codec(strm, flush)               # next_in = NULL, avail_in = 0
//!   # classification, in the C's order:
//!   if consumed == 0 && produced == 0 && something remains:  -> Z_DATA_ERROR
//!   if nothing remains && produced < out_chunk:              -> {finished, Out}
//!   else if res != Z_NEED_DICT:                              -> {continue, Out}
//!   else                                                     -> {need_dictionary, Adler, Out}
//! ```
//!
//! and the Erlang-level wrappers on top of it: `deflate/3` / `inflate/2`
//! (`dequeue_all_chunks`, i.e. drain until `finished`) and `safeInflate/2`
//! (`dequeue_next_chunk`, i.e. one chunk per call).
//!
//! # Chunk sizes
//!
//! `zlib.erl` hardcodes them (`zlib.erl:141-144`):
//!
//! * deflate: `?DEFLATE_IN_CHUNKSIZE` = `?DEFLATE_OUT_CHUNKSIZE` = 8 KiB. This is the same
//!   schedule [`Deflate::compress_with`] drives, which is why that one-shot entry point is
//!   the oracle's image.
//! * inflate: `?INFLATE_IN_CHUNKSIZE` = 8 KiB in, `?INFLATE_OUT_CHUNKSIZE` = 16 KiB out.
//!   The 16 KiB input-piece variant is exercised too — inflate's *output* never depends on
//!   the input chunking, only its call sequence does.
//!
//! # What is pinned
//!
//! * the deflate pump concatenated equals [`Deflate::compress_with`] at levels `0,1,6,9`
//!   in all three framings, and equals the **oracle**'s image (level 0 included: the
//!   schedule is byte-visible there, and the assertion is not vacuous — a wide-buffer
//!   schedule produces a *different* stream, which the test also shows);
//! * the inflate pump reassembles the payload and reaches `Z_STREAM_END` (the NIF's
//!   `eos_seen`, which is what `inflateEnd/1` checks) for sizes that do and do not divide
//!   the 16 KiB output chunk;
//! * `deflate(Z, Part1, none)` is not yet a complete stream, `deflate(Z, Part2, finish)`
//!   completes it, and the joined output decodes to `Part1 ++ Part2`;
//! * a `sync` flush is a resumption point: the `00 00 FF FF` marker ends the prefix, a
//!   *fresh* inflater reads exactly `Part1` out of it and knows it is not at the stream end,
//!   and the same stream then finishes;
//! * a `safeInflate/2`-shaped loop (`{continue, Out}` … `{finished, Out}`) reassembles the
//!   plaintext;
//! * `deflateParams` in the Erlang layer's shape (`deflate(Z, <<>>, sync)` then
//!   `deflateParams/3` against the NIF's one-byte dummy buffer) is accepted with
//!   `Status::Ok` and nothing pending, and the resulting stream decodes — and equals the
//!   oracle's own `deflateParams` image;
//! * a preset dictionary through both directions: the deflate side reports the dictionary's
//!   Adler-32, a fresh inflater asks for it by that same id, and taking it decodes;
//! * the whole round-trip matrix (eleven levels × three framings × six inputs) through the
//!   one-shot entries *and* both pumps.

mod zsupport;

use std::collections::VecDeque;

use beam_common::{BeamError, Result};
use beam_deflate::{
    adler32, decompress_all, Deflate, DeflateConfig, Flush, Inflate, Pump, Status, Strategy,
};
use zsupport::hex;

// ---------------------------------------------------------------------------
// The NIF's pump (`zlib_nif.c`), in test code
// ---------------------------------------------------------------------------

/// `?DEFLATE_IN_CHUNKSIZE`/`?DEFLATE_OUT_CHUNKSIZE` — 8 KiB each (`zlib.erl:141-142`), the
/// schedule [`Deflate::compress_with`] reproduces.
const DEFLATE_SCHEDULE: Schedule = (8 << 10, 8 << 10);

/// `?INFLATE_IN_CHUNKSIZE`/`?INFLATE_OUT_CHUNKSIZE` — 8 KiB in, 16 KiB out
/// (`zlib.erl:143-144`), the schedule `inflate/2` and `safeInflate/2` use.
const INFLATE_SCHEDULE: Schedule = (8 << 10, 16 << 10);

/// `?MAX_WBITS` for a zlib (RFC 1950) stream — `inflateInit/1`'s default.
const ZLIB_WBITS: i8 = beam_deflate::MAX_WBITS;
/// `?MAX_WBITS` negated: raw RFC 1951 (`zlib:zip/1`).
const RAW_WBITS: i8 = -beam_deflate::MAX_WBITS;
/// `16 + ?MAX_WBITS`: a gzip member (`zlib:gzip/1`).
const GZIP_WBITS: i8 = beam_deflate::GZIP_WBITS_OFFSET + beam_deflate::MAX_WBITS;

/// `Z_SYNC_FLUSH`'s marker: an empty stored block, byte-aligned — `deflate.c`'s
/// `_tr_stored_block(s, 0, 0, 0)` after `bi_windup`.
const SYNC_MARKER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// The NIF's two chunk sizes: `(input_chunk_size, output_chunk_size)`.
type Schedule = (usize, usize);

/// One `deflate()`/`inflate()` call. The C's pump is a single loop because both directions
/// are `int (*codec)(z_stream *, int)`; this trait is that function pointer.
trait Codec {
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: Flush) -> Result<Pump>;
}

impl Codec for Deflate {
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: Flush) -> Result<Pump> {
        self.deflate(input, output, flush)
    }
}

impl Codec for Inflate {
    fn step(&mut self, input: &[u8], output: &mut [u8], flush: Flush) -> Result<Pump> {
        self.inflate(input, output, flush)
    }
}

/// `zlib_codec`'s verdict — the tuple the NIF hands `zlib.erl`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// `{finished, Out}`.
    Finished,
    /// `{continue, Out}`.
    Continue,
    /// `{need_dictionary, Adler, Out}`.
    NeedDict(u32),
}

/// One `zlib_codec` call: the verdict, the bytes it produced, and the raw zlib status the
/// classification folded away (`Z_STREAM_END` is the NIF's `eos_seen`; the rest is
/// informative).
#[derive(Debug)]
struct Verdict {
    kind: Kind,
    out: Vec<u8>,
    status: Status,
}

/// The stream state `zlib_data_t` holds: the codec, its input queue and the two chunk
/// sizes.
struct Nif<C: Codec> {
    codec: C,
    /// `d->input_queue` — `enif_ioq_peek`/`enif_ioq_deq` as a byte queue. An empty piece is
    /// never enqueued (`enif_inspect_iovec` of `<<>>` yields no vectors), which also keeps
    /// the pump from spinning on an empty front piece.
    queue: VecDeque<Vec<u8>>,
    /// Bytes already consumed from `queue.front()`.
    head: usize,
    /// Bytes still queued (`enif_ioq_size/1`).
    queued: usize,
    /// `input_chunk_size` — the most one NIF call may consume.
    in_chunk: usize,
    /// `output_chunk_size` — the buffer each NIF call allocates.
    out_chunk: usize,
    /// `d->eos_seen` — `Z_STREAM_END` was reported at some point. `inflateEnd/1` refuses a
    /// stream that never saw it.
    eos_seen: bool,
}

/// One Erlang-level call's answer. `need_dict` is the Adler-32 `inflate/2` turns into
/// `error:{need_dictionary, Adler}` — the Erlang layer stashes the bytes collected before
/// it and returns them from the next call.
struct Call {
    out: Vec<u8>,
    need_dict: Option<u32>,
}

impl<C: Codec> Nif<C> {
    fn new(codec: C, (in_chunk, out_chunk): Schedule) -> Nif<C> {
        assert!(
            in_chunk > 0 && out_chunk > 0,
            "the NIF's chunk sizes are non-zero ({in_chunk}/{out_chunk})"
        );
        Nif {
            codec,
            queue: VecDeque::new(),
            head: 0,
            queued: 0,
            in_chunk,
            out_chunk,
            eos_seen: false,
        }
    }

    /// `zlib_enqueue_input/2` (`enif_ioq_enqv`).
    fn enqueue(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.queued += data.len();
        self.queue.push_back(data.to_vec());
    }

    /// `enif_ioq_size/1`.
    fn remaining(&self) -> usize {
        self.queued
    }

    /// `enif_ioq_deq/3` — drop `n` consumed bytes from the front of the queue.
    fn deq(&mut self, n: usize) {
        assert!(n <= self.queued, "deq of {n} from a queue of {}", self.queued);
        self.queued -= n;
        self.head += n;
        while self
            .queue
            .front()
            .is_some_and(|front| self.head == front.len())
        {
            self.queue.pop_front();
            self.head = 0;
        }
    }

    /// `zlib_flush_queue` — one NIF call's worth of `deflate()`/`inflate()` calls: up to
    /// `input_chunk_size` bytes fed with `Z_NO_FLUSH`, then, once the queue is drained and
    /// the output buffer still has room, one call with `next_in = NULL` and the caller's
    /// flush mode.
    ///
    /// Returns `(status, consumed, produced)`; `remaining()` is the queue's size afterwards.
    fn flush_queue(&mut self, out: &mut [u8], flush: Flush) -> Result<(Status, usize, usize)> {
        let mut produced = 0usize;
        let mut consumed = 0usize;
        let mut status = Status::Ok;
        while status == Status::Ok && consumed < self.in_chunk {
            let Some(front) = self.queue.front().map(|piece| piece.len() - self.head) else {
                break;
            };
            // `block_size = MIN(input_vec[vec_idx].iov_len, input_limit)` — the piece, capped
            // at this call's input budget. `enif_ioq_peek` hands the codec a pointer into the
            // queue, so the peek and the codec borrow disjoint fields.
            let block = front.min(self.in_chunk);
            let piece = self.queue.front().expect("a non-empty front piece");
            let pump = self.codec.step(
                &piece[self.head..self.head + block],
                &mut out[produced..],
                Flush::None,
            )?;            // The C's own assertion (`zlib_nif.c:368`):
            // `avail_in == 0 || avail_out == 0 || res != Z_OK`. It is what licenses the
            // per-iteration `deq` below: only a non-`Z_OK` call may leave a block
            // half-consumed. If this ever fires the port has changed zlib's progress rules.
            assert!(
                pump.consumed == block
                    || produced + pump.produced == out.len()
                    || pump.status != Status::Ok,
                "a Z_OK call consumed {}/{} and produced {} of {}",
                pump.consumed,
                block,
                pump.produced,
                out.len() - produced
            );            consumed += pump.consumed;
            produced += pump.produced;
            status = pump.status;
            // The C reaches the next iovec with `vec_idx++` and deqs at the end; dequeuing
            // per iteration is equivalent because a *partially* consumed block can only
            // happen when the call did not return `Z_OK`, which ends the loop.
            self.deq(pump.consumed);
        }
        if status == Status::Ok && self.remaining() == 0 && produced < out.len() {
            // "We've reached the end of our input and need to flush the zlib state. Note
            // that we do this even when the flush parameter is Z_NO_FLUSH" — the C's
            // comment; it is why one `deflate(Z, Data, finish)` call needs a second NIF
            // call to hear `Z_STREAM_END` when the first one exactly filled the buffer.
            let pump = self.codec.step(&[], &mut out[produced..], flush)?;
            produced += pump.produced;
            status = pump.status;
        }
        Ok((status, consumed, produced))
    }

    /// `zlib_codec` — allocate one `output_chunk_size` buffer, run the queue through it and
    /// classify the answer.
    fn drain(&mut self, flush: Flush) -> Result<Verdict> {
        let mut out = vec![0u8; self.out_chunk];
        let (status, consumed, produced) = self.flush_queue(&mut out, flush)?;
        if status == Status::StreamEnd {
            self.eos_seen = true;
        }
        let full = produced == self.out_chunk;
        out.truncate(produced);
        // The C's zero-progress guard: "Die if we've made zero progress; this should not
        // happen on well-formed input" → `Z_DATA_ERROR`, which `zlib.erl` raises as
        // `data_error`.
        if consumed == 0 && produced == 0 && self.remaining() != 0 {
            return Err(BeamError::BadArg);
        }
        // The classification, in the C's own order. Note that a drained queue with a buffer
        // that was *not* filled is `finished` whatever the status says — the status only
        // decides whether `eos_seen` was set.
        let kind = if !full && self.remaining() == 0 {
            Kind::Finished
        } else if let Status::NeedDict(adler) = status {
            Kind::NeedDict(adler)
        } else {
            Kind::Continue
        };
        Ok(Verdict { kind, out, status })
    }

    /// `zlib:deflate/3` and `zlib:inflate/2`: enqueue `data`, then drain
    /// (`dequeue_all_chunks`) until `finished` (or `need_dictionary`).
    fn call(&mut self, data: &[u8], flush: Flush) -> Result<Call> {
        self.enqueue(data);
        let mut out = Vec::new();
        loop {
            let verdict = self.drain(flush)?;
            out.extend_from_slice(&verdict.out);
            match verdict.kind {
                Kind::Finished => return Ok(Call { out, need_dict: None }),
                Kind::Continue => {}
                Kind::NeedDict(adler) => {
                    return Ok(Call {
                        out,
                        need_dict: Some(adler),
                    })
                }
            }
        }
    }

    /// `zlib:safeInflate/2`: enqueue `data`, then take exactly one chunk
    /// (`dequeue_next_chunk`).
    fn chunk(&mut self, data: &[u8], flush: Flush) -> Result<Verdict> {
        self.enqueue(data);
        self.drain(flush)
    }

    /// `zlib_inflateEnd`'s own condition: `(!eos_seen || ioq_size > 0)` is a `data_error`,
    /// so a clean end is a seen `Z_STREAM_END` over a drained queue.
    fn inflate_end_ok(&self) -> bool {
        self.eos_seen && self.remaining() == 0
    }
}

/// `zlib:open()` + `deflateInit2/6`.
fn deflate_nif(config: DeflateConfig, schedule: Schedule) -> Nif<Deflate> {
    Nif::new(Deflate::new(config).expect("a valid DeflateConfig"), schedule)
}

/// `zlib:open()` + `inflateInit2/3`.
fn inflate_nif(window_bits: i8, schedule: Schedule) -> Nif<Inflate> {
    Nif::new(
        Inflate::new(window_bits).expect("valid window bits"),
        schedule,
    )
}

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

/// The three framings — `zlib:compress/1`'s, `zlib:zip/1`'s and `zlib:gzip/1`'s — with the
/// `inflateInit2` window bits that read each of them back.
fn framings(level: i8) -> [(&'static str, DeflateConfig, i8); 3] {
    [
        ("zlib", DeflateConfig::zlib(level), ZLIB_WBITS),
        ("raw", DeflateConfig::raw(level), RAW_WBITS),
        ("gzip", DeflateConfig::gzip(level), GZIP_WBITS),
    ]
}

/// An inflate run instrumented the way `safeInflate/2` observes it.
struct InflateRun {
    out: Vec<u8>,
    kinds: Vec<Kind>,
    /// The NIF's `d->eos_seen`.
    eos_seen: bool,
    /// `inflateEnd/1`'s answer for this run (see [`Nif::inflate_end_ok`]).
    end_ok: bool,
}

/// `safeInflate/2`'s loop (`walk/3` in the module docs): enqueue the whole image on the
/// first call, `[]` afterwards, and take one chunk per call until `finished`.
fn pump_inflate(window_bits: i8, image: &[u8], schedule: Schedule) -> InflateRun {
    let mut nif = inflate_nif(window_bits, schedule);
    let mut out = Vec::new();
    let mut kinds = Vec::new();
    let mut feed: &[u8] = image;
    loop {
        let verdict = nif
            .chunk(feed, Flush::None)
            .expect("inflate reports statuses, never errors");
        feed = &[];
        out.extend_from_slice(&verdict.out);
        kinds.push(verdict.kind);
        match verdict.kind {
            Kind::Finished => break,
            Kind::Continue => assert!(kinds.len() < 100_000, "the pump must terminate"),
            Kind::NeedDict(adler) => panic!("unexpected need_dictionary({adler})"),
        }
    }
    InflateRun {
        out,
        kinds,
        eos_seen: nif.eos_seen,
        end_ok: nif.inflate_end_ok(),
    }
}

/// Everything a *fresh* inflater reads out of `image`, how much of it that consumed, and
/// whether it reached the end of the stream: `(plaintext, consumed, ended)`.
fn decode_prefix(window_bits: i8, image: &[u8]) -> (Vec<u8>, usize, bool) {
    let mut stream = Inflate::new(window_bits).expect("valid window bits");
    let mut buf = vec![0u8; 1 << 16];
    let mut body = Vec::new();
    let mut at = 0usize;
    loop {
        let pump = stream
            .inflate(&image[at..], &mut buf, Flush::None)
            .expect("inflate reports statuses, never errors");
        at += pump.consumed;
        body.extend_from_slice(&buf[..pump.produced]);
        if pump.consumed == 0 && pump.produced == 0 {
            break;
        }
    }
    (body, at, stream.at_stream_end())
}

/// Can a fresh inflater read `image` as a *complete* stream? `decompress_all` only returns
/// `Ok` on `Z_STREAM_END`, so a non-final prefix is always an error.
fn reaches_a_stream_end(window_bits: i8, image: &[u8]) -> bool {
    decompress_all(window_bits, image, usize::MAX).is_ok()
}

/// A deterministic pseudo-random stream (a 64-bit LCG): no `rand`, no clock, and
/// effectively incompressible, so an inflate of it really needs several 16 KiB chunks.
fn pseudo_random(n: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.push((state >> 33) as u8);
    }
    out
}

// ---------------------------------------------------------------------------
// Deflate: the pump is the NIF's, and the NIF's is the oracle's
// ---------------------------------------------------------------------------

/// The concatenated deflate pump output is [`Deflate::compress_with`]'s one-shot image at
/// levels `0,1,6,9` in all three framings — the schedule `compress_with` documents itself
/// as reproducing (`?DEFLATE_IN_CHUNKSIZE`/`?DEFLATE_OUT_CHUNKSIZE`).
#[test]
fn the_deflate_pump_reproduces_the_one_shot_image() {
    let data = zsupport::large_input(40_000);
    let mut checked = 0usize;
    for level in [0i8, 1, 6, 9] {
        for (framing, config, _) in framings(level) {
            let want = Deflate::compress_with(config, &data).expect("one-shot");
            let mut nif = deflate_nif(config, DEFLATE_SCHEDULE);
            let call = nif.call(&data, Flush::Finish).expect("the pump");
            assert_eq!(
                hex(&call.out),
                hex(&want),
                "{framing} at level {level}: the pump schedule changed the bytes"
            );
            assert!(call.need_dict.is_none());
            assert_eq!(
                nif.remaining(),
                0,
                "{framing} at level {level}: the queue drained"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 12);
    println!("{checked} pump schedules reproduce Deflate::compress_with byte for byte");
}

/// At level 0 the pump schedule is **part of the stream**, so the oracle comparison above
/// is not vacuous: `deflate_stored` copies a block straight to `next_out` once
/// `left + avail_in >= min_block` (32 KiB at memLevel 8) and the output buffer can hold it,
/// so a wide buffer emits one ~40 000-byte stored block where the NIF's 8 KiB buffers emit
/// 32 768 + 7 232.
#[test]
fn the_level_0_schedule_is_part_of_the_stream() {
    let data = zsupport::large_input(40_000);
    let mut narrow = deflate_nif(DeflateConfig::zlib(0), DEFLATE_SCHEDULE);
    let narrow = narrow.call(&data, Flush::Finish).expect("the NIF schedule").out;
    let mut wide = deflate_nif(DeflateConfig::zlib(0), (1 << 20, 1 << 20));
    let wide = wide.call(&data, Flush::Finish).expect("a wide schedule").out;

    assert_ne!(
        hex(&narrow),
        hex(&wide),
        "level 0's stored-block split must depend on avail_in/avail_out"
    );
    for (name, image) in [("nif", &narrow), ("wide", &wide)] {
        let (body, used) = decompress_all(ZLIB_WBITS, image, usize::MAX).expect("it still decodes");
        assert_eq!(body, data, "{name}: the payload");
        assert_eq!(used, image.len(), "{name}: every byte is consumed");
    }
    // The narrow one is what the oracle produces, and it is the one-shot path.
    assert_eq!(
        hex(&narrow),
        hex(&Deflate::compress_with(DeflateConfig::zlib(0), &data).expect("one-shot"))
    );
}

/// The same twelve cases against the live oracle, one `erl` boot: level 0's schedule-
/// dependent image included, which is the claim the previous test makes possible.
#[test]
fn the_deflate_pump_matches_the_live_oracle() {
    let data = zsupport::large_input(40_000);
    let mut cases = Vec::new();
    let mut configs = Vec::new();
    for level in [0i8, 1, 6, 9] {
        for (framing, config, _) in framings(level) {
            cases.push(match framing {
                "zlib" => zsupport::Case::zlib(level, &data),
                "raw" => zsupport::Case::raw(level, &data),
                _ => zsupport::Case::gzip(level, &data),
            });
            configs.push((framing, config));
        }
    }
    let Some(answers) = zsupport::oracle_batch(&cases) else {
        println!("SKIP the_deflate_pump_matches_the_live_oracle: erl not found");
        return;
    };
    let mut checked = 0usize;
    for ((case, answer), (framing, config)) in cases.iter().zip(answers).zip(configs) {
        let want = answer.expect("the oracle accepted the parameter tuple");
        let mut nif = deflate_nif(config, DEFLATE_SCHEDULE);
        let got = nif.call(&case.data, Flush::Finish).expect("the pump").out;
        assert_eq!(
            hex(&got),
            hex(&want),
            "{framing} at level {} (windowBits {})",
            case.level,
            case.window_bits
        );
        checked += 1;
    }
    assert_eq!(checked, 12);
    println!("{checked} pump schedules are byte-identical to the oracle");
}

// ---------------------------------------------------------------------------
// Inflate: the pump reassembles the payload and ends the stream
// ---------------------------------------------------------------------------

/// The inflate pump reassembles the payload, sets the NIF's `eos_seen` and leaves the queue
/// drained — the three facts `inflateEnd/1` and `safeInflate/2` turn into observable
/// behaviour.
///
/// Both sizes matter for the *classification*: 70 000 output bytes leave a partial last
/// chunk (the `finished` verdict arrives with `Z_STREAM_END` itself), while 32 768 is an
/// exact multiple of the 16 KiB output buffer (the `finished` verdict arrives on the
/// follow-up call, after a `{continue, Out}` whose call already reported `Z_STREAM_END`).
#[test]
fn the_inflate_pump_reassembles_the_payload() {
    let mut checked = 0usize;
    for size in [70_000usize, 32_768] {
        let data = zsupport::large_input(size);
        for level in [0i8, 1, 6, 9] {
            for (framing, config, window_bits) in framings(level) {
                let image = Deflate::compress_with(config, &data).expect("compress");
                let run = pump_inflate(window_bits, &image, INFLATE_SCHEDULE);
                assert_eq!(run.out, data, "{framing}/{size}/level {level}: the plaintext");
                assert_eq!(
                    run.kinds.last(),
                    Some(&Kind::Finished),
                    "{framing}/{size}/level {level}: the loop ends on `finished`"
                );
                assert!(
                    run.eos_seen,
                    "{framing}/{size}/level {level}: Z_STREAM_END was reported"
                );
                assert!(
                    run.end_ok,
                    "{framing}/{size}/level {level}: `inflateEnd/1` would accept the stream"
                );
                // Inflate's *output* never depends on the input chunking: 16 KiB pieces
                // decode the same bytes (only the call boundaries move).
                let wide = pump_inflate(window_bits, &image, (16 << 10, 16 << 10));
                assert_eq!(
                    wide.out, data,
                    "{framing}/{size}/level {level}: 16 KiB input pieces"
                );
                assert!(wide.end_ok);
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 24);
    println!("{checked} inflate runs reassembled their payload and saw the stream end");
}

/// `deflate(Z, Part1, none)` is not yet a stream a fresh inflater can finish;
/// `deflate(Z, Part2, finish)` completes it, and the joined output decodes to
/// `Part1 ++ Part2`.
#[test]
fn an_incremental_deflate_needs_its_finish_flush() {
    let data = zsupport::mixed_kib();
    let (part1, part2) = data.split_at(2048);
    let mut nif = deflate_nif(DeflateConfig::zlib(6), DEFLATE_SCHEDULE);

    let first = nif.call(part1, Flush::None).expect("deflate(Z, Part1, none)");
    assert!(first.need_dict.is_none());
    assert!(
        !reaches_a_stream_end(ZLIB_WBITS, &first.out),
        "`deflate(..., none)` must not produce a complete stream (it produced {} bytes)",
        first.out.len()
    );
    // Whatever the partial feed did release is a *prefix* of the payload, never a reordered
    // or short-but-final body.
    let (partial, _, ended) = decode_prefix(ZLIB_WBITS, &first.out);
    assert!(!ended, "a non-final flush is not the stream end");
    assert!(
        data.starts_with(&partial),
        "a partial feed released {} bytes that are not a prefix of the payload",
        partial.len()
    );

    let second = nif.call(part2, Flush::Finish).expect("deflate(Z, Part2, finish)");
    let mut image = first.out;
    image.extend_from_slice(&second.out);
    let (body, used) =
        decompress_all(ZLIB_WBITS, &image, usize::MAX).expect("the finished stream decodes");
    assert_eq!(body, data, "the joined output decodes to Part1 ++ Part2");
    assert_eq!(used, image.len(), "every byte of the joined output");
}

/// A `sync` flush is a resumption point: the prefix ends with the `00 00 FF FF` marker, a
/// *fresh* inflater reads exactly `Part1` out of it (and knows it is not at the stream end),
/// and the same stream then finishes with the rest.
#[test]
fn a_sync_flush_is_a_resumption_point() {
    let data = zsupport::mixed_kib();
    let (part1, part2) = data.split_at(2048);
    let mut nif = deflate_nif(DeflateConfig::zlib(6), DEFLATE_SCHEDULE);

    let first = nif.call(part1, Flush::Sync).expect("deflate(Z, Part1, sync)");
    assert!(first.need_dict.is_none());
    assert!(
        first.out.ends_with(&SYNC_MARKER),
        "a sync flush ends with the empty stored block 00 00 FF FF (got {:02x?})",
        &first.out[first.out.len().saturating_sub(8)..]
    );
    let (decodable, consumed, ended) = decode_prefix(ZLIB_WBITS, &first.out);
    assert_eq!(decodable, part1, "a sync flush releases everything so far");
    assert_eq!(
        consumed,
        first.out.len(),
        "and leaves nothing the decoder cannot take"
    );
    assert!(!ended, "a sync flush is not the end of the stream");

    let second = nif.call(part2, Flush::Finish).expect("deflate(Z, Part2, finish)");
    let mut image = first.out;
    image.extend_from_slice(&second.out);
    let (body, used) = decompress_all(ZLIB_WBITS, &image, usize::MAX).expect("the full stream");
    assert_eq!(body, data);
    assert_eq!(used, image.len());
    // The flush really did change the bytes, so the assertions above are about a stream
    // that carries the marker rather than about the one-shot image.
    assert_ne!(
        hex(&image),
        hex(&Deflate::compress_with(DeflateConfig::zlib(6), &data).expect("one-shot")),
        "a mid-stream flush must be visible in the bytes"
    );
}

/// `safeInflate/2`'s loop (`{continue, Out}` … `{finished, Out}`) reassembles the plaintext,
/// and an incompressible payload really does take several 16 KiB chunks.
#[test]
fn a_safe_inflate_loop_reassembles_the_plaintext() {
    let data = pseudo_random(70_000);
    let image = Deflate::compress_with(DeflateConfig::zlib(6), &data).expect("compress");
    let mut nif = inflate_nif(ZLIB_WBITS, INFLATE_SCHEDULE);
    let mut out = Vec::new();
    let mut calls = 0usize;
    let mut feed: &[u8] = &image;
    loop {
        let verdict = nif.chunk(feed, Flush::None).expect("safeInflate");
        feed = &[];
        calls += 1;
        out.extend_from_slice(&verdict.out);
        match verdict.kind {
            Kind::Finished => break,
            Kind::Continue => assert!(calls < 1000, "the loop must terminate"),
            Kind::NeedDict(adler) => panic!("unexpected need_dictionary({adler})"),
        }
    }
    assert_eq!(out, data, "the chunks concatenate to the plaintext");
    assert!(
        calls > 4,
        "a 70 000-byte incompressible payload needs several chunks (took {calls})"
    );
    assert!(nif.inflate_end_ok(), "`inflateEnd/1` would accept the stream");
}

// ---------------------------------------------------------------------------
// `deflateParams` in the Erlang layer's shape
// ---------------------------------------------------------------------------

/// `zlib:deflateParams/3` is `deflate(Z, <<>>, sync)` followed by `deflateParams/3`, which
/// the NIF runs against a **one-byte** dummy output buffer and reports as
/// `Z_STREAM_ERROR` if that buffer fills (`zlib_nif.c:716-755`).
///
/// So the contract is: after the sync pump the parameter change returns `Status::Ok` (not
/// `BufError`, and not a silent fill of the dummy buffer), and the stream still decodes to
/// the whole payload. The bytes are also compared with the oracle's own `deflateParams`
/// image when `erl` is present.
#[test]
fn deflate_params_after_a_sync_pump_is_accepted() {
    let data = zsupport::mixed_kib();
    let (part1, part2) = data.split_at(2048);
    let cases: [(i8, i8, Strategy, u8); 3] = [
        (6, 1, Strategy::Default, 0),
        (1, 9, Strategy::Default, 0),
        (6, 2, Strategy::HuffmanOnly, 2),
    ];
    let oracle_available = zsupport::have_oracle();
    for (init_level, new_level, strategy, strategy_index) in cases {
        let mut nif = deflate_nif(DeflateConfig::zlib(init_level), DEFLATE_SCHEDULE);
        let mut image = nif
            .call(part1, Flush::Sync)
            .expect("deflate(Z, Part1, sync)")
            .out;
        // The Erlang layer's own `deflate(Z, <<>>, sync)`: byte-aligns the stream with
        // nothing pending, so `deflateParams`' `Z_BLOCK` has nothing to flush.
        image.extend_from_slice(
            &nif.call(&[], Flush::Sync)
                .expect("deflate(Z, <<>>, sync)")
                .out,
        );

        let mut dummy = [0u8; 1];
        let step = nif
            .codec
            .set_params(new_level, strategy, &mut dummy)
            .expect("deflateParams");
        assert_eq!(
            step.status,
            Status::Ok,
            "{init_level} -> {new_level}/{strategy:?}: the C's `deflateParams` returns Z_OK \
             whenever the change applied"
        );
        assert_eq!(step.consumed, 0, "deflateParams takes no input");
        assert_eq!(
            step.produced, 0,
            "{init_level} -> {new_level}/{strategy:?}: nothing was pending, so the one-byte \
             dummy buffer must stay untouched (the C turns a filled one into Z_STREAM_ERROR)"
        );

        image.extend_from_slice(
            &nif.call(part2, Flush::Finish)
                .expect("deflate(Z, Part2, finish)")
                .out,
        );
        let (body, used) = decompress_all(ZLIB_WBITS, &image, usize::MAX)
            .unwrap_or_else(|e| panic!("{init_level} -> {new_level}/{strategy:?}: {e:?}"));
        assert_eq!(body, data, "{init_level} -> {new_level}/{strategy:?}: the payload");
        assert_eq!(used, image.len());

        if oracle_available {
            let want = zsupport::oracle_deflate_params(
                init_level,
                part1,
                new_level,
                strategy_index,
                part2,
            )
            .expect("the oracle produced a deflateParams image");
            assert_eq!(
                hex(&image),
                hex(&want),
                "{init_level} -> {new_level}/{strategy:?}: the oracle's own deflateParams image"
            );
        } else {
            println!("SKIP the deflateParams oracle cross-check: erl not found");
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Preset dictionaries, both directions
// ---------------------------------------------------------------------------

/// `deflateSetDictionary` → deflate → a fresh inflater reports `NeedDict` with the
/// dictionary's Adler-32 → `inflateSetDictionary` → the payload.
#[test]
fn a_preset_dictionary_round_trips_through_both_directions() {
    let dictionary: &[u8] = b"the quick brown fox jumps over the lazy dog";
    let data = zsupport::mixed_kib();
    let adler = adler32(1, dictionary);

    let mut deflater = deflate_nif(DeflateConfig::zlib(6), DEFLATE_SCHEDULE);
    assert_eq!(
        deflater
            .codec
            .set_dictionary(dictionary)
            .expect("deflateSetDictionary"),
        adler,
        "`deflateSetDictionary` returns the dictionary's Adler-32"
    );
    let image = deflater
        .call(&data, Flush::Finish)
        .expect("a dictionary stream compresses")
        .out;
    // A dictionary stream is not a valid one-shot payload: the ETF `COMPRESSED` and
    // `zlib:uncompress/1` shapes have no way to supply the dictionary.
    assert!(
        matches!(
            decompress_all(ZLIB_WBITS, &image, usize::MAX),
            Err(BeamError::BadArg)
        ),
        "a preset-dictionary stream cannot be decoded without its dictionary"
    );

    // The streaming route asks for it by id, with nothing produced yet.
    let mut inflater = inflate_nif(ZLIB_WBITS, INFLATE_SCHEDULE);
    let verdict = inflater.chunk(&image, Flush::None).expect("inflate");
    assert_eq!(
        verdict.kind,
        Kind::NeedDict(adler),
        "the same Adler-32 the compressor reported"
    );
    assert_eq!(verdict.status, Status::NeedDict(adler));
    assert!(
        verdict.out.is_empty(),
        "nothing can be produced before the dictionary arrives"
    );
    assert!(inflater.remaining() > 0, "the body is still queued");

    // `inflateSetDictionary` then a retry with no new input — what `zlib:safeInflate/2`'s
    // documentation prescribes.
    inflater
        .codec
        .set_dictionary(dictionary)
        .expect("inflateSetDictionary");
    let retry = inflater.call(&[], Flush::None).expect("the retry decodes");
    assert!(retry.need_dict.is_none());
    assert_eq!(retry.out, data, "the dictionary made the difference");
    assert!(
        inflater.inflate_end_ok(),
        "`inflateEnd/1` would accept the stream"
    );
}

// ---------------------------------------------------------------------------
// The whole matrix, through both pumps
// ---------------------------------------------------------------------------

/// Levels `-1, 0..=9` × the three framings × six inputs, decoded three ways: the library's
/// one-shot `decompress_all`, the deflate pump compared with the one-shot image, and the
/// inflate pump.
#[test]
fn the_round_trip_matrix_is_exact() {
    let inputs: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("abc", b"abc".to_vec()),
        ("zeros16", vec![0u8; 16]),
        ("a300", vec![b'a'; 300]),
        ("mixed", zsupport::mixed_kib()),
        ("large", zsupport::large_input(70_000)),
    ];
    let mut checked = 0usize;
    for level in [-1i8, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
        for (framing, config, window_bits) in framings(level) {
            for (name, data) in &inputs {
                let image = Deflate::compress_with(config, data)
                    .unwrap_or_else(|e| panic!("{framing}/{name}/level {level}: {e:?}"));
                let (body, used) = decompress_all(window_bits, &image, usize::MAX)
                    .unwrap_or_else(|e| panic!("{framing}/{name}/level {level}: {e:?}"));
                assert_eq!(body, *data, "{framing}/{name}/level {level}: one-shot");
                assert_eq!(used, image.len(), "{framing}/{name}/level {level}: consumed");

                let mut deflater = deflate_nif(config, DEFLATE_SCHEDULE);
                let pumped = deflater.call(data, Flush::Finish).expect("the deflate pump");
                assert_eq!(
                    hex(&pumped.out),
                    hex(&image),
                    "{framing}/{name}/level {level}: the deflate pump"
                );

                let run = pump_inflate(window_bits, &image, INFLATE_SCHEDULE);
                assert_eq!(run.out, *data, "{framing}/{name}/level {level}: the inflate pump");
                assert!(run.eos_seen && run.end_ok, "{framing}/{name}/level {level}: end");
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 11 * 3 * inputs.len());
    println!("{checked} round trips through the one-shot entries and both pumps");
}
