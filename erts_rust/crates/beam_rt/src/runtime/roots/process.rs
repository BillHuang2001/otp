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

//! [`ProcessRootsMut`] — the MUTABLE root door of a single [`beam_process::Process`].
//!
//! `beam_process` already owns the write-back half of every one of its root containers: each one
//! gathers its words into a caller's `Vec` and scatters a (possibly rewritten) slice back with the
//! token the gather reported (`Registers`/`Dict`/`SavedArgs`/`SignalQueue`'s
//! `gather_roots`/`scatter_roots` pair). The mailbox is the exception — it has no mutable view at
//! all — so this module rebuilds it through the public envelope API instead (see
//! [`visit_mailbox`]).
//!
//! That is why the door lives **here** and not in the sibling crate: it composes existing public
//! seams, so `beam_process` needs no edit at all. The trait is ours, so the orphan rule is not in
//! play.
//!
//! # The family set and the value gather
//!
//! The words this door visits are the same words
//! [`Runtime::gather_off_heap_roots`](crate::Runtime::gather_off_heap_roots) pushes for the same
//! process — the registers (a parked process's published `x` registers + Erlang stack + the
//! host-published extra roots), the mailbox queue, the dictionary, the saved arguments and the
//! pending signals — which is exactly [`beam_process::Process`]'s own **complete** published root
//! set (its `run_rooted_collection` gathers those five families in this order and scatters them
//! back in it).
//!
//! The traversal ORDER here follows that canonical collection order —
//! registers → mailbox → dict → saved_args → signals — and deliberately **differs** from the value
//! gather's, which pushes the dictionary first and, inside a message, `payload`, `token`, `from`
//! (this door visits `payload`, `from`, `token`). Parity with the value gather is therefore a
//! **set/multiset** property, never an ordering one: the same words, in whatever order each walk
//! happens to take them.

use beam_message::Message;
use beam_term::prelude::Eterm;

use super::RootFamily;

/// Hands `f` the writable root slots of this process, one family slice at a time.
///
/// A local extension trait rather than an inherent method: `beam_process::Process` belongs to a
/// sibling crate (it must not be edited for this), while the trait itself is ours, so implementing
/// it for a foreign type is legal.
///
/// The visitor may replace any element of a slice it is handed, but it must not change the slice's
/// length, reorder it, or depend on any container's iteration order — see the module documentation
/// of [`crate::runtime::roots`] for the three contracts in full. One family is visited at a time:
/// no two borrows of this process are outstanding at once, and the owner writes its buffer back
/// before the next family is gathered (and before any error is propagated).
///
/// # Errors
///
/// Whatever `f` answers. The words gathered for the family that failed — and, for the families
/// already visited, everything they had rewritten — are written back first, so a failed walk leaves
/// every slot in place and loses nothing.
pub(crate) trait ProcessRootsMut {
    /// Visits every root family of this process, tagging each slice with the [`RootFamily`] it
    /// belongs to.
    ///
    /// A family holding no words contributes nothing (the visitor is not called for it), and a
    /// process with no live roots at all is a legal, silent no-op.
    fn for_each_root_mut(
        &mut self,
        f: &mut impl FnMut(RootFamily, &mut [Eterm]) -> beam_common::Result<()>,
    ) -> beam_common::Result<()>;
}

impl ProcessRootsMut for beam_process::Process {
    fn for_each_root_mut(
        &mut self,
        f: &mut impl FnMut(RootFamily, &mut [Eterm]) -> beam_common::Result<()>,
    ) -> beam_common::Result<()> {
        // The canonical order `beam_process`'s own rooted collection uses, so a moving pass and the
        // collector see the families in the same sequence (parity is a set property; this is
        // readability, not correctness).
        visit_gathered(self.registers_mut(), RootFamily::ProcessRegisters, f)?;
        visit_mailbox(self, f)?;
        visit_gathered(self.dict_mut(), RootFamily::ProcessDict, f)?;
        visit_gathered(self.saved_args_mut(), RootFamily::ProcessSavedArgs, f)?;
        visit_gathered(self.signals_mut(), RootFamily::ProcessSignals, f)?;
        Ok(())
    }
}

