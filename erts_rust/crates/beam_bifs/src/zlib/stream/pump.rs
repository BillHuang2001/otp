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

//! `zlib_return`, the shared pump and its classification order.
//!
//! The ground truth is `../../../../../erts/emulator/nifs/common/zlib_nif.c`:
//! `zlib_return` (`:246`), `zlib_flush_queue` (`:339-406`) and `zlib_codec`
//! (`:408-469`). The C takes a `int (*codec)(z_stream*, int)` function pointer;
//! this port takes a closure of the same shape
//! (`FnMut(&[u8], &mut [u8], Flush) -> Result<beam_deflate::Pump>`), so
//! `deflate_nif` and `inflate_nif` run one body rather than two near-copies.
//!
//! # The classification order (`zlib_codec`)
//!
//! 1. a **hard** status (`Z_DATA_ERROR`/`Z_STREAM_ERROR`/`Z_MEM_ERROR` — the C's
//!    `res < 0 && res != Z_BUF_ERROR`) is answered by [`zlib_return`] and nothing
//!    else runs: the two stream faults raise, `Z_MEM_ERROR` returns the atom;
//! 2. `Z_STREAM_END` latches `eos_seen`;
//! 3. **zero** progress on a non-empty queue is the C's `Z_DATA_ERROR` (a raise);
//! 4. `[Binary]` when output was produced, `[]` when not;
//! 5. `finished` when the queue drained **and** the output buffer was not filled;
//! 6. otherwise `{need_dictionary, Adler, Out}` for `Z_NEED_DICT`, `{continue, Out}`
//!    for everything else.
//!
//! `Z_BUF_ERROR` is zlib's **non-fatal** "no progress possible" code and flows
//! through the classification like `Z_OK` — which is why the hard test is spelled
//! out as a three-variant match rather than [`beam_deflate::Status::is_error`]
//! (that predicate counts `BufError` as an error, as zlib's sign convention says,
//! but `zlib_codec` deliberately exempts it).
//!
//! # Codes with no arm here
//!
//! `Z_ERRNO` and `Z_VERSION_ERROR` have no [`beam_deflate::Status`] variant:
//! `beam_deflate` has no syscall layer, and a header-version mismatch is a
//! compile-time concern of the C library (an unacceptable configuration surfaces
//! as a construction-time [`beam_common::BeamError::BadArg`] instead). Neither is
//! reachable, so neither is modelled. `NeedDict` is never a [`zlib_return`] input —
//! the pump classifies it — so it lands on the C's `default:` arm
//! (`unknown_error`).

use std::collections::VecDeque;

use beam_common::Result;
use beam_deflate::{Flush, Pump, Status};
use beam_term::Eterm;

use crate::bits;
use crate::context::BifContext;

use super::raise_atom;

/// `zlib_return` (`zlib_nif.c:246`) — zlib's status code as the term `zlib.erl`
/// expects to receive.
///
/// Only `stream_error` and `data_error` are **exceptions** (the C's
/// `enif_raise_exception`), so those two are `Err`; every other code is a plain
/// atom (`ok`, `stream_end`, `mem_error`, `buf_error`) and is `Ok(atom)`.
///
/// The atom is interned through the context (an existing name needs no hook, a new
/// one needs the [`crate::AtomInterner`] seam), never fabricated.
///
/// # Errors
///
/// The `stream_error`/`data_error` raise, or the context's own atom failure.
pub(super) fn zlib_return(ctx: &mut BifContext<'_>, status: Status) -> Result<Eterm> {
    match status {
        Status::Ok => ctx.intern_atom("ok"),
        Status::StreamEnd => ctx.intern_atom("stream_end"),
        Status::StreamError => Err(raise_atom("stream_error")),
        Status::DataError => Err(raise_atom("data_error")),
        Status::MemError => ctx.intern_atom("mem_error"),
        Status::BufError => ctx.intern_atom("buf_error"),
        // The C's `default:` arm. `NeedDict` is classified by the pump, never
        // translated, so this is unreachable — see the module doc.
        Status::NeedDict(_) => ctx.intern_atom("unknown_error"),
    }
}

/// What one `zlib_codec` call answers, as a term to hand back (or a raised
/// exception).
///
/// [`Outcome::Value`] is the C's shape for a status `zlib_codec` answers with a
/// bare term rather than a `{Tag, Out}` tuple — `Z_MEM_ERROR`'s `mem_error`
/// (`zlib_nif.c:418-420`). It is unreachable in practice (`beam_deflate` allocates
/// through the Rust allocator), kept so no status is silently dropped.
pub(super) enum Outcome {
    /// `{finished, Out}` — the queue drained and the output buffer was not filled.
    Finished(Eterm),
    /// `{continue, Out}` — more work is possible; `zlib.erl` drains again.
    Continue(Eterm),
    /// `{need_dictionary, Adler, Out}` — `Z_NEED_DICT` (`zlib_nif.c:466-467`).
    NeedDictionary { adler: u32, out: Eterm },
    /// A bare term (`mem_error`).
    Value(Eterm),
}

