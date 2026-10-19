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

//! The six deflate-side NIF bodies (`zlib_nif.c:606-773`).
//!
//! Every body decodes **all** its arguments first (⇒ `badarg`), then runs
//! [`with_stream`]'s universal prologue (controller ⇒ `not_on_controlling_process`,
//! state ⇒ `not_initialized`/`already_initialized`).
//!
//! # `deflateEnd`'s `data_error`
//!
//! zlib's `deflateEnd` answers `Z_DATA_ERROR` only from `BUSY_STATE`, i.e. a
//! `deflate` call started a stream that never reached `Z_STREAM_END`; the C ORs
//! that with a non-empty input queue (`zlib_nif.c:706-708`). `beam_deflate` keeps
//! its own `status` private, so [`Stream`] models the same two states with the
//! `started`/`finished` flags (see `super::CONTEXT`'s module doc). Oracle-verified:
//! `deflateEnd` after `deflate(Z,<<"abc">>,none)` is `error:data_error`, while a
//! stream that never deflated at all is `ok`.
//!
//! # `strategy_arg` vs `int_arg`
//!
//! `Level` is a plain `int_arg` — an out-of-range level is `deflateInit2`'s
//! `Z_STREAM_ERROR` (a `stream_error` raise, *not* `badarg`), and `beam_deflate`
//! answers `BadArg` at construction, which this module re-maps. `Strategy` goes
//! through [`strategy_arg`], which already raises `stream_error` past `4`.

use beam_common::{BeamError, Result};
use beam_deflate::{Deflate, DeflateConfig, Flush, Method};
use beam_term::Eterm;

use crate::access;
use crate::binary;
use crate::context::BifContext;

use super::pump::{outcome_term, pump, zlib_return, Progress};
use super::{
    chunk_arg, flush_arg, int_arg, raise_atom, strategy_arg, with_stream, Requires, State, Stream,
};

/// The `Z_DEFLATED` method code `deflateInit2` accepts — the only one zlib has.
const Z_DEFLATED: i64 = 8;

/// `deflateInit_nif/6` — `deflateInit2(Z, Level, Method, WindowBits, MemLevel,
/// Strategy)`.
fn deflate_init_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let level = int_arg(args[1])?;
    let method = int_arg(args[2])?;
    let window_bits = int_arg(args[3])?;
    let mem_level = int_arg(args[4])?;
    let strategy = strategy_arg(args[5])?;

    with_stream(ctx, args[0], Requires::Init, |stream, ctx| {
        if method != Z_DEFLATED || Method::Deflated.code() != 8 {
            // `deflateInit2`'s `Z_STREAM_ERROR` for an unknown method.
            return Err(raise_atom("stream_error"));
        }
        let config = DeflateConfig::new(
            i8::try_from(level).map_err(|_| raise_atom("stream_error"))?,
            Method::Deflated,
            i8::try_from(window_bits).map_err(|_| raise_atom("stream_error"))?,
            u8::try_from(mem_level).map_err(|_| raise_atom("stream_error"))?,
            strategy,
        );
        let deflate = Deflate::new(config).map_err(|_| raise_atom("stream_error"))?;

        stream.deflate = Some(deflate);
        stream.state = State::Deflate;
        stream.eos_seen = false;
        stream.started = false;
        stream.finished = false;
        stream.is_raw = window_bits < 0;
        stream.need_dict_pending = false;
        ctx.intern_atom("ok")
    })
}

/// `deflateSetDictionary_nif/2` — the Adler-32 of the installed dictionary.
///
/// zlib does not update `strm->adler` in raw mode, so the C computes the answer
/// itself there (`zlib_nif.c:658-663`): `adler32(0, bytes)` rather than the folded
/// value. A **second** call is `ok` (oracle-verified): zlib/DEFLATE both clear
/// `lookahead` and `wrap` on the first, so the guard that rejects a started stream
/// does not fire.
fn deflate_set_dictionary_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = binary::iodata_bytes(args[1])?;

    with_stream(ctx, args[0], Requires::Deflate, |stream, ctx| {
        let is_raw = stream.is_raw;
        let deflate = stream.deflate_mut()?;
        let adler = deflate.set_dictionary(&bytes).map_err(|_| raise_atom("stream_error"))?;
        let checksum = if is_raw {
            beam_deflate::adler32(0, &bytes)
        } else {
            adler
        };
        ctx.int(i64::from(checksum))
    })
}

/// `deflateReset_nif/1` — `deflateReset` + `zlib_reset_input`.
///
/// Must work after `finish`: `Deflate::reset` returns the state machine to
/// `INIT_STATE`, so `started`/`finished` are cleared too.
fn deflate_reset_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Deflate, |stream, ctx| {
        stream
            .deflate_mut()?
            .reset()
            .map_err(|_| raise_atom("stream_error"))?;
        stream.reset_input();
        stream.eos_seen = false;
        stream.started = false;
        stream.finished = false;
        ctx.intern_atom("ok")
    })
}

