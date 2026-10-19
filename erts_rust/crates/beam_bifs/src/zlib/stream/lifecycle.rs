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

//! The stream lifecycle and input/stash NIF bodies (`zlib_nif.c:473-602`, `:971-996`).
//!
//! Seven bodies: `open_nif/0`, `close_nif/1`, `set_controller_nif/2`, `enqueue_nif/2`
//! and the three stash keys.
//!
//! # The stash is an **ETF image**
//!
//! The C keeps the stashed term in its own `enif_alloc_env` (a private heap that
//! outlives the call). This crate cannot: the caller's heap is rewound after the
//! BIF, so a stored `Eterm` would go stale (the hazard `src/ets/CONTEXT.md` records).
//! The stash is therefore an `[131][body]` image through
//! [`beam_codec::Encoder`]/[`beam_codec::decode_term_with_options_used`], and the
//! names the decoder appends are committed to the context's table the way
//! `binconv::decode_term_bif` does it.
//!
//! # `getStash_nif` is the first key a *foreign* process reaches
//!
//! `zlib.erl`'s `restore_progress/2` calls `getStash_nif` before every
//! `deflate`/`inflate`, so the controller check on it is load-bearing: a foreign
//! process's `zlib:deflate/3` is `not_on_controlling_process` raised *here*, and a
//! foreign `zlib:deflateParams/3` reaches here too (via its own `deflate/3`).
//!
//! # `Requires::Closable` is "the process check, plus not closed"
//!
//! The C imposes **no** state precondition on `set_controller_nif`,
//! `getStash_nif`/`setStash_nif`/`clearStash_nif` (`zlib_nif.c:583-602`, `:473-522`),
//! so `Requires::Closable` (anything but `Closed`) is the closest the draft's enum
//! offers. `zlib.erl` never touches a closed stream's stash or controller, so the
//! difference is unobservable through the module; the [tombstone](super::State)
//! is what keeps a second `close` a `not_initialized`.

use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{Atom, Eterm};

use crate::access;
use crate::binary;
use crate::context::BifContext;

use super::{
    insert_stream, raise_atom, with_stream, Requires, Stream, MAX_QUEUE_SEGMENTS,
};

/// `open_nif/0` — mint a handle for a fresh stream owned by the caller.
fn open_nif(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let id = insert_stream(Stream::new(ctx.self_pid()));
    access::make_ref_thing(ctx.heap(), id)
}

/// `close_nif/1` — `zlib_internal_close`; a second `close` is `not_initialized`
/// (the registry entry stays as the tombstone).
fn close_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Closable, |stream, ctx| {
        stream.close();
        ctx.intern_atom("ok")
    })
}

/// `set_controller_nif/2` — hand the stream to another **local** process.
///
/// The pid is decoded before the prologue, as the C's `enif_get_local_pid` is part
/// of its single `badarg` condition: a non-pid *or* an external pid is `badarg`.
/// There is no "inside a call" check in this OTP version.
fn set_controller_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let new_owner = ProcessId::from_eterm(args[1])?;
    with_stream(ctx, args[0], Requires::Closable, |stream, ctx| {
        stream.controller = new_owner;
        ctx.intern_atom("ok")
    })
}

/// `enqueue_nif/2` — the one input path.
///
/// The argument is `erlang:iolist_to_iovec/1`'s output: a list of binaries that may
/// carry an improper tail. At most [`MAX_QUEUE_SEGMENTS`] segments are taken (the
/// C's `enif_inspect_iovec(env, 256, …)`) and the remainder is handed back as
/// `{continue, Tail}` for `zlib.erl`'s `enqueue_input_1/2` to re-submit; `[]` is
/// `ok`. Each segment is flattened through
/// [`crate::binary::iodata_bytes`] — the same iolist set the one-shots accept — so a
/// non-binary, non-list element is `badarg`.
fn enqueue_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let iovec = args[1];

    with_stream(ctx, args[0], Requires::Open, |stream, ctx| {
        let mut rest = iovec;
        let mut queued = 0usize;
        while !access::is_nil(rest) {
            if queued >= MAX_QUEUE_SEGMENTS {
                let tag = ctx.intern_atom("continue")?;
                return ctx.tuple(&[tag, rest]);
            }
            if !access::is_list(rest) {
                return Err(BeamError::BadArg);
            }
            stream
                .queue
                .push_back(binary::iodata_bytes(access::car(rest))?);
            rest = access::cdr(rest);
            queued += 1;
        }
        ctx.intern_atom("ok")
    })
}