/// The `gather_roots`/`scatter_roots` half of a `beam_process` root container, unified so the four
/// slice-shaped families share **one** visit (and therefore one write-back rule).
///
/// The two methods are `beam_process`'s own pair; the `usize` is opaque here on purpose — it is a
/// register's live-`x` count for [`Registers`](beam_process::Registers) and a start offset for the
/// other three, and both are simply the token its `scatter_roots` expects alongside the slice it
/// gathered.
trait GatheredRoots {
    /// Appends the container's words to `out`, answering the token its `scatter_roots` takes.
    fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize;

    /// Writes the (relocated) words back, pairing with `gather_roots`' token.
    fn scatter_roots(&mut self, roots: &[Eterm], token: usize);
}

impl GatheredRoots for beam_process::Registers {
    fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        beam_process::Registers::gather_roots(self, out)
    }

    fn scatter_roots(&mut self, roots: &[Eterm], token: usize) {
        beam_process::Registers::scatter_roots(self, roots, token);
    }
}

impl GatheredRoots for beam_process::Dict {
    fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        beam_process::Dict::gather_roots(self, out)
    }

    fn scatter_roots(&mut self, roots: &[Eterm], token: usize) {
        beam_process::Dict::scatter_roots(self, roots, token);
    }
}

impl GatheredRoots for beam_process::SavedArgs {
    fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        beam_process::SavedArgs::gather_roots(self, out)
    }

    fn scatter_roots(&mut self, roots: &[Eterm], token: usize) {
        beam_process::SavedArgs::scatter_roots(self, roots, token);
    }
}

impl GatheredRoots for beam_process::SignalQueue {
    fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        beam_process::SignalQueue::gather_roots(self, out)
    }

    fn scatter_roots(&mut self, roots: &[Eterm], token: usize) {
        beam_process::SignalQueue::scatter_roots(self, roots, token);
    }
}

/// Visits one slice-shaped root family: gathers its words into a **fresh** buffer, hands the whole
/// buffer to `f`, then scatters it back with the gather's own token.
///
/// The buffer is fresh per family, which is what makes `scatter_roots(&buf, token)` correct for
/// both token flavours at once: a register token indexes from the start of the register region
/// (which a fresh buffer *is*), and a start offset is `0`.
///
/// The write-back happens **before** the error is propagated: a visitor that had already replaced
/// one word never loses it, and the owner's container never keeps a stale word while the error is
/// on its way out. A family with no words is skipped entirely — no visitor call, nothing to write
/// back.
fn visit_gathered<K, F>(
    container: &mut K,
    family: RootFamily,
    f: &mut F,
) -> beam_common::Result<()>
where
    K: GatheredRoots,
    F: FnMut(RootFamily, &mut [Eterm]) -> beam_common::Result<()>,
{
    let mut buf: Vec<Eterm> = Vec::new();
    let token = container.gather_roots(&mut buf);
    if buf.is_empty() {
        return Ok(());
    }
    let outcome = f(family, &mut buf);
    container.scatter_roots(&buf, token);
    outcome
}