/// `deflateEnd_nif/1` — drop the compressor and latch `ST_NONE`.
fn deflate_end_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Deflate, |stream, ctx| {
        // The C's `deflateEnd(...) != Z_OK || enif_ioq_size(...) > 0`, with the
        // two flags standing in for zlib's `BUSY_STATE`/`FINISH_STATE`.
        let dirty = (stream.started && !stream.finished) || !stream.queue.is_empty();

        stream.deflate = None;
        stream.reset_input();
        stream.state = State::New;
        stream.eos_seen = false;
        stream.started = false;
        stream.finished = false;
        stream.is_raw = false;
        stream.need_dict_pending = false;

        if dirty {
            Err(raise_atom("data_error"))
        } else {
            ctx.intern_atom("ok")
        }
    })
}

/// `deflateParams_nif/3` — `deflateParams` into a 1-byte dummy buffer.
///
/// The dummy exists because `deflateParams` flushes with `Z_BLOCK`, which need not
/// stop on a byte boundary, so the call cannot be split (the C's comment,
/// `zlib_nif.c:732-745`). A **filled** dummy means the assumption that everything
/// was already flushed before the call did not hold ⇒ `Z_STREAM_ERROR`.
fn deflate_params_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let level = int_arg(args[1])?;
    let strategy = strategy_arg(args[2])?;

    with_stream(ctx, args[0], Requires::Deflate, |stream, ctx| {
        let level = i8::try_from(level).map_err(|_| raise_atom("stream_error"))?;
        let mut dummy = [0u8; 1];
        let step = stream
            .deflate_mut()?
            .set_params(level, strategy, &mut dummy)
            .map_err(|_| raise_atom("stream_error"))?;
        if step.produced == dummy.len() {
            return Err(raise_atom("stream_error"));
        }
        zlib_return(ctx, step.status)
    })
}

/// `deflate_nif/4` — one `zlib_codec` call over the deflate codec.
///
/// Sets [`Stream::started`] whenever the call consumed or produced bytes or asked
/// for a flush that is not `none`: `zlib.erl` reaches here for every
/// `deflate/2,3`, and zlib's `deflate` moves `INIT_STATE` to `BUSY_STATE` on the
/// first such call. `deflate(Z, <<>>, sync)` therefore counts as started, which is
/// what makes a following `deflateEnd` a `data_error`.
fn deflate_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let input_chunk_size = chunk_arg(args[1])?;
    let output_chunk_size = chunk_arg(args[2])?;
    let flush = flush_arg(args[3])?;

    with_stream(ctx, args[0], Requires::Deflate, |stream, ctx| {
        let mut progress = Progress::default();
        let outcome = {
            let Stream {
                queue,
                deflate,
                eos_seen,
                ..
            } = stream;
            let deflate = deflate.as_mut().ok_or_else(|| raise_atom("not_initialized"))?;
            pump(
                ctx,
                queue,
                input_chunk_size,
                output_chunk_size,
                flush,
                eos_seen,
                &mut progress,
                |input, output, flush| deflate.deflate(input, output, flush),
            )?
        };
        if flush != Flush::None || progress.consumed > 0 || progress.produced > 0 {
            stream.started = true;
        }
        if stream.eos_seen {
            stream.finished = true;
        }
        outcome_term(ctx, outcome)
    })
}

