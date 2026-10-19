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

//! The `zlib` stream API at the NIF level: the 19 non-exported local functions
//! `../../../../../erts/preloaded/src/zlib.erl:100-106` declares in its
//! `-nifs([...])` list.
//!
//! This node owns the whole surface. This file carries the shared machinery (state,
//! registry, handle minting, the universal prologue, the argument tables, the key
//! table, the local-call predicate and [`register`]); the bodies live in the four
//! submodules — [`pump`] (`zlib_return` plus the shared pump and its classification
//! order), [`lifecycle`] (`open`/`close`/`set_controller`/`enqueue` and the three
//! stash keys), [`deflate`] (six bodies) and [`inflate`] (six bodies). Each of the
//! four is also where the `beam_deflate` mapping of its own keys is documented.
//!
//! # The 19 keys, and how each maps to [`beam_deflate`]
//!
//! | NIF | `beam_deflate` |
//! |---|---|
//! | `open_nif/0` | — (mints a handle, `ST_NONE`, `EOS_BEHAVIOR_CUT`) |
//! | `close_nif/1` | drops the codec (the C's `deflateEnd`/`inflateEnd`), `ST_CLOSED` |
//! | `set_controller_nif/2` | — (local pid only) |
//! | `deflateInit_nif/6` | `Deflate::new` via `DeflateConfig::new` |
//! | `deflateSetDictionary_nif/2` | `Deflate::set_dictionary` (+ `beam_deflate::adler32` for a raw stream) |
//! | `deflateReset_nif/1` | `Deflate::reset` |
//! | `deflateEnd_nif/1` | drops the codec; the C's `deflateEnd` status model |
//! | `deflateParams_nif/3` | `Deflate::set_params` into a 1-byte dummy buffer |
//! | `deflate_nif/4` | the pump over `Deflate::deflate` |
//! | `inflateInit_nif/3` | `Inflate::new` |
//! | `inflateSetDictionary_nif/2` | `Inflate::set_dictionary` |
//! | `inflateGetDictionary_nif/1` | `Inflate::get_dictionary` |
//! | `inflateReset_nif/1` | `Inflate::reset` |
//! | `inflateEnd_nif/1` | drops the codec; `data_error` unless `eos_seen` |
//! | `inflate_nif/4` | the EOS entry step, then the pump over `Inflate::inflate` |
//! | `getStash_nif/1`, `clearStash_nif/1`, `setStash_nif/2` | — (the ETF stash) |
//! | `enqueue_nif/2` | — (the only input path) |
//!
//! # The universal prologue, in the C's order
//!
//! 1. arity/type validation failures ⇒ `badarg` (a `BifRegistry` call already matched the
//!    arity, so this is each body's own argument destructuring);
//! 2. caller is not the stream's controlling process ⇒ raise `error:not_on_controlling_process`;
//! 3. the state precondition ⇒ raise `error:not_initialized` or `error:already_initialized`.
//!
//! The handle decodes through `access::ref_thing_id`, so a term that is not an internal
//! reference — or one this module never minted — is `badarg`, exactly as the C's
//! `enif_get_resource` fails.
//!
//! # Representation
//!
//! * **Handle** — an on-heap internal `Ref` thing (`access::make_ref_thing`) whose id
//!   starts at [`STREAM_ID_BASE`] (`1 << 44`), disjoint from the crate's other reference
//!   id spaces (compiled patterns `1 << 32`, ETS continuations `1 << 40` and `1 << 41`,
//!   compiled specs `1 << 48`, runtime resources `1 << 52`, `erl_ddll` `1 << 53`) and far
//!   below `2^59` so it still fits the 60-bit slot that thing's id word carries.
//!   `is_reference/1` is `true` for it, and it survives map keys, ETS keys and an ETF
//!   round trip, so it behaves like the C's resource term for everything `zlib.erl`
//!   does with it.
//! * **Registry** — one process-global `id -> Stream` map behind the crate's
//!   poison-tolerant lock.
//! * **Queue** — a `VecDeque<Vec<u8>>`, one `Vec<u8>` per enqueued iovec segment; the
//!   pump drains the consumed prefix of the front piece (`enif_ioq_deq`).
//! * **Stash** — an **ETF image** (`[131][body]`), never a raw `Eterm`: the caller's heap
//!   is rewound after the BIF, so a stored word would go stale (the crate's documented
//!   hazard, `src/ets/CONTEXT.md`). `zlib.erl` only ever stashes `{Kind, iolist}`
//!   (`save_progress/3`), so the round trip is cheap. Encode with
//!   `Encoder::new(ctx.atoms())` + `encode` + `finish`; decode with
//!   `decode_term_with_options_used` into a **clone** of `ctx.atoms()` and then commit
//!   the names the decoder appended (`known..scratch.len()`) through
//!   `BifContext::intern_atom`, mirroring `binconv::decode_term_bif`.
//!
//! # `close_nif` leaves a tombstone (a documented residual)
//!
//! The C relies on the garbage collector to reclaim a `zlib_data_t`, so its `close` only
//! runs `deflateEnd`/`inflateEnd` and flips the state to `ST_CLOSED` — a **second**
//! `close` still finds the resource and answers `not_initialized`. This module keeps the
//! registry entry for the same reason: `close` drops the compressor, the queue and the
//! stash and sets [`State::Closed`], but the entry itself is never removed (this crate's
//! bounded M1 registry leak, like the compiled-pattern and ETS spec registries).
//!
//! # `deflateEnd`'s status, inferred
//!
//! zlib's `deflateEnd` answers `Z_DATA_ERROR` only when its `status` is `BUSY_STATE`,
//! i.e. a `deflate` call started a stream that never reached `Z_STREAM_END`.
//! `beam_deflate::Deflate` keeps that state private, so [`Stream`] models it with two
//! flags: `started` is set by any pump that consumed or produced bytes or ran a
//! non-`Flush::None` flush, and `finished` is set when a pump step reported
//! `Status::StreamEnd`. `deflateEnd_nif` therefore raises `data_error` when
//! `started && !finished`, or when the input queue is non-empty — the C's
//! `deflateEnd(...) != Z_OK || ioq_size > 0`. A stream that never deflated at all is
//! `ok` (oracle-verified), and `deflateReset` clears both flags.
//!
//! # Codes with no arm in this port
//!
//! `Z_ERRNO` (the C's integer `errno`) is unreachable here: `beam_deflate` has no syscall
//! layer, so `Status` has no such variant. `Z_VERSION_ERROR` is unreachable too: a `zlib`
//! header version mismatch is a compile-time concern of the C library, and
//! `beam_deflate` reports a configuration it cannot accept as `BadArg` at construction
//! instead. `NeedDict` is never a `zlib_return` input (the pump classifies it), so it
//! lands on the C's `default:` arm (`unknown_error`).
//!
mod deflate;
mod inflate;
mod lifecycle;
mod pump;

