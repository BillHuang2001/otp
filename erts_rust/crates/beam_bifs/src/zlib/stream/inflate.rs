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

//! The six inflate-side NIF bodies (`zlib_nif.c:777-969`).
//!
//! # `inflateSetDictionary`'s discriminator
//!
//! zlib answers `Z_STREAM_ERROR` when a framed stream's `mode != DICT`, and
//! `Z_DATA_ERROR` when the dictionary's Adler-32 does not match the header's.
//! `beam_deflate::Inflate::set_dictionary` collapses both into `BadArg`, so
//! [`Stream::need_dict_pending`] supplies the discriminator: without it (and
//! outside raw mode) the call is the `stream_error` raise — oracle-verified on a
//! fresh framed stream — and with it a rejected dictionary is `data_error`.
//!
//! # `inflateEnd` is where a truncated stream is detected
//!
//! The C raises `Z_DATA_ERROR` when `!eos_seen || ioq_size > 0`
//! (`zlib_nif.c:921-923`); no other body looks at the flag, so `inflateEnd` is the
//! only place a stream that never reached `Z_STREAM_END` is reported. Oracle-verified:
//! a truncated stream followed by `inflateEnd` is `error:data_error`.

use beam_common::{BeamError, Result};
use beam_deflate::Inflate;
use beam_term::Eterm;

use crate::binary;
use crate::bits;
use crate::context::BifContext;

use super::pump::{outcome_term, pump, Outcome, Progress};
use super::{
    chunk_arg, flush_arg, int_arg, raise_atom, with_stream, Requires, State, Stream,
    EOS_BEHAVIOR_ERROR, EOS_BEHAVIOR_RESET,
};

/// `inflateInit_nif/3` — `inflateInit2(Z, WindowBits)` plus the eos behaviour.
///
/// `inflateInit(Z, -8)` (raw) and `inflateInit(Z, 47)` (gzip/zlib auto-detect) are
/// both `ok` on the oracle; `beam_deflate::Inflate::new` accepts the same set, so
/// anything it refuses is the C's `Z_STREAM_ERROR`.
fn inflate_init_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let window_bits = int_arg(args[1])?;
    // `arg_eos_behavior/1` only ever produces 0/1/2; anything else is not a value
    // the Erlang layer can send, so it is an argument fault (`badarg`), decided
    // before the controller check like every other argument decode.
    let eos_behavior = u8::try_from(int_arg(args[2])?)
        .ok()
        .filter(|value| *value <= 2)
        .ok_or(BeamError::BadArg)?;

    with_stream(ctx, args[0], Requires::Init, |stream, ctx| {
        let inflate = Inflate::new(i8::try_from(window_bits).map_err(|_| raise_atom("stream_error"))?)
            .map_err(|_| raise_atom("stream_error"))?;

        stream.inflate = Some(inflate);
        stream.state = State::Inflate;
        stream.eos_behavior = eos_behavior;
        stream.eos_seen = false;
        stream.is_raw = window_bits < 0;
        stream.need_dict_pending = false;
        ctx.intern_atom("ok")
    })
}

/// `inflateSetDictionary_nif/2` — install the preset dictionary `NeedDict` asked
/// for.
fn inflate_set_dictionary_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = binary::iodata_bytes(args[1])?;

    with_stream(ctx, args[0], Requires::Inflate, |stream, ctx| {
        if !stream.need_dict_pending && !stream.is_raw {
            // zlib's `state->wrap != 0 && state->mode != DICT`.
            return Err(raise_atom("stream_error"));
        }
        match stream.inflate_mut()?.set_dictionary(&bytes) {
            Ok(()) => {
                stream.need_dict_pending = false;
                ctx.intern_atom("ok")
            }
            // A mismatched dictionary id is `Z_DATA_ERROR`.
            Err(_) => Err(raise_atom("data_error")),
        }
    })
}

/// `inflateGetDictionary_nif/1` — up to 32768 bytes, `<<>>` before any inflate.
fn inflate_get_dictionary_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Inflate, |stream, ctx| {
        let dictionary = stream.inflate_mut()?.get_dictionary().to_vec();
        bits::make_binary(ctx.heap(), &dictionary)
    })
}