/// Visits the mailbox family: every queued message's `[payload, from, token]` words.
///
/// # Why drain and rebuild
///
/// `beam_message`'s [`Queue`](beam_message::Queue) exposes no mutable view — no `iter_mut`, no
/// `set_payload` — by design, because a queued [`Message`] owns an amortized payload graph (its
/// off-heap handle list, its refc-binary list, its node entries, and for an oversized payload a
/// whole [`Fragments`](beam_message::Fragments) chain whose words the payload points *into*). So
/// the family is drained and rebuilt with the public envelope API:
///
/// * [`Queue::dequeue`](beam_message::Queue::dequeue) empties the queue into an owned `Vec`, so no
///   borrow of the process is held while the visitor runs;
/// * the drain is `while let Some(..) = dequeue()`, which yields messages in FIFO order, and the
///   rebuild enqueues them in that same order, so **the mailbox is never reordered**;
/// * every message is re-installed exactly once — either rebuilt around its visited words or
///   enqueued as it was drained — so **no message is ever lost or duplicated**, on the success path
///   and on the error path alike;
/// * the rebuild is [`Message::with_payload`], the documented relocation-safe form: it keeps
///   `from`, `token`, the off-heap handles, the refc binaries, the node entries and the fragment
///   chain across. Building a fresh `Message` instead would drop the fragment chain (a
///   use-after-free: the payload would point into fragments nobody owns) and leak the refcounts the
///   message held.
///
/// Because the message **count** and their **order** are unchanged, an in-flight receive scan is
/// unaffected: a [`QueueCursor`](beam_message::QueueCursor) is a positional index, and every
/// position still names the same message after the rebuild (only its words may have moved).
///
/// # The error path
///
/// If `f` answers `Err` for a message, that message is re-enqueued **unrebuilt** — exactly as it
/// was drained — and every message behind it is re-enqueued unchanged before the error is
/// returned. Each message is therefore all-or-nothing (a half-visited envelope never reaches the
/// queue) and the queue is fully rebuilt on **every** exit path, so a failed walk cannot strand a
/// single message in the local `Vec`.
///
/// # `Queue::enqueue`'s immediate check
///
/// [`Queue::enqueue`] runs `debug_assert_enqueue_terms`, which requires the envelope's `from` and
/// `token` to be **immediates**. That predicate is satisfied by construction here: this door is for
/// a *relocation*, and an immediate word maps to itself, so a walk that rewrites it writes the same
/// immediate back. The assertion is deliberately **not** weakened — a visitor that stores a young
/// heap word in `from`/`token` (which the legacy C permits for a heap `token` tuple and a heap
/// `Info`-tuple `from`) must fail loudly rather than leave a dangling envelope word behind.
fn visit_mailbox<F>(
    process: &mut beam_process::Process,
    f: &mut F,
) -> beam_common::Result<()>
where
    F: FnMut(RootFamily, &mut [Eterm]) -> beam_common::Result<()>,
{
    // 1. Drain every message out of the mailbox, in FIFO order. Nothing is visited yet, so no
    //    borrow of the process is outstanding while `f` runs.
    let mut drained: Vec<Message> = Vec::new();
    while let Some(message) = process.mailbox_mut().dequeue() {
        drained.push(message);
    }

    // 2. Visit each message's three term words and re-enqueue it, preserving FIFO order. The
    //    `pending` iterator yields the messages in drain order, so a partial walk that stops on an
    //    error still has every remaining message in hand.
    let mut pending = drained.into_iter();
    while let Some(mut message) = pending.next() {
        let mut words = [message.payload(), message.from(), message.token()];
        let outcome = f(RootFamily::ProcessMailbox, &mut words);
        match outcome {
            Ok(()) => {
                // `set_from`/`set_token` write the envelope words into the owned message first,
                // and `with_payload` (which consumes it) swaps in the payload last — the same
                // order `beam_process`'s own rebuild uses.
                message.set_from(words[1]);
                message.set_token(words[2]);
                process.mailbox_mut().enqueue(message.with_payload(words[0]));
            }
            Err(err) => {
                // Re-install this message exactly as it was drained (all-or-nothing per message)
                // plus every message behind it, so the queue is whole again before the error
                // leaves.
                process.mailbox_mut().enqueue(message);
                for rest in pending {
                    process.mailbox_mut().enqueue(rest);
                }
                return Err(err);
            }
        }
    }
    Ok(())
}