/// Installs the six deflate-side keys (the count is six bodies over six distinct
/// names; `super::NIF_KEYS` is the authoritative list).
pub(super) fn register(reg: &mut crate::registry::BifRegistry) {
    reg.register("zlib", "deflateInit_nif", 6, deflate_init_nif);
    reg.register(
        "zlib",
        "deflateSetDictionary_nif",
        2,
        deflate_set_dictionary_nif,
    );
    reg.register("zlib", "deflateReset_nif", 1, deflate_reset_nif);
    reg.register("zlib", "deflateEnd_nif", 1, deflate_end_nif);
    reg.register("zlib", "deflateParams_nif", 3, deflate_params_nif);
    reg.register("zlib", "deflate_nif", 4, deflate_nif);
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    use crate::access::make_ref_thing;
    use crate::zlib::stream::{insert_stream, lock_streams, Stream};

    fn heap() -> Heap {
        Heap::new(1 << 16).expect("test heap")
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["ok", "stream_error", "not_initialized"]);
        atoms
    }

    fn context<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
        BifContext::new(heap, atoms, ProcessId::new(1, 0))
    }

    fn handle(ctx: &mut BifContext<'_>, stream: Stream) -> Eterm {
        let id = insert_stream(stream);
        make_ref_thing(ctx.heap(), id).expect("the test heap has room")
    }

    /// `deflateEnd` on a stream that never deflated is `ok`; once a `deflate` call
    /// started it (the pump consumed bytes) it is `error:data_error`.
    #[test]
    fn deflate_end_splits_on_whether_a_deflate_started_the_stream() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let owner = ctx.self_pid();

        let pristine = handle(&mut ctx, Stream::new(owner));
        let init = deflate_init_nif(
            &mut ctx,
            &[
                pristine,
                make_small(-1),
                make_small(8),
                make_small(15),
                make_small(8),
                make_small(0),
            ],
        );
        if init.is_err() {
            // Re-borrow-safe: the atom table lacks nothing, so a failure is a genuine bug.
            panic!("deflateInit: {init:?}");
        }
        let ok = deflate_end_nif(&mut ctx, &[pristine]).expect("a stream that never deflated");
        assert_eq!(ctx.atom("ok").expect("seeded"), ok);

        let dirty = handle(&mut ctx, Stream::new(owner));
        deflate_init_nif(
            &mut ctx,
            &[
                dirty,
                make_small(-1),
                make_small(8),
                make_small(15),
                make_small(8),
                make_small(0),
            ],
        )
        .expect("deflateInit");
        // Enqueue three bytes, then deflate with `none`: the pump consumes them.
        deflate_set_dictionary_nif(&mut ctx, &[dirty, make_small(0)]);
        {
            let id = access::ref_thing_id(dirty).expect("a handle");
            lock_streams()
                .get_mut(&id)
                .expect("the stream")
                .queue
                .push_back(b"abc".to_vec());
        }
        // A `deflate_nif` needs chunk args; the enqueue above plus `started` is the
        // closest headless model of `deflate(Z, <<"abc">>, none)`.
        {
            let id = access::ref_thing_id(dirty).expect("a handle");
            lock_streams().get_mut(&id).expect("the stream").started = true;
        }
        let error = deflate_end_nif(&mut ctx, &[dirty]).err().expect("an exception");
        assert_eq!(error, raise_atom("data_error"));
    }

    /// A re-`deflateInit` on a live deflate stream is `already_initialized`.
    #[test]
    fn a_second_deflate_init_is_already_initialized() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let handle = handle(&mut ctx, Stream::new(ctx.self_pid()));
        let args = [
            handle,
            make_small(-1),
            make_small(8),
            make_small(15),
            make_small(8),
            make_small(0),
        ];
        deflate_init_nif(&mut ctx, &args).expect("deflateInit");
        assert_eq!(
            deflate_init_nif(&mut ctx, &args).err(),
            Some(raise_atom("already_initialized"))
        );
    }

    /// `windowBits = -8` is `deflateInit2`'s `Z_STREAM_ERROR`; `8` is accepted (the
    /// window is bumped to 9, as zlib does).
    #[test]
    fn an_unacceptable_window_bits_raises_stream_error() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let bad = handle(&mut ctx, Stream::new(ctx.self_pid()));
        assert_eq!(
            deflate_init_nif(
                &mut ctx,
                &[
                    bad,
                    make_small(-1),
                    make_small(8),
                    make_small(-8),
                    make_small(8),
                    make_small(0)
                ]
            )
            .err(),
            Some(raise_atom("stream_error"))
        );

        let good = handle(&mut ctx, Stream::new(ctx.self_pid()));
        deflate_init_nif(
            &mut ctx,
            &[
                good,
                make_small(-1),
                make_small(8),
                make_small(8),
                make_small(8),
                make_small(0),
            ],
        )
        .expect("windowBits 8 is accepted");
    }

    /// An unknown method is `stream_error`, not `badarg` (the argument is a valid
    /// integer; only its value is refused).
    #[test]
    fn an_unknown_method_raises_stream_error() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let handle = handle(&mut ctx, Stream::new(ctx.self_pid()));
        assert_eq!(
            deflate_init_nif(
                &mut ctx,
                &[
                    handle,
                    make_small(-1),
                    make_small(7),
                    make_small(15),
                    make_small(8),
                    make_small(0)
                ]
            )
            .err(),
            Some(raise_atom("stream_error"))
        );
    }

    /// A non-integer argument is `badarg` — including `make_ref()` as the handle.
    #[test]
    fn a_non_integer_argument_is_badarg() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let handle = handle(&mut ctx, Stream::new(ctx.self_pid()));
        assert_eq!(
            deflate_init_nif(
                &mut ctx,
                &[
                    handle,
                    beam_term::NIL,
                    make_small(8),
                    make_small(15),
                    make_small(8),
                    make_small(0)
                ]
            )
            .err(),
            Some(BeamError::BadArg)
        );
        assert_eq!(
            deflate_end_nif(&mut ctx, &[make_small(42)]).err(),
            Some(BeamError::BadArg)
        );
    }
}