// The byte-exact golden suite for the whole stream surface. It is deliberately long: it
// embeds the oracle goldens (per-level leaf vectors, the whole 129-byte level-6 stream, the
// one-shot framings, the dictionary pair) plus a comprehensive matrix over every key, the
// same convention as `beam_deflate/tests/deflate_goldens.rs`. Test-only: it cannot affect a
// release build.
#[cfg(test)]
mod tests;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use beam_common::{BeamError, ExitReason, Raise, Result};
use beam_deflate::{Flush, Strategy};
use beam_process::ProcessId;
use beam_term::Eterm;

use crate::access;
use crate::context::BifContext;

/// The first id this module hands out for a stream handle.
///
/// `1 << 44`: far above every other on-heap reference id space in this crate
/// (`src/binary.rs` compiled patterns `1 << 32`, `src/ets.rs` continuations `1 << 40`
/// and compiled specs `1 << 48`, `src/ets/reverse.rs` reverse continuations `1 << 41`,
/// `src/runtime/atomics.rs` resources `1 << 52`, `src/runtime/ddll/monitor.rs`
/// `1 << 53`) so a handle can never be mistaken for one of those, and well below `2^59`
/// so it still fits the 60-bit small integer an internal reference's id word carries.
pub(super) const STREAM_ID_BASE: u64 = 1 << 44;