/// Encode `term` into the `[131][body]` stash image.
///
/// # Errors
///
/// The encoder's own failures (`unsupported_external_term` for a term with no wire
/// image), never a fabricated image.
fn encode_stash(ctx: &mut BifContext<'_>, term: Eterm) -> Result<Vec<u8>> {
    let mut encoder = beam_codec::Encoder::new(ctx.atoms());
    encoder.encode(term)?;
    Ok(encoder.finish())
}

/// Decode a stash image onto the context's heap, committing the atom names the
/// decoder appended.
///
/// `binconv::decode_term_bif`'s shape: decode into a **clone** of the shared table
/// (copy-on-write, so only an interning write pays), collect the names at the
/// indices past the clone's original length, then commit them through
/// [`BifContext::intern_atom`] — the indices the decoder wrote into the term are
/// then the shared table's.
///
/// # Errors
///
/// The codec's decode error (a corrupt image) or the context's atom failure.
fn decode_stash(ctx: &mut BifContext<'_>, image: &[u8]) -> Result<Eterm> {
    let mut scratch = ctx.atoms().clone();
    let known = scratch.len();
    let options = beam_codec::DecodeOptions {
        safe: false,
        local_node: None,
        local_hash: None,
    };
    let (term, _used) =
        beam_codec::decode_term_with_options_used(image, ctx.heap(), &mut scratch, &options)?;
    for index in known..scratch.len() {
        if let Some(name) = scratch.name(Atom::from_raw(index as u32)) {
            ctx.intern_atom(name)?;
        }
    }
    Ok(term)
}

/// `getStash_nif/1` — `empty`, or `{ok, Term}` with the stashed term copied out.
fn get_stash_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Closable, |stream, ctx| {
        match stream.stash.clone() {
            None => ctx.intern_atom("empty"),
            Some(image) => {
                let term = decode_stash(ctx, &image)?;
                let tag = ctx.intern_atom("ok")?;
                ctx.tuple(&[tag, term])
            }
        }
    })
}

/// `setStash_nif/2` — store the term; a **second** stash is the bare atom `error`.
fn set_stash_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Closable, |stream, ctx| {
        if stream.stash.is_some() {
            return Err(raise_atom("error"));
        }
        let image = encode_stash(ctx, args[1])?;
        stream.stash = Some(image);
        ctx.intern_atom("ok")
    })
}

/// `clearStash_nif/1` — `ok`, or the bare atom `error` when the stash is empty.
fn clear_stash_nif(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    with_stream(ctx, args[0], Requires::Closable, |stream, ctx| {
        if stream.stash.take().is_none() {
            return Err(raise_atom("error"));
        }
        ctx.intern_atom("ok")
    })
}

