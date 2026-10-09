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

//! The **fun** builder and readers — the closure interior. `beam_term::fun` owns
//! the frozen header/descriptor layout; this module only obtains storage and
//! forwards the reads.
use beam_common::{BeamError, Result};
use beam_term::Eterm;

use super::{reserve, Slack};
use crate::context::ExecutionContext;
use crate::machine::Machine;

/// Builds a local **fun** (closure) term.
///
/// # Frozen word layout
///
/// The on-heap header/descriptor layout is owned by [`beam_term::fun`] (the
/// frozen-layout lowest common ancestor); this function only obtains the storage
/// from the execution context and writes it through that module's
/// [`make_fun_header`](beam_term::fun::make_fun_header) and
/// [`FUN_DESC_WORDS_LOCAL`](beam_term::fun::FUN_DESC_WORDS_LOCAL).
///
/// C's `ErlFunThing` (`erl_fun.h:65-99`) is
/// `[thing_word, entry, env[0..num_free]]`, where `entry` is a **pointer** to an
/// out-of-line `ErlFunEntry`/`Export` that owns the code address, `uniq[16]` and
/// `index` (`erl_fun.c`, `emu/instrs.tab:545-584` `make_fun3`).
///
/// M1 has no code addresses or fun-entry table, so the single `entry` pointer is
/// **inlined as two descriptor words** — the
/// [`FunKind::Local`](beam_term::fun::FunKind::Local) substitution
/// [`beam_term::fun`] owns. The resulting block is
/// `1 (header) + 2 (uniq, index) + num_free` words:
///
/// ```text
/// word 0            MAKE_FUN_HEADER(arity, num_free, Local)  (exact C encoding)
/// word 1            uniq   (u64, the fun's identity)  — replaces the entry pointer
/// word 2            index  (u32, the fun's lambda-table index)
/// words 3 .. 3+n    env[0..num_free]                  (identical to C's order/position)
/// ```
///
/// `arity` and `num_free` must each fit 8 bits; a larger value yields
/// [`BeamError::SystemLimit`]. The block is reserved **rooted** for
/// `1 + FUN_DESC_WORDS_LOCAL + num_free + extra` words **before** the `read` closure
/// runs, so the free variables are read from the machine's post-collection state and
/// a non-small integer operand is materialized into the spare tail (never a second
/// collecting allocation). A closure returning the wrong free-variable count is a
/// typed [`crate::error::unsupported`]. The pointer is valid until the next
/// collecting allocation.
pub fn make_fun<R>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    arity: u32,
    uniq: u64,
    index: u32,
    num_free: usize,
    extra: usize,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack) -> Result<Vec<Eterm>>,
{
    if arity as usize > beam_term::fun::FUN_MAX_FIELD || num_free > beam_term::fun::FUN_MAX_FIELD {
        return Err(BeamError::SystemLimit);
    }
    let header =
        beam_term::fun::make_fun_header(arity as usize, num_free, beam_term::fun::FunKind::Local)
            .map_err(BeamError::from)?;
    let env_offset = 1 + beam_term::fun::FUN_DESC_WORDS_LOCAL;
    let hp = reserve(machine, ctx, env_offset + num_free + extra, live)?;
    // SAFETY: the reserve returned `env_offset + num_free + extra` exclusively-owned,
    // writable words; the `extra`-word tail after the env words is the slack.
    let mut slack = unsafe { Slack::new(hp.add(env_offset + num_free), extra) };
    let free = read(machine, &mut slack)?;
    if free.len() != num_free {
        return Err(crate::error::unsupported(
            "make_fun: free-variable count mismatch",
        ));
    }
    // SAFETY: the reserve block is live and exclusively owned. Word 0 receives the fun
    // header, word 1 the `uniq`, word 2 the `index` and words
    // `env_offset..env_offset + num_free` the free variables — every write is in
    // bounds.
    let root = unsafe {
        *hp = header;
        *hp.add(1) = Eterm::from_raw(uniq as usize);
        *hp.add(2) = Eterm::from_raw(index as usize);
        for (i, var) in free.iter().enumerate() {
            *hp.add(env_offset + i) = *var;
        }
        hp
    };
    Ok(beam_term::make_boxed(root))
}

/// `is_any_fun(t)` — true for the boxed fun/closure term built by [`make_fun`].
pub fn is_fun(t: Eterm) -> bool {
    // SAFETY: `is_any_fun` checks `is_boxed` before dereferencing the header word.
    unsafe { beam_term::is_any_fun(t) }
}

/// The arity of a fun term; a non-fun yields [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_arity`] (the layout owner) over the term's
/// header word.
pub fn fun_arity(t: Eterm) -> Result<u32> {
    Ok(beam_term::fun::fun_arity(Eterm::from_raw(fun_header_word(t)?)) as u32)
}

/// The number of free variables (`env` size) of a fun term; a non-fun yields
/// [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_num_free`] (the layout owner) over the term's
/// header word.
pub fn fun_num_free(t: Eterm) -> Result<usize> {
    Ok(beam_term::fun::fun_num_free(Eterm::from_raw(
        fun_header_word(t)?,
    )))
}

/// The unique identity (`uniq`) stored in a fun term; a non-fun yields
/// [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_uniq`] (the layout owner), which reads the
/// [`FunKind::Local`](beam_term::fun::FunKind::Local) descriptor word — the only
/// kind this engine builds.
pub fn fun_uniq(t: Eterm) -> Result<u64> {
    // SAFETY: `t` must be a live fun term (the caller's contract; `make_fun` only
    // ever builds `FunKind::Local` funs); `fun_uniq` re-checks the kind before
    // reading the descriptor word.
    unsafe { beam_term::fun::fun_uniq(t) }.map_err(BeamError::from)
}

/// The lambda-table index stored in a fun term; a non-fun yields
/// [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_index`] (the layout owner).
pub fn fun_index(t: Eterm) -> Result<u32> {
    // SAFETY: `t` must be a live fun term (the caller's contract); `fun_index`
    // re-checks the kind (`Local`/`Imported`) before reading the descriptor word.
    unsafe { beam_term::fun::fun_index(t) }.map_err(BeamError::from)
}

/// The free variables of a fun term, in `env` order; a non-fun yields
/// [`BeamError::BadArg`].
///
/// Forwarded to [`beam_term::fun::fun_env`] (the layout owner). The returned slice
/// borrows the term's storage, so the caller must keep the owning heap alive and
/// unmoved.
pub fn fun_free<'a>(t: Eterm) -> Result<&'a [Eterm]> {
    // SAFETY: `t` must be a live fun term whose storage the caller keeps alive and
    // unmoved for `'a` (the documented borrow contract); `fun_env` re-checks the
    // fun subtag before computing the env slice.
    unsafe { beam_term::fun::fun_env(t) }.map_err(BeamError::from)
}

/// The raw header word of a fun term, or [`BeamError::BadArg`] if `t` is not a fun.
///
/// The type check [`fun_arity`]/[`fun_num_free`] share before forwarding the header
/// word to [`beam_term::fun`].
fn fun_header_word(t: Eterm) -> Result<usize> {
    if !is_fun(t) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_fun` established that `t` is boxed, so its header word is a live,
    // initialised term word.
    Ok(unsafe { (*beam_term::boxed_val(t)).raw() })
}