/// `?EOS_BEHAVIOR_ERROR` (`zlib.erl:134`) — a further stream after `Z_STREAM_END` is
/// `error:data_error`.
pub(super) const EOS_BEHAVIOR_ERROR: u8 = 0;

/// `?EOS_BEHAVIOR_RESET` (`zlib.erl:135`) — the inflater is reset and accepts the next
/// stream (what `zlib:gunzip/1` uses for concatenated gzip members).
pub(super) const EOS_BEHAVIOR_RESET: u8 = 1;

/// `?EOS_BEHAVIOR_CUT` (`zlib.erl:136`) — input after `Z_STREAM_END` is dropped. The
/// behaviour `open_nif/0` installs, as the C does (`zlib_nif.c:543`).
pub(super) const EOS_BEHAVIOR_CUT: u8 = 2;

/// The C's `enif_inspect_iovec(env, 256, ...)` segment cap (`zlib_nif.c:985`).
///
/// `enqueue_nif` walks at most this many pieces and hands the rest back as
/// `{continue, Tail}` for `zlib.erl`'s `enqueue_input_1/2` to re-submit.
pub(super) const MAX_QUEUE_SEGMENTS: usize = 256;

/// The `stream` state — the C's `ST_NONE`/`ST_DEFLATE`/`ST_INFLATE`/`ST_CLOSED`
/// (`zlib_nif.c`, `zlib_data_t.state`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum State {
    /// `ST_NONE` — newly opened, or just ended. Either init may be called.
    New,
    /// `ST_DEFLATE` — a deflate stream is live.
    Deflate,
    /// `ST_INFLATE` — an inflate stream is live.
    Inflate,
    /// `ST_CLOSED` — `close/1` ran. The entry stays as a tombstone (see the module doc).
    Closed,
}

/// One live `zlib_data_t`: its state, its owner, its queued input, its codec and its
/// stash.
pub(super) struct Stream {
    /// The C's `state`; decides which NIFs may run.
    pub(super) state: State,
    /// The C's `controlling_process` — the only process allowed to call the stream's
    /// NIFs (see [`with_stream`]).
    pub(super) controller: ProcessId,
    /// The queued input pieces (`enif_ioq`), oldest first.
    pub(super) queue: VecDeque<Vec<u8>>,
    /// The deflate side, present exactly when `state` is [`State::Deflate`].
    pub(super) deflate: Option<beam_deflate::Deflate>,
    /// The inflate side, present exactly when `state` is [`State::Inflate`].
    pub(super) inflate: Option<beam_deflate::Inflate>,
    /// `?EOS_BEHAVIOR_*` — what `inflate_nif` does with input that follows
    /// `Z_STREAM_END`.
    pub(super) eos_behavior: u8,
    /// The C's `eos_seen`: a pump step reported `Status::StreamEnd`. `inflateEnd_nif` is
    /// the only place a truncated stream is detected through it.
    pub(super) eos_seen: bool,
    /// The deflate-side mirror of `eos_seen` — zlib's `FINISH_STATE`, which decides
    /// whether `deflateEnd` answers `Z_DATA_ERROR` (see the module doc).
    pub(super) finished: bool,
    /// Whether any deflate pump started the stream — zlib's `BUSY_STATE` half of the
    /// same rule (see the module doc).
    pub(super) started: bool,
    /// `windowBits < 0`: no zlib wrapper, so `deflateSetDictionary` answers the Adler-32
    /// of the bytes themselves (zlib does not update `adler` in raw mode).
    pub(super) is_raw: bool,
    /// The inflater reported `Status::NeedDict` and no dictionary has been installed
    /// since — zlib's `mode == DICT`.
    ///
    /// `beam_deflate::Inflate::set_dictionary` collapses zlib's `Z_STREAM_ERROR`
    /// (`state->wrap != 0 && state->mode != DICT`) and `Z_DATA_ERROR` (a mismatched
    /// dictionary id) into one `BadArg`, so this flag supplies the discriminator the two
    /// observable answers need: without it the C answers `Z_STREAM_ERROR` (the
    /// `stream_error` raise, oracle-verified on a fresh framed stream), with it a
    /// rejected dictionary is `Z_DATA_ERROR` (`data_error`).
    pub(super) need_dict_pending: bool,
    /// The stashed term as an ETF image (`[131][body]`), or `None` when empty.
    pub(super) stash: Option<Vec<u8>>,
}