/// The pump's byte counters — what `deflate_nif` needs to decide whether a
/// `deflate` call started the stream (zlib's `BUSY_STATE`; see
/// [`super::Stream::started`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Progress {
    /// Input bytes the codec consumed across the whole call.
    pub(super) consumed: usize,
    /// Output bytes the codec wrote across the whole call.
    pub(super) produced: usize,
}

/// `zlib_flush_queue` + `zlib_codec`: pump the queued input through `codec`, then
/// flush.
///
/// `codec` is one zlib step — `Deflate::deflate` or `Inflate::inflate`. It is
/// `FnMut` rather than `Fn` because the C's function pointer is called several
/// times per `zlib_codec` call and each call advances the same `z_stream`.
///
/// `eos_seen` is written through (the C's `d->eos_seen = 1`) and `progress`
/// accumulates both byte counts.
///
/// The input pieces come from `queue` (one `Vec<u8>` per enqueued iovec segment);
/// the consumed prefix of each is drained, and a fully consumed piece is removed
/// (`enif_ioq_deq`).
///
/// # Errors
///
/// The two stream faults via [`zlib_return`] (`stream_error`/`data_error`), the
/// C's zero-progress `Z_DATA_ERROR`, and the codec's own `Err` (a configuration
/// `beam_deflate` refuses, which cannot happen after a successful init).
pub(super) fn pump<F>(
    ctx: &mut BifContext<'_>,
    queue: &mut VecDeque<Vec<u8>>,
    input_chunk_size: usize,
    output_chunk_size: usize,
    flush: Flush,
    eos_seen: &mut bool,
    progress: &mut Progress,
    mut codec: F,
) -> Result<Outcome>
where
    F: FnMut(&[u8], &mut [u8], Flush) -> Result<Pump>,
{
    let mut out = vec![0u8; output_chunk_size];
    let mut out_pos = 0usize;
    let mut consumed = 0usize;
    let mut status = Status::Ok;

    // `zlib_flush_queue`'s loop. The C's `enif_consume_timeslice` breaks are
    // deliberately not reproduced (they are a scheduler courtesy, not semantics);
    // `out_pos < output_chunk_size` stands in for the C's `avail_out > 0`
    // `ASSERT` at `zlib_nif.c:368`, which is what makes the loop terminate.
    while status == Status::Ok
        && !queue.is_empty()
        && consumed < input_chunk_size
        && out_pos < output_chunk_size
    {
        let Some(front) = queue.front() else { break };
        let block = front.len().min(input_chunk_size - consumed);
        let step = codec(&front[..block], &mut out[out_pos..], Flush::None)?;
        consumed += step.consumed;
        out_pos += step.produced;
        status = step.status;
        // Advance the front piece by what the codec consumed (`enif_ioq_deq`).
        match queue.front_mut() {
            Some(piece) if step.consumed >= piece.len() => {
                queue.pop_front();
            }
            Some(piece) => piece.drain(..step.consumed),
            None => break,
        }
        if step.consumed == 0 && step.produced == 0 {
            // The C's `ASSERT` says a `Z_OK` step either drains its block or fills
            // the output buffer. A codec that does neither would spin here forever,
            // so stop and let the zero-progress classification below answer it.
            break;
        }
    }

    let remaining = queue.len();
    if status == Status::Ok && remaining == 0 && out_pos < output_chunk_size {
        // The C's end-of-input flush (`zlib_nif.c:391-401`): done even for
        // `Z_NO_FLUSH`, since the previous call may have filled the buffer.
        let step = codec(&[], &mut out[out_pos..], flush)?;
        out_pos += step.produced;
        status = step.status;
    }

    progress.consumed = consumed;
    progress.produced = out_pos;

    if matches!(
        status,
        Status::DataError | Status::StreamError | Status::MemError
    ) {
        return zlib_return(ctx, status).map(Outcome::Value);
    }

    if status == Status::StreamEnd {
        *eos_seen = true;
    }

    if consumed == 0 && out_pos == 0 && remaining != 0 {
        // The C's "die if we've made zero progress" (`zlib_nif.c:439-444`).
        return Err(raise_atom("data_error"));
    }

    let out_term = if out_pos > 0 {
        let binary = bits::make_binary(ctx.heap(), &out[..out_pos])?;
        ctx.list(&[binary])?
    } else {
        ctx.list(&[])?
    };

    if remaining == 0 && out_pos < output_chunk_size {
        return Ok(Outcome::Finished(out_term));
    }
    match status {
        Status::NeedDict(adler) => Ok(Outcome::NeedDictionary {
            adler,
            out: out_term,
        }),
        _ => Ok(Outcome::Continue(out_term)),
    }
}