/// `inflateReset_nif/1` — `inflateReset` + `zlib_reset_input`.
fn inflate_reset_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Inflate, |stream, ctx| {
        stream
            .inflate_mut()?
            .reset()
            .map_err(|_| raise_atom("stream_error"))?;
        stream.reset_input();
        stream.eos_seen = false;
        stream.need_dict_pending = false;
        ctx.intern_atom("ok")
    })
}

/// `inflateEnd_nif/1` — drop the inflater and latch `ST_NONE`.
fn inflate_end_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Inflate, |stream, ctx| {
        let dirty = !stream.eos_seen || !stream.queue.is_empty();

        stream.inflate = None;
        stream.reset_input();
        stream.state = State::New;
        stream.eos_seen = false;
        stream.is_raw = false;
        stream.need_dict_pending = false;

        if dirty {
            Err(raise_atom("data_error"))
        } else {
            ctx.intern_atom("ok")
        }
    })
}

/// `inflate_nif/4` — the EOS entry step, then one `zlib_codec` call.
///
/// The entry step (`zlib_nif.c:947-966`) is what makes the three
/// `?EOS_BEHAVIOR_*` modes observable on **concatenated** streams: `error` raises
/// `data_error`, `reset` restarts the inflater and keeps the queued input, `cut`
/// drops the queued input (the `open_nif` default).
///
/// A `Z_NEED_DICT` answer latches [`Stream::need_dict_pending`] so a following
/// `inflateSetDictionary` can tell the two zlib faults apart, and is answered as
/// `{need_dictionary, Adler, Out}`.
fn inflate_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let input_chunk_size = chunk_arg(args[1])?;
    let output_chunk_size = chunk_arg(args[2])?;
    let flush = flush_arg(args[3])?;

    with_stream(ctx, args[0], Requires::Inflate, |stream, ctx| {
        if stream.eos_seen && !stream.queue.is_empty() {
            if stream.eos_behavior == EOS_BEHAVIOR_ERROR {
                return Err(raise_atom("data_error"));
            } else if stream.eos_behavior == EOS_BEHAVIOR_RESET {
                stream
                    .inflate_mut()?
                    .reset()
                    .map_err(|_| raise_atom("stream_error"))?;
                stream.eos_seen = false;
            } else {
                // `EOS_BEHAVIOR_CUT` — the only remaining value (`open_nif`'s
                // default, and `inflateInit_nif` refuses anything outside 0..=2).
                stream.reset_input();
            }
        }

        let mut progress = Progress::default();
        let outcome = {
            let Stream {
                queue,
                inflate,
                eos_seen,
                ..
            } = stream;
            let inflate = inflate.as_mut().ok_or_else(|| raise_atom("not_initialized"))?;
            pump(
                ctx,
                queue,
                input_chunk_size,
                output_chunk_size,
                flush,
                eos_seen,
                &mut progress,
                |input, output, flush| inflate.inflate(input, output, flush),
            )?
        };
        if matches!(outcome, Outcome::NeedDictionary { .. }) {
            stream.need_dict_pending = true;
        }
        outcome_term(ctx, outcome)
    })
}