impl Stream {
    /// A fresh stream in `ST_NONE` owned by `controller`, with the C's `open_nif`
    /// defaults (`eos_behavior` `cut`, no stash, an empty queue).
    pub(super) fn new(controller: ProcessId) -> Stream {
        Stream {
            state: State::New,
            controller,
            queue: VecDeque::new(),
            deflate: None,
            inflate: None,
            eos_behavior: EOS_BEHAVIOR_CUT,
            eos_seen: false,
            finished: false,
            started: false,
            is_raw: false,
            need_dict_pending: false,
            stash: None,
        }
    }

    /// `zlib_internal_close` — drop the codec, the queue and the stash, and latch
    /// `ST_CLOSED`.
    pub(super) fn close(&mut self) {
        self.deflate = None;
        self.inflate = None;
        self.queue.clear();
        self.stash = None;
        self.eos_seen = false;
        self.need_dict_pending = false;
        // Both status mirrors describe the codec that was just dropped, so they go with it.
        self.started = false;
        self.finished = false;
        self.state = State::Closed;
    }

    /// `zlib_reset_input` — empty the queue and drop the stash, leaving the state alone.
    pub(super) fn reset_input(&mut self) {
        self.queue.clear();
        self.stash = None;
    }

    /// The live deflate codec.
    ///
    /// The state check is [`with_stream`]'s; this accessor folds the
    /// `State::Deflate`-without-a-compressor case into the same `not_initialized` answer
    /// because the two are always set and cleared together, so it cannot be reached (and
    /// a raise is a better outcome than a panic should that ever change).
    pub(super) fn deflate_mut(&mut self) -> Result<&mut beam_deflate::Deflate> {
        match (&self.state, self.deflate.as_mut()) {
            (State::Deflate, Some(deflate)) => Ok(deflate),
            _ => Err(raise_atom("not_initialized")),
        }
    }

    /// The live inflate codec — the [`Stream::deflate_mut`] twin.
    pub(super) fn inflate_mut(&mut self) -> Result<&mut beam_deflate::Inflate> {
        match (&self.state, self.inflate.as_mut()) {
            (State::Inflate, Some(inflate)) => Ok(inflate),
            _ => Err(raise_atom("not_initialized")),
        }
    }
}

/// The process-global stream registry (`id -> Stream`).
fn streams() -> &'static Mutex<HashMap<u64, Stream>> {
    static STREAMS: OnceLock<Mutex<HashMap<u64, Stream>>> = OnceLock::new();
    STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Locks the stream registry, recovering the guard if another thread panicked while
/// holding it (a poison must not turn a stream operation into a crash).
pub(super) fn lock_streams() -> MutexGuard<'static, HashMap<u64, Stream>> {
    streams().lock().unwrap_or_else(|e| e.into_inner())
}

/// The next stream id (monotonically increasing from [`STREAM_ID_BASE`]).
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(STREAM_ID_BASE);

/// Stores `stream` under a fresh id and returns that id.
pub(super) fn insert_stream(stream: Stream) -> u64 {
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
    lock_streams().insert(id, stream);
    id
}