/// Installs the seven lifecycle/input/stash keys (`super::NIF_KEYS` is the
/// authoritative list).
pub(super) fn register(reg: &mut crate::registry::BifRegistry) {
    reg.register("zlib", "open_nif", 0, open_nif);
    reg.register("zlib", "close_nif", 1, close_nif);
    reg.register("zlib", "set_controller_nif", 2, set_controller_nif);
    reg.register("zlib", "enqueue_nif", 2, enqueue_nif);
    reg.register("zlib", "getStash_nif", 1, get_stash_nif);
    reg.register("zlib", "clearStash_nif", 1, clear_stash_nif);
    reg.register("zlib", "setStash_nif", 2, set_stash_nif);
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_term::{make_small, AtomTable};

    use crate::zlib::stream::{assert_badarg, assert_raise_atom};

    fn heap() -> Heap {
        Heap::new(1 << 16).expect("test heap")
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "ok",
            "empty",
            "continue",
            "not_initialized",
            "not_on_controlling_process",
            "error",
            "hello",
        ]);
        atoms
    }

    fn context<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
        BifContext::new(heap, atoms, ProcessId::new(1, 0))
    }

    /// `open_nif` mints a handle that `is_reference/1` accepts and that no other
    /// open hands out twice.
    #[test]
    fn open_mints_a_fresh_handle() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let first = open_nif(&mut ctx, &[]).expect("a handle");
        let second = open_nif(&mut ctx, &[]).expect("a handle");
        assert!(access::is_internal_ref(first));
        assert_ne!(
            access::ref_thing_id(first).expect("id"),
            access::ref_thing_id(second).expect("id")
        );
    }

    /// The prologue's order: a bad handle is `badarg`, a foreign caller is
    /// `not_on_controlling_process`, and only then the state.
    #[test]
    fn the_prologue_precedence_is_badarg_controller_state() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        assert_badarg(close_nif(&mut ctx, &[make_small(1)]));

        let handle = open_nif(&mut ctx, &[]).expect("a handle");
        let mut other_heap = Heap::new(1 << 12).expect("test heap");
        let mut other = BifContext::new(&mut other_heap, &atoms, ProcessId::new(2, 0));
        assert_raise_atom(
            close_nif(&mut other, &[handle]),
            "not_on_controlling_process",
        );

        close_nif(&mut ctx, &[handle]).expect("the controller may close");
        assert_raise_atom(close_nif(&mut ctx, &[handle]), "not_initialized");
    }

    /// The stash round-trips a term through its ETF image, and a second `setStash`
    /// is the bare atom `error`.
    #[test]
    fn the_stash_round_trips_through_its_etf_image() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let handle = open_nif(&mut ctx, &[]).expect("a handle");

        assert_eq!(
            get_stash_nif(&mut ctx, &[handle]).expect("empty"),
            ctx.atom("empty").expect("seeded")
        );
        assert_raise_atom(clear_stash_nif(&mut ctx, &[handle]), "error");

        let term = ctx.atom("hello").expect("seeded");
        set_stash_nif(&mut ctx, &[handle, term]).expect("a first stash");
        assert_raise_atom(set_stash_nif(&mut ctx, &[handle, term]), "error");

        let answer = get_stash_nif(&mut ctx, &[handle]).expect("a tuple");
        assert_eq!(beam_codec::term::tuple_arity(answer).expect("a tuple"), 2);
        let tag = beam_codec::term::tuple_element(answer, 0).expect("tag");
        assert_eq!(tag, ctx.atom("ok").expect("seeded"));
        let value = beam_codec::term::tuple_element(answer, 1).expect("value");
        assert_eq!(value, ctx.atom("hello").expect("seeded"));

        clear_stash_nif(&mut ctx, &[handle]).expect("clears");
        assert_eq!(
            get_stash_nif(&mut ctx, &[handle]).expect("empty again"),
            ctx.atom("empty").expect("seeded")
        );
    }

    /// `enqueue_nif` needs a live codec (`not_initialized`) and flattens each
    /// segment into its own queued piece.
    #[test]
    fn enqueue_requires_an_open_stream_and_queues_each_segment() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let handle = open_nif(&mut ctx, &[]).expect("a handle");

        assert_raise_atom(
            enqueue_nif(&mut ctx, &[handle, beam_term::NIL]),
            "not_initialized",
        );

        let first = crate::bits::make_binary(ctx.heap(), b"ab").expect("room");
        let second = crate::bits::make_binary(ctx.heap(), b"cd").expect("room");
        let list = ctx.list(&[first, second]).expect("room");
        {
            let id = access::ref_thing_id(handle).expect("id");
            let mut guard = super::super::lock_streams();
            let stream = guard.get_mut(&id).expect("the stream");
            stream.state = super::super::State::Inflate;
        }
        assert_eq!(
            enqueue_nif(&mut ctx, &[handle, list]).expect("ok"),
            ctx.atom("ok").expect("seeded")
        );
        let id = access::ref_thing_id(handle).expect("id");
        let guard = super::super::lock_streams();
        let stream = guard.get(&id).expect("the stream");
        assert_eq!(stream.queue.len(), 2);
        assert_eq!(stream.queue.front().map(Vec::as_slice), Some(&b"ab"[..]));
    }

    /// `set_controller_nif` refuses a non-pid and an external pid with `badarg`, and
    /// hands the stream over so the new owner passes the check.
    #[test]
    fn set_controller_moves_the_stream_to_a_local_pid() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let handle = open_nif(&mut ctx, &[]).expect("a handle");

        assert_badarg(set_controller_nif(&mut ctx, &[handle, make_small(1)]));

        let target = ProcessId::new(7, 0).to_eterm();
        set_controller_nif(&mut ctx, &[handle, target]).expect("a local pid");

        // The new controller passes; the old one no longer does.
        let mut other_heap = Heap::new(1 << 12).expect("test heap");
        let mut new_owner = BifContext::new(&mut other_heap, &atoms, ProcessId::new(7, 0));
        close_nif(&mut new_owner, &[handle]).expect("the new controller may close");
        assert_raise_atom(
            close_nif(&mut ctx, &[handle]),
            "not_on_controlling_process",
        );
    }
}