/// Installs the six inflate-side keys (`super::NIF_KEYS` is the authoritative list).
pub(super) fn register(reg: &mut crate::registry::BifRegistry) {
    reg.register("zlib", "inflateInit_nif", 3, inflate_init_nif);
    reg.register(
        "zlib",
        "inflateSetDictionary_nif",
        2,
        inflate_set_dictionary_nif,
    );
    reg.register(
        "zlib",
        "inflateGetDictionary_nif",
        1,
        inflate_get_dictionary_nif,
    );
    reg.register("zlib", "inflateReset_nif", 1, inflate_reset_nif);
    reg.register("zlib", "inflateEnd_nif", 1, inflate_end_nif);
    reg.register("zlib", "inflate_nif", 4, inflate_nif);
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    use crate::access::{make_ref_thing, ref_thing_id};
    use crate::zlib::stream::{
        assert_badarg, assert_raise_atom, insert_stream, lock_streams, Stream,
    };

    fn heap() -> Heap {
        Heap::new(1 << 16).expect("test heap")
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["ok", "stream_error", "data_error", "not_initialized"]);
        atoms
    }

    fn context<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
        BifContext::new(heap, atoms, ProcessId::new(1, 0))
    }

    /// An initialised inflate stream, owned by the context's process.
    fn initialised<'a>(ctx: &mut BifContext<'a>, window_bits: i64) -> Eterm {
        let owner = ctx.self_pid();
        let id = insert_stream(Stream::new(owner));
        let handle = make_ref_thing(ctx.heap(), id).expect("room");
        inflate_init_nif(ctx, &[handle, make_small(window_bits), make_small(2)])
            .expect("inflateInit");
        handle
    }

    /// `inflateInit` accepts the raw and auto-detect window bits; the eos behaviour
    /// outside 0..=2 is `badarg`.
    #[test]
    fn inflate_init_accepts_raw_and_auto_detect() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        initialised(&mut ctx, -8);
        initialised(&mut ctx, 47);

        let owner = ctx.self_pid();
        let id = insert_stream(Stream::new(owner));
        let handle = make_ref_thing(ctx.heap(), id).expect("room");
        assert_badarg(inflate_init_nif(
            &mut ctx,
            &[handle, make_small(15), make_small(3)],
        ));
    }

    /// `inflateEnd` is `data_error` until `eos_seen`, then `ok`.
    #[test]
    fn inflate_end_splits_on_eos_seen() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let truncated = initialised(&mut ctx, 15);
        assert_raise_atom(inflate_end_nif(&mut ctx, &[truncated]), "data_error");

        let complete = initialised(&mut ctx, 15);
        {
            let id = ref_thing_id(complete).expect("a handle");
            lock_streams().get_mut(&id).expect("the stream").eos_seen = true;
        }
        let ok = inflate_end_nif(&mut ctx, &[complete]).expect("no end of stream owed");
        assert_eq!(ctx.atom("ok").expect("seeded"), ok);
    }

    /// A fresh **framed** stream refuses a dictionary with `stream_error`; a raw one
    /// accepts it (`zlib_nif.c:826` + oracle).
    #[test]
    fn a_framed_stream_refuses_a_dictionary_before_need_dict() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let framed = initialised(&mut ctx, 15);
        let dictionary = bits::make_binary(ctx.heap(), b"abcdef").expect("room");
        assert_raise_atom(
            inflate_set_dictionary_nif(&mut ctx, &[framed, dictionary]),
            "stream_error",
        );

        let raw = initialised(&mut ctx, -8);
        let dictionary = bits::make_binary(ctx.heap(), b"abcdef").expect("room");
        inflate_set_dictionary_nif(&mut ctx, &[raw, dictionary]).expect("raw mode accepts");
    }

    /// `inflateGetDictionary` is `<<>>` before any inflate.
    #[test]
    fn the_dictionary_starts_empty() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let handle = initialised(&mut ctx, 15);
        let term = inflate_get_dictionary_nif(&mut ctx, &[handle]).expect("a binary");
        let (bytes, offset, size_bits) =
            beam_codec::term::bitstring_bytes(term).expect("a readable bitstring");
        assert_eq!(size_bits, 0);
        assert_eq!(offset, 0);
        assert!(bytes.is_empty() || size_bits == 0);
    }

    /// A wrong state is `not_initialized`, and a wrong controller is reported before
    /// it — the prologue's order.
    #[test]
    fn the_prologue_order_is_state_after_controller() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let handle = initialised(&mut ctx, 15);

        // A brand-new (uninitialised) stream.
        let fresh_id = insert_stream(Stream::new(ctx.self_pid()));
        let fresh = make_ref_thing(ctx.heap(), fresh_id).expect("r");
        assert_raise_atom(inflate_end_nif(&mut ctx, &[fresh]), "not_initialized");

        // The same stream, addressed by a process that is not its controller.
        let mut other_heap = Heap::new(1 << 12).expect("test heap");
        let mut other = BifContext::new(&mut other_heap, &atoms, ProcessId::new(2, 0));
        assert_raise_atom(
            inflate_end_nif(&mut other, &[handle]),
            "not_on_controlling_process",
        );
    }

    /// A handle this module never minted is `badarg`.
    #[test]
    fn a_non_handle_is_badarg() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        assert_badarg(inflate_end_nif(&mut ctx, &[make_small(7)]));
    }
}