/// The `error`-class raise of the bare atom `reason` — how the C reports
/// `enif_raise_exception(env, am_<reason>)`.
pub(super) fn raise_atom(reason: &'static str) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(reason)))
}

/// The state precondition a NIF imposes on its stream, checked after the controlling
/// process (the C's `d->state != ST_…` tests).
pub(super) enum Requires {
    /// `deflateInit`/`inflateInit`: the stream must be untouched, else
    /// `already_initialized`.
    Init,
    /// A live deflate stream, else `not_initialized`.
    Deflate,
    /// A live inflate stream, else `not_initialized`.
    Inflate,
    /// Either codec (the `enqueue_nif` precondition), else `not_initialized`.
    Open,
    /// Anything but `Closed` (`close_nif`), else `not_initialized`.
    Closable,
}

/// Runs one stream NIF's universal prologue and then `body` with the stream locked.
///
/// The order is the C's: the handle decodes first (`badarg` for anything that is not an
/// internal reference this module minted), then the controlling-process check raises
/// `not_on_controlling_process`, then [`Requires`] raises
/// `not_initialized`/`already_initialized`. Every argument-type check a body needs has
/// already run (the bodies decode their own arguments before calling in), so a malformed
/// argument is `badarg` even when the caller is not the owner — exactly the C's single
/// `if (argc != n || !get_zlib_data(...) || !enif_get_int(...))` condition.
///
/// The registry stays locked for the whole body: `zlib.erl` never re-enters a stream NIF
/// from inside one, and holding the guard is what makes the state transitions atomic.
pub(super) fn with_stream<R>(
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    requires: Requires,
    body: impl FnOnce(&mut Stream, &mut BifContext<'_>) -> Result<R>,
) -> Result<R> {
    let id = access::ref_thing_id(handle)?;
    let caller = ctx.self_pid();
    let mut guard = lock_streams();
    let stream = guard.get_mut(&id).ok_or(BeamError::BadArg)?;
    if stream.controller != caller {
        return Err(raise_atom("not_on_controlling_process"));
    }
    let satisfied = match requires {
        Requires::Init => stream.state == State::New,
        Requires::Deflate => stream.state == State::Deflate,
        Requires::Inflate => stream.state == State::Inflate,
        Requires::Open => matches!(stream.state, State::Deflate | State::Inflate),
        Requires::Closable => stream.state != State::Closed,
    };
    if !satisfied {
        return Err(match requires {
            Requires::Init => raise_atom("already_initialized"),
            _ => raise_atom("not_initialized"),
        });
    }
    body(stream, ctx)
}

// ---------------------------------------------------------------------------
// Argument decoding (the Erlang layer has already turned its atoms into integers)
// ---------------------------------------------------------------------------

/// The `i64` value of a small-integer argument, or `badarg`.
///
/// Every integer the `zlib.erl` validation layer can produce (`arg_level` 0..=9 or -1,
/// `arg_bitsz` -15..=-8 | 8..47, `arg_mem` 1..=9, `arg_strategy`/`arg_eos_behavior`
/// 0..=4 and `arg_flush` 0|2|3|4) is a small integer, so a bignum here is `badarg` —
/// which is also what `enif_get_int` reports for a value outside `int`'s range.
pub(super) fn int_arg(term: Eterm) -> Result<i64> {
    if access::is_small(term) {
        Ok(beam_term::signed_value(term))
    } else {
        Err(BeamError::BadArg)
    }
}

/// A positive chunk size from `deflate_nif`/`inflate_nif`'s second and third arguments.
///
/// `zlib.erl` hardcodes 8 KiB/16 KiB (`zlib.erl:141-144`), so a zero or negative value
/// is unreachable through it; it is refused rather than clamped because a zero output
/// chunk would make the pump's "output buffer not filled" test meaningless.
pub(super) fn chunk_arg(term: Eterm) -> Result<usize> {
    let value = int_arg(term)?;
    usize::try_from(value)
        .ok()
        .filter(|&size| size > 0)
        .ok_or(BeamError::BadArg)
}

/// `zlib.erl`'s flush atoms as integers (`?Z_NO_FLUSH` 0, `?Z_SYNC_FLUSH` 2,
/// `?Z_FULL_FLUSH` 3, `?Z_FINISH` 4) mapped onto [`Flush`].
///
/// `1` (`Z_PARTIAL_FLUSH`, deliberately absent from `arg_flush/1`) and `5` (`Z_BLOCK`,
/// `deflateParams`' own internal flush) have no `zlib` binding, so they are `badarg`.
pub(super) fn flush_arg(term: Eterm) -> Result<Flush> {
    match int_arg(term)? {
        0 => Ok(Flush::None),
        2 => Ok(Flush::Sync),
        3 => Ok(Flush::Full),
        4 => Ok(Flush::Finish),
        _ => Err(BeamError::BadArg),
    }
}

/// `zlib.erl`'s `arg_strategy/1` integers (`?Z_DEFAULT_STRATEGY` 0 … `?Z_RLE` 3) mapped
/// onto [`Strategy`], plus zlib's own `Z_FIXED` (4).
///
/// Anything else is `deflateInit2`'s `Z_STREAM_ERROR`, i.e. the `stream_error` raise —
/// never `badarg`, because the argument is a valid integer and only its value is refused
/// by zlib.
pub(super) fn strategy_arg(term: Eterm) -> Result<Strategy> {
    match int_arg(term)? {
        0 => Ok(Strategy::Default),
        1 => Ok(Strategy::Filtered),
        2 => Ok(Strategy::HuffmanOnly),
        3 => Ok(Strategy::Rle),
        4 => Ok(Strategy::Fixed),
        _ => Err(raise_atom("stream_error")),
    }
}

// ---------------------------------------------------------------------------
// The key table and the local-call predicate
// ---------------------------------------------------------------------------

/// The 19 `zlib.erl` NIF keys, in the module's `-nifs([...])` order.
///
/// This table is the one definition of the surface: [`register`] installs exactly these
/// keys through the three submodules and [`is_nif_key`] answers the runtime's local-call
/// predicate from it, so the two can never disagree.
pub(super) const NIF_KEYS: &[(&str, u32)] = &[
    ("close_nif", 1),
    ("open_nif", 0),
    ("set_controller_nif", 2),
    ("deflateInit_nif", 6),
    ("deflateSetDictionary_nif", 2),
    ("deflateReset_nif", 1),
    ("deflateEnd_nif", 1),
    ("deflateParams_nif", 3),
    ("deflate_nif", 4),
    ("inflateInit_nif", 3),
    ("inflateSetDictionary_nif", 2),
    ("inflateGetDictionary_nif", 1),
    ("inflateReset_nif", 1),
    ("inflateEnd_nif", 1),
    ("inflate_nif", 4),
    ("getStash_nif", 1),
    ("clearStash_nif", 1),
    ("setStash_nif", 2),
    ("enqueue_nif", 2),
];

/// Whether `Function/Arity` is one of the 19 `zlib` stream keys.
///
/// `crate::zlib::serves_local_call` short-circuits on the module name and delegates here.
pub(crate) fn is_nif_key(function: &str, arity: usize) -> bool {
    NIF_KEYS
        .iter()
        .any(|&(name, key_arity)| name == function && key_arity as usize == arity)
}

/// Installs the 19 `zlib` NIF keys ([`NIF_KEYS`]) into `reg`.
///
/// Three submodules own the bodies: [`lifecycle`] 7 (`open`/`close`/`set_controller`/
/// `enqueue` and the three stash keys), [`deflate`] 6 and [`inflate`] 6 — 19 in total,
/// exactly the `-nifs([...])` list `zlib.erl:100-106` declares.
pub(crate) fn register(reg: &mut crate::registry::BifRegistry) {
    lifecycle::register(reg);
    deflate::register(reg);
    inflate::register(reg);
}

/// Asserts that `result` is the `error`-class raise of the **bare atom** `name` —
/// what [`raise_atom`] builds, without needing `PartialEq` on
/// [`BeamError`](beam_common::BeamError) (which does not implement it).
#[cfg(test)]
pub(super) fn assert_raise_atom<Ok>(result: Result<Ok>, name: &'static str) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(
                raise.class,
                beam_common::ExceptionClass::Error,
                "error:{name} must be the `error` class"
            );
            assert_eq!(
                raise.reason,
                ExitReason::atom(name),
                "error:{name} must carry that bare atom"
            );
            assert!(
                raise.stacktrace.is_empty(),
                "error:{name} is raised without a stacktrace"
            );
        }
        Err(other) => panic!("expected the raise error:{name}, got {other:?}"),
        Ok(_) => panic!("expected the raise error:{name}, got a value"),
    }
}