/// The term a [`Outcome`] answers with: the `{need_dictionary, Adler, Out}` tuple
/// for the dictionary request, the plain `Out` list otherwise.
///
/// # Errors
///
/// The context's atom/allocation failures.
pub(super) fn outcome_term(ctx: &mut BifContext<'_>, outcome: Outcome) -> Result<Eterm> {
    match outcome {
        Outcome::Finished(out) | Outcome::Continue(out) | Outcome::Value(out) => Ok(out),
        Outcome::NeedDictionary { adler, out } => {
            let tag = ctx.intern_atom("need_dictionary")?;
            let count = ctx.int(i64::from(adler))?;
            ctx.tuple(&[tag, count, out])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_deflate::Pump;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    fn heap() -> Heap {
        Heap::new(1 << 14).expect("test heap")
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "ok",
            "stream_end",
            "mem_error",
            "buf_error",
            "unknown_error",
            "need_dictionary",
        ]);
        atoms
    }

    fn queue_of(pieces: &[&[u8]]) -> VecDeque<Vec<u8>> {
        pieces.iter().map(|piece| piece.to_vec()).collect()
    }

    fn atom_name(ctx: &mut BifContext<'_>, term: Eterm) -> &'static str {
        // The predicted atoms are all short latin-1 names; read the one that matches.
        for name in [
            "ok",
            "stream_end",
            "mem_error",
            "buf_error",
            "unknown_error",
            "need_dictionary",
        ] {
            if ctx.atom(name).expect("seeded") == term {
                return name;
            }
        }
        panic!("not one of the predicted atoms");
    }

    /// `zlib_return`'s whole table: two raises, five plain atoms.
    #[test]
    fn zlib_return_raise_only_for_the_two_stream_faults() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        for (status, name) in [
            (Status::Ok, "ok"),
            (Status::StreamEnd, "stream_end"),
            (Status::MemError, "mem_error"),
            (Status::BufError, "buf_error"),
            (Status::NeedDict(7), "unknown_error"),
        ] {
            let term = zlib_return(&mut ctx, status).expect("a plain atom");
            assert_eq!(atom_name(&mut ctx, term), name);
        }
        assert_eq!(
            zlib_return(&mut ctx, Status::StreamError).err(),
            Some(raise_atom("stream_error"))
        );
        assert_eq!(
            zlib_return(&mut ctx, Status::DataError).err(),
            Some(raise_atom("data_error"))
        );
    }

    /// The `finished` arm: the queue drained and the output buffer was not filled.
    #[test]
    fn a_drained_queue_with_room_left_is_finished() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut queue = queue_of(&[b"abc"]);
        let mut eos = false;
        let mut progress = Progress::default();

        let outcome = pump(
            &mut ctx,
            &mut queue,
            8,
            8,
            Flush::None,
            &mut eos,
            &mut progress,
            |input, output, _flush| {
                if input.is_empty() {
                    return Ok(Pump {
                        status: Status::Ok,
                        consumed: 0,
                        produced: 0,
                    });
                }
                output[0] = 0xAA;
                Ok(Pump {
                    status: Status::Ok,
                    consumed: input.len(),
                    produced: 1,
                })
            },
        )
        .expect("the pump runs");

        assert!(matches!(outcome, Outcome::Finished(_)));
        assert_eq!(progress.consumed, 3);
        assert_eq!(progress.produced, 1);
        assert!(!eos);
        assert!(queue.is_empty());
    }

    /// `Z_STREAM_END` latches `eos_seen` and still classifies as finished.
    #[test]
    fn stream_end_latches_eos_seen() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut queue = queue_of(&[b"ab"]);
        let mut eos = false;
        let mut progress = Progress::default();

        pump(
            &mut ctx,
            &mut queue,
            8,
            8,
            Flush::None,
            &mut eos,
            &mut progress,
            |input, _output, _flush| {
                Ok(Pump {
                    status: if input.is_empty() {
                        Status::Ok
                    } else {
                        Status::StreamEnd
                    },
                    consumed: input.len(),
                    produced: 0,
                })
            },
        )
        .expect("the pump runs");

        assert!(eos, "a StreamEnd step latches the flag");
    }

    /// A filled output buffer with input left over is `continue`, not `finished`.
    #[test]
    fn a_full_output_buffer_with_input_left_is_continue() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut queue = queue_of(&[b"abcd"]);
        let mut eos = false;
        let mut progress = Progress::default();

        let outcome = pump(
            &mut ctx,
            &mut queue,
            8,
            2,
            Flush::None,
            &mut eos,
            &mut progress,
            |_input, output, _flush| {
                for byte in output.iter_mut() {
                    *byte = 1;
                }
                Ok(Pump {
                    status: Status::Ok,
                    consumed: 0,
                    produced: output.len(),
                })
            },
        )
        .expect("the pump runs");

        assert!(matches!(outcome, Outcome::Continue(_)));
        assert!(!queue.is_empty(), "nothing was consumed");
    }

    /// `Z_NEED_DICT` with input left over is the three-tuple.
    #[test]
    fn need_dict_with_input_left_is_the_three_tuple() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut queue = queue_of(&[b"ab", b"cd"]);
        let mut eos = false;
        let mut progress = Progress::default();

        let outcome = pump(
            &mut ctx,
            &mut queue,
            8,
            8,
            Flush::None,
            &mut eos,
            &mut progress,
            |input, _output, _flush| {
                if input.is_empty() {
                    return Ok(Pump {
                        status: Status::Ok,
                        consumed: 0,
                        produced: 0,
                    });
                }
                Ok(Pump {
                    status: Status::NeedDict(42),
                    consumed: input.len(),
                    produced: 0,
                })
            },
        )
        .expect("the pump runs");

        match outcome_term(&mut ctx, outcome).expect("a term") {
            term => {
                assert_eq!(beam_codec::term::tuple_arity(term).expect("a tuple"), 3);
                let tag = beam_codec::term::tuple_element(term, 0).expect("tag");
                assert_eq!(atom_name(&mut ctx, tag), "need_dictionary");
                let adler = beam_codec::term::tuple_element(term, 1).expect("adler");
                assert_eq!(beam_term::signed_value(adler), 42);
            }
        }
    }

    /// `Z_MEM_ERROR` is a hard status that answers a plain atom, not a tuple.
    #[test]
    fn mem_error_answers_a_bare_atom() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut queue = queue_of(&[b"ab"]);
        let mut eos = false;
        let mut progress = Progress::default();

        let outcome = pump(
            &mut ctx,
            &mut queue,
            8,
            8,
            Flush::None,
            &mut eos,
            &mut progress,
            |input, _output, _flush| {
                Ok(Pump {
                    status: Status::MemError,
                    consumed: input.len(),
                    produced: 0,
                })
            },
        )
        .expect("a plain return value, not an exception");

        let term = outcome_term(&mut ctx, outcome).expect("a term");
        assert_eq!(atom_name(&mut ctx, term), "mem_error");
    }

    /// The two hard faults raise before anything else is classified.
    #[test]
    fn a_hard_status_raises_before_classification() {
        for (status, reason) in [
            (Status::DataError, "data_error"),
            (Status::StreamError, "stream_error"),
        ] {
            let mut heap = heap();
            let atoms = atoms();
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
            let mut queue = queue_of(&[b"ab"]);
            let mut eos = false;
            let mut progress = Progress::default();

            let error = pump(
                &mut ctx,
                &mut queue,
                8,
                8,
                Flush::None,
                &mut eos,
                &mut progress,
                |input, _output, _flush| {
                    Ok(Pump {
                        status,
                        consumed: input.len(),
                        produced: 0,
                    })
                },
            )
            .err()
            .expect("an exception");
            assert_eq!(error, raise_atom(reason));
        }
    }

    /// Zero progress on a non-empty queue is the C's `Z_DATA_ERROR`.
    #[test]
    fn zero_progress_on_a_non_empty_queue_is_data_error() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut queue = queue_of(&[b"abcd"]);
        let mut eos = false;
        let mut progress = Progress::default();

        let error = pump(
            &mut ctx,
            &mut queue,
            8,
            8,
            Flush::None,
            &mut eos,
            &mut progress,
            |_input, _output, _flush| {
                Ok(Pump {
                    status: Status::Ok,
                    consumed: 0,
                    produced: 0,
                })
            },
        )
        .err()
        .expect("an exception");

        assert_eq!(error, raise_atom("data_error"));
        assert!(!queue.is_empty(), "a no-progress step consumes nothing");
    }
}