/// Asserts that `result` is [`BeamError::BadArg`] — the argument fault (never a raise).
#[cfg(test)]
pub(super) fn assert_badarg<Ok>(result: Result<Ok>) {
    match result {
        Err(BeamError::BadArg) => {}
        Err(other) => panic!("expected badarg, got {other:?}"),
        Ok(_) => panic!("expected badarg, got a value"),
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use beam_term::make_small;

    /// The key table is exactly `zlib.erl`'s `-nifs([...])` list: 19 keys over 19
    /// distinct names.
    #[test]
    fn the_key_table_is_the_preloaded_module_s_nifs_list() {
        assert_eq!(NIF_KEYS.len(), 19);
        for (index, &(name, arity)) in NIF_KEYS.iter().enumerate() {
            assert!(
                NIF_KEYS[index + 1..].iter().all(|&(other, _)| other != name),
                "{name} appears once"
            );
            assert!(is_nif_key(name, arity as usize), "{name}/{arity} is a key");
        }
    }

    /// The predicate is exact on the arity: `open_nif/1` is not a key, `open_nif/0` is.
    #[test]
    fn the_local_call_predicate_is_arity_exact() {
        assert!(is_nif_key("open_nif", 0));
        assert!(!is_nif_key("open_nif", 1));
        assert!(is_nif_key("deflate_nif", 4));
        assert!(!is_nif_key("deflate_nif", 3));
        assert!(is_nif_key("getStash_nif", 1));
        assert!(!is_nif_key("getstash_nif", 1));
        // The `-removed` functions are not keys (the oracle answers `undef` for them).
        assert!(!is_nif_key("inflateChunk", 1));
        assert!(!is_nif_key("getBufSize", 1));
        assert!(!is_nif_key("compress", 1));
    }

    /// The flush table is `arg_flush/1`'s, and only its four values are accepted.
    #[test]
    fn the_flush_table_matches_arg_flush() {
        assert_eq!(flush_arg(make_small(0)).expect("none"), Flush::None);
        assert_eq!(flush_arg(make_small(2)).expect("sync"), Flush::Sync);
        assert_eq!(flush_arg(make_small(3)).expect("full"), Flush::Full);
        assert_eq!(flush_arg(make_small(4)).expect("finish"), Flush::Finish);
        // `Z_PARTIAL_FLUSH` is deliberately absent from `arg_flush/1`.
        assert_badarg(flush_arg(make_small(1)));
        assert_badarg(flush_arg(make_small(5)));
        assert_badarg(flush_arg(beam_term::NIL));
    }

    /// The strategy table is `arg_strategy/1`'s plus zlib's `Z_FIXED`; every other
    /// integer is zlib's `Z_STREAM_ERROR`, i.e. the `stream_error` raise.
    #[test]
    fn the_strategy_table_is_the_c_s_range() {
        assert_eq!(strategy_arg(make_small(0)).expect("default"), Strategy::Default);
        assert_eq!(strategy_arg(make_small(1)).expect("filtered"), Strategy::Filtered);
        assert_eq!(
            strategy_arg(make_small(2)).expect("huffman_only"),
            Strategy::HuffmanOnly
        );
        assert_eq!(strategy_arg(make_small(3)).expect("rle"), Strategy::Rle);
        assert_eq!(strategy_arg(make_small(4)).expect("fixed"), Strategy::Fixed);
        assert_raise_atom(strategy_arg(make_small(5)), "stream_error");
        assert_badarg(strategy_arg(beam_term::NIL));
    }

    /// A chunk size must be a positive integer: `zlib.erl` hardcodes 8 KiB/16 KiB, and a
    /// zero output chunk would make the pump's "not filled" test meaningless.
    #[test]
    fn a_chunk_size_must_be_positive() {
        assert_eq!(chunk_arg(make_small(8192)).expect("8 KiB"), 8192);
        assert_badarg(chunk_arg(make_small(0)));
        assert_badarg(chunk_arg(make_small(-1)));
        assert_badarg(chunk_arg(beam_term::NIL));
    }

    /// `open_nif`'s defaults are the C's: `ST_NONE`, the caller as the controller, the
    /// `cut` eos behaviour, an empty queue and no stash.
    #[test]
    fn a_fresh_stream_carries_the_open_nif_defaults() {
        let owner = ProcessId::new(10, 0);
        let stream = Stream::new(owner);
        assert_eq!(stream.state, State::New);
        assert_eq!(stream.controller, owner);
        assert_eq!(stream.eos_behavior, EOS_BEHAVIOR_CUT);
        assert!(!stream.eos_seen);
        assert!(!stream.started && !stream.finished);
        assert!(!stream.is_raw && !stream.need_dict_pending);
        assert!(stream.queue.is_empty());
        assert!(stream.stash.is_none());
        assert!(stream.deflate.is_none() && stream.inflate.is_none());
    }

    /// `close` latches `ST_CLOSED` and releases everything — the tombstone the module doc
    /// describes (the registry entry itself is what makes a second `close`
    /// `not_initialized`).
    #[test]
    fn close_drops_everything_and_latches_closed() {
        let mut stream = Stream::new(ProcessId::new(10, 0));
        stream.queue.push_back(vec![1, 2, 3]);
        stream.stash = Some(vec![131, 97, 1]);
        stream.started = true;
        stream.eos_seen = true;
        stream.close();
        assert_eq!(stream.state, State::Closed);
        assert!(stream.queue.is_empty());
        assert!(stream.stash.is_none());
        assert!(!stream.started && !stream.eos_seen);
        assert!(stream.deflate_mut().is_err());
        assert!(stream.inflate_mut().is_err());
    }

    /// A fresh handle is minted above every other id space and stays inside the 60-bit
    /// slot the ref thing's id word carries.
    #[test]
    fn stream_ids_sit_in_their_own_space() {
        assert_eq!(STREAM_ID_BASE, 1 << 44);
        for other in [1u64 << 32, 1 << 40, 1 << 41, 1 << 48, 1 << 52, 1 << 53] {
            assert!(STREAM_ID_BASE.min(other) < STREAM_ID_BASE.max(other));
            assert!(
                !(STREAM_ID_BASE..STREAM_ID_BASE + (1 << 20)).contains(&other),
                "the stream space is disjoint from {other}"
            );
        }
        assert!(STREAM_ID_BASE < 1 << 59, "fits the ref id's small-integer slot");
        // The id counter is process-global, so a test running in parallel may mint between
        // these two calls: `insert_stream` is pinned to hand out its own pre-increment
        // value (distinct, from the stream space, monotonic), never a shared or repeated id.
        let first = insert_stream(Stream::new(ProcessId::new(1, 0)));
        let second = insert_stream(Stream::new(ProcessId::new(1, 0)));
        assert!(second > first, "ids increase monotonically ({first} then {second})");
        assert!(first >= STREAM_ID_BASE);
        {
            let streams = lock_streams();
            assert!(streams.contains_key(&first));
            assert!(streams.contains_key(&second));
        }
    }
}
