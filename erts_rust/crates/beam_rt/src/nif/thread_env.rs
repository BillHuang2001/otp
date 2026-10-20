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

#![allow(dead_code)] // wired by the runtime startup in a follow-up task; see the module docs

//! The **process-independent host for NIF-created threads** — the [`NifHost`] behind
//! `enif_alloc_env`/`enif_make_atom`/`enif_send` on a thread a NIF created (contract **A.3**).
//!
//! # What it plugs into
//!
//! [`beam_abi`]'s thread-env seam ([`beam_abi::nif::thread_env`]) is the ABI half: `enif_alloc_env`
//! takes no argument, so it resolves a host in three steps — (1) the host of the NIF call the calling
//! thread is inside, (2) else the host the installed [`ThreadEnvHostFactory`] opens for that thread,
//! (3) else `NULL`. A thread a NIF created (`enif_thread_create`) is never inside a call, so step (2)
//! is the only one that can serve it. This module is the **host half**: [`ThreadEnvFactory`] is the
//! factory the runtime installs, and [`ForeignHost`] is what it opens, one per thread.
//!
//! The measured oracle behaviour this reproduces (`beam_bifs/HANDOFF_A3_NIF_HOST.md` §2, the C
//! fixture `a3_thread_env.c` against OTP 29.1.1): on an `enif_thread_create`d worker,
//! `enif_alloc_env` answers a non-`NULL` env, an atom built on it round-trips through
//! `enif_get_atom`, and `enif_send(NULL, &pid, msg_env, msg)` answers `1` and delivers — waking a
//! receiver parked in `receive`.
//!
//! # The atom index space is the hub's, never a private table
//!
//! `enif_make_atom` can **create** an atom, and a created atom must carry a real index in the
//! runtime's one live `AtomTable`: a private table would hand a NIF a **wrong term**, which is
//! forbidden ([`atoms`](super::atoms)' module docs carry the full argument). Every name therefore
//! goes through the crate's one index allocator, [`ForeignAtomHub`]: [`ForeignHost::intern_atom`]
//! **reserves** an index (the code server commits reservations at its next table touch) and
//! [`ForeignHost::atom_name`]/[`ForeignHost::existing_atom`] read the hub's mirror of the same
//! space.
//!
//! # The one deliberate substitution: a queued ETF image, not a synchronous delivery
//!
//! The C's `enif_send` from a thread with no calling process delivers **synchronously inside the
//! call**: `env == NULL` means `c_p == NULL`, so the sender is `am_undefined` and the message is
//! queued into the receiver's mailbox before `enif_send` returns (`erl_nif.c:1006`, `:876-877`).
//!
//! Here the message is **queued in [`ForeignSends`] at `enif_send` time** — which is when the hook
//! answers the C's `1` — and reaches the mailbox (and wakes a parked receiver) at the runtime's
//! **next pump**, which drains the queue. That is a substitution, not parity: the observable
//! ordering "the message is in the receiver's mailbox by the time the NIF thread's `enif_send`
//! returned" is not reproduced. It is the only shape available: the receiver's heap, mailbox and
//! process table are owned by the runtime thread, and this host runs on a thread that owns none of
//! them.
//!
//! # Why an ETF image and not the C's zero-copy heap splice
//!
//! The C hands the message to the receiver as a **heap fragment**
//! (`erts_create_message_from_nif_env`) — the terms stay where they were built and the fragment is
//! linked into the receiver's queue. That is sound in the C because every term was built on the
//! *runtime's* heaps and against the runtime's one atom table.
//!
//! A term built on a NIF-created thread is not: it lives on that thread's own heap and carries atom
//! **indices** from the hub's (predictive) mirror of the live index space. Handing those words to
//! the runtime verbatim would hand it a foreign heap (already unsound — the heap may be freed while
//! the runtime reads it) and, worse, an atom index whose name the live table may not yet have
//! committed. An **External Term Format image** is the one representation that crosses that boundary
//! by *name*: it is encoded against the hub's scratch table (a dense `AtomTable` whose `0..n` names
//! are exactly the hub's), and the runtime decodes it into its own heap against its own live table
//! (installing the same local-node identity, so a message carrying one of this node's pids/ports/
//! references still decodes to the internal id). The copy is the cost of crossing the thread
//! boundary safely, and it is why [`ForeignHost::send`] needs the message to be *encodable*.
//!
//! # Ownership, lifetime and threading
//!
//! * [`ThreadEnvFactory`] is `Send + Sync + 'static` (the ABI seam's requirement — it lives in a
//!   process-global `Mutex`, see the compile-time assertion at the bottom of this module). It holds
//!   only `Arc`-shared state: the hub, the send queue, the node **name** (resolved through the hub
//!   at encode time, never a snapshotted `Atom`) and the creation.
//! * [`ForeignHost`] is **not** `Send`: it owns a [`Heap`] (a raw `*mut Eterm` arena). Each thread
//!   that needs one opens its own, which is exactly what the factory's `open` is for.
//! * The ABI caches the opened host on its thread under the factory's **generation** stamp: an
//!   install or clear of the factory bumps the generation, so a host opened by a superseded factory
//!   is dropped on that thread's next use rather than reused. A NIF must therefore not keep an env
//!   (or a term built on it) across a factory reinstall: the env names its owning host, and that
//!   host is dropped with the generation it was opened under.
//! * [`ForeignHost::alloc_env`] builds the env over **this** host, so `enif_make_copy` is the
//!   identity and a term built on the env is a term of this host's one heap. Consequence: a term
//!   from *another* thread's [`ForeignHost`] heap is **not** valid here, so this host must never be
//!   handed one. [`NifHost::send`] is the only cross-thread route and it does not copy terms in —
//!   it encodes the message it is given.
//! * The heap is a fixed-capacity bump arena that is **never rewound** here. That is deliberate: a
//!   term a NIF still holds must not be overwritten, and this host cannot know when a term has
//!   stopped being used (`enif_send` copies it out immediately, but nothing forbids the NIF from
//!   reusing it). A thread that builds messages forever therefore exhausts its own heap and gets
//!   typed allocation failures from the `enif_make_*` family — never a corrupted term. Recycling is
//!   a follow-up concern, and it can only be done at a point where the C's `enif_free_env` lifetime
//!   rule (no term is used after its env is freed) is enforced.
//!
//! # Typed gaps (each one a typed `None`/`false`, never a wrong term)
//!
//! * **No calling process.** [`NifHost::self_pid`] is `None` (the C's `phy_proc` with
//!   `ERTS_INVALID_PID`), so `enif_self`/`enif_is_current_process_alive` answer their failure codes.
//!   A `from: None` therefore means "no sender at all" — the C's `am_undefined` — and *not* the
//!   calling process, which is what `None` means on a call host.
//! * **`atom_table` stays at the trait default `None`.** The env-less `enif_compare`/
//!   `enif_is_identical`/`enif_hash` therefore order a *nested* atom by its **raw index** instead of
//!   by name on this thread (`beam_term::compare`'s documented fallback). The hub's mirror could
//!   serve a name, but it is a *predicted* view (`AtomTable` is not behind a lock here) and this
//!   host must not hand `beam_term` a table the runtime may be appending to.
//! * **`select`, monitors, liveness, `whereis_pid`, `make_ref`, `unique_integer`, `system_info`,
//!   `term_to_binary`/`binary_to_term`, `port_command`, `priv_data`,
//!   `dynamic_resource_call`/`consume_timeslice`/`nif_schedule`** keep their trait defaults: every
//!   one of them needs the runtime's process table, poller, reference space or code server, which
//!   this host does not hold. (`term_to_binary`'s *encode* work is available through
//!   [`ForeignHost::send`]'s own image builder; the `enif_term_to_binary` hook stays unsupported so
//!   it never claims an image this host cannot vouch for.)
//! * **A `remote` destination.** [`NifHost::send`] resolves the destination with
//!   [`ProcessId::from_eterm`], which accepts only an **internal local** pid. A remote pid therefore
//!   answers the C's failure code `0` where the C would deliver over distribution and answer `1` —
//!   the one *wrong answer* in this module, recorded as a gap: delivering remotely needs a
//!   `beam_dist` route the host does not hold.
//! * **A message carrying a local (`make_fun3`) closure** cannot be encoded (its `NEW_FUN_EXT`
//!   identity lives in the loaded module, which this host cannot reach), so the send is the typed
//!   `None` — the ABI's `0` — rather than an image with an invented identity. The same holds for any
//!   other tag the codec refuses.
//! * **An atom the hub's mirror does not know** (one the runtime interned without a subsequent hub
//!   sync) is unnameable, so an image containing it is the typed `None` rather than a wrong name.

use core::ffi::c_int;
use std::sync::{Arc, Mutex};

use beam_abi::nif::thread_env::ThreadEnvHostFactory;
use beam_abi::nif::types::ErlNifEnv;
use beam_abi::{NifEnv, NifHost};
use beam_alloc::Heap;
use beam_codec::Encoder;
use beam_process::ProcessId;
use beam_term::prelude::{atom_val, is_atom, make_atom, Eterm};
use beam_term::Atom;

use super::atoms::ForeignAtomHub;

/// The message heap a [`ForeignHost`] opens: a fixed-capacity arena of term words.
///
/// Sized like a generous message env rather than like a process heap — a NIF-created worker builds
/// transient messages, and the arena is never rewound (see the module docs), so it is the thread's
/// lifetime budget for term construction. `1 << 20` words is 8 MiB.
const THREAD_ENV_HEAP_WORDS: usize = 1 << 20;

/// One `enif_send` a NIF-created thread issued.
///
/// Queued by [`ForeignHost::send`] and drained by the runtime at its next pump (the follow-up
/// wiring), which decodes `image` onto the receiver's heap against the runtime's **live** atom table
/// and enqueues it in the target's mailbox, waking a parked receiver.
pub(crate) struct ForeignSend {
    /// The receiver, resolved from the `ErlNifPid` the caller passed: an **internal local** pid.
    pub(crate) target: ProcessId,
    /// The sender, decoded from the ABI's `from` operand exactly like the target. `None` is the C's
    /// `undefined` sender — which is what a message env has (`c_p == NULL`), so this is the ordinary
    /// case for a NIF-created thread.
    pub(crate) from: Option<ProcessId>,
    /// The message as a **complete** External Term Format image: the leading `VERSION_MAGIC` (`131`)
    /// included, as `erlang:term_to_binary/1` writes it, encoded against the hub's index space with
    /// this node's local identity installed (so an internal pid/port/reference inside it has a wire
    /// image).
    pub(crate) image: Vec<u8>,
}

/// The shared queue a [`ForeignHost`] pushes [`ForeignSend`]s onto.
///
/// `Arc<Mutex<…>>` because the producers are the NIF's own threads (each with its own host) and the
/// consumer is the runtime thread (which owns no host). An `Arc` and not an `Rc`: the hosts are on
/// other threads.
pub(crate) type ForeignSends = Arc<Mutex<Vec<ForeignSend>>>;

/// The host one NIF-created thread builds terms and message envs on.
///
/// Opened by [`ThreadEnvHostFactory::open`] and cached by `beam_abi` on the thread that asked, so
/// there is one of these per thread that touches the seam. See the module docs for the ownership
/// rules, the substitution and the typed gaps.
pub(crate) struct ForeignHost {
    /// The one atom-index allocator: every name this host creates or reads goes through it, so the
    /// indices it hands a NIF are real live indices (see [`ForeignAtomHub`]).
    hub: Arc<ForeignAtomHub>,
    /// The queue an `enif_send` pushes onto — drained by the runtime, not by this thread.
    sends: ForeignSends,
    /// Every message env [`NifHost::alloc_env`] handed out and [`NifHost::free_env`] has not released
    /// — i.e. the envs this host **owns**. Field order is load-bearing: `envs` is declared **before**
    /// `heap` so the envs are dropped first, and an env's `Drop` (which withdraws its current-host
    /// token) never runs against an arena that is already gone.
    envs: Vec<Box<NifEnv>>,
    /// This host's own term arena: fixed capacity, never rewound (module docs).
    heap: Heap,
    /// The local node's **sysname**, kept as a name (shared with the factory, so opening a host costs
    /// no owned string) and resolved to an index through the hub at encode time — never a snapshotted
    /// `Atom`, which could be stale relative to the live table.
    node: Arc<str>,
    /// The node's live creation, written into every pid/port image.
    creation: u32,
}

impl ForeignHost {
    /// Encode `msg` as a complete ETF image against the hub's atom index space.
    ///
    /// Two steps, in this order:
    ///
    /// 1. **Reserve the node name first.** [`ForeignAtomHub::scratch_table`] is a cached image
    ///    stamped by the hub's length, so a reservation made *after* the image was taken would not be
    ///    in it and the encoder could not render the sysname atom. Reserving is idempotent (an
    ///    existing name keeps its index) and lands in the one index space.
    /// 2. **Encode** with [`Encoder::new`] (which writes `VERSION_MAGIC`, i.e. exactly
    ///    `term_to_binary/1`'s shape) and the local-node identity installed, so an internal
    ///    pid/port/reference is encodable and decodes back to the same internal id.
    ///
    /// `None` for any term the codec refuses (a local fun, an atom the hub cannot name, a malformed
    /// shape) — never a partial or invented image.
    fn encode_image(&self, msg: Eterm) -> Option<Vec<u8>> {
        let node = self.hub.reserve(self.node.as_ref());
        let table = self.hub.scratch_table();
        let mut encoder =
            Encoder::new(table.as_ref()).with_local_node(Atom::from_raw(node), self.creation);
        encoder.encode(msg).ok()?;
        Some(encoder.finish())
    }
}

impl NifHost for ForeignHost {
    /// This host's own arena — the heap a message env's terms are built on.
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// Intern `name`: **reserve** its index in the one live index space.
    ///
    /// Never `None`: a reservation always answers an index (an existing name keeps its own). The
    /// name becomes visible to the rest of the runtime when the code server next commits the hub's
    /// reservations ([`ForeignAtomHub::sync`], called at `atoms_mut`/`intern_pending` entry), and it is
    /// nameable here immediately ([`NifHost::atom_name`]).
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(Atom::from_raw(self.hub.reserve(name))))
    }

    /// The text of `atom`, or `None` for a non-atom term or an index the hub does not know.
    ///
    /// The hub's mirror covers the committed prefix **and** the reservation tail, which is what lets
    /// a thread read back its own just-created atom without touching (or locking) the live table.
    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.hub.name_of(atom_val(atom).raw())
    }

    /// The atom `name`, **without** creating it (C `enif_make_existing_atom`).
    ///
    /// `None` for a name neither the hub's mirror nor a standing reservation holds — never a
    /// fabricated index.
    fn existing_atom(&mut self, name: &str) -> Option<Eterm> {
        self.hub
            .lookup(name)
            .map(|index| make_atom(Atom::from_raw(index)))
    }

    /// No calling process: the C's `phy_proc` is `ERTS_INVALID_PID` on a message env, so `enif_self`
    /// answers its failure code rather than inventing a pid.
    ///
    /// This is also why a `from: None` handed to [`NifHost::send`] means *no sender at all* here,
    /// where on a call host it means the calling process.
    fn self_pid(&mut self) -> Option<Eterm> {
        None
    }

    /// A fresh message env over **this** host — one heap, one atom source.
    ///
    /// The env borrows this host for its lifetime (the ownership contract
    /// [`NifHost::alloc_env`] states) and is released by [`NifHost::free_env`]. Constructing it also
    /// publishes this host as the thread's *current* host, which is how `beam_abi`'s
    /// `enif_alloc_env(void)` reaches a host; for this seam that publication is observationally a
    /// no-op (the factory would have opened the same host) and its token is withdrawn when the env
    /// drops.
    ///
    /// `None` is never answered here: an env allocates no arena (it is pushed onto this host's
    /// `envs`), so the trait's "unsupported" answer would be a lie about a hook this host does serve.
    fn alloc_env(&mut self) -> Option<*mut ErlNifEnv> {
        let mut env = Box::new(NifEnv::new(self));
        let ptr = env.as_ptr();
        self.envs.push(env);
        Some(ptr)
    }

    /// Release an env [`NifHost::alloc_env`] handed out: `true` when `env` is one of this host's
    /// (which is then dropped), `false` for a null or unknown pointer — memory this host does not
    /// own is never freed.
    ///
    /// Freeing an env does **not** rewind the heap: terms built on it may still be reachable through
    /// whatever the NIF kept, and overwriting them would hand out a dangling term (module docs).
    fn free_env(&mut self, env: *mut ErlNifEnv) -> bool {
        if env.is_null() {
            return false;
        }
        let Some(index) = self
            .envs
            .iter_mut()
            .position(|owned| (**owned).as_ptr() == env)
        else {
            return false;
        };
        self.envs.swap_remove(index);
        true
    }

    /// `enif_make_copy(dst_env, src_term)` — the identity on this host.
    ///
    /// In the C the destination env's heap is a *different* heap, so the term is `size_object`d and
    /// `copy_struct`ed onto it. Here every env this host allocates is over **this same host**, so a
    /// term materialized on a message env already lives on the heap `enif_make_copy` would copy it to:
    /// the copy is exactly the identity, and it is sound for precisely the same one-heap reason
    /// [`NifHostImpl`](super::NifHostImpl) gives for the call host.
    ///
    /// The limit that reasoning implies: a term from **another** thread's host heap is not valid
    /// here. Nothing in this module can be handed one — [`NifHost::send`] is the only cross-thread
    /// route and it encodes the message it receives rather than copying a term in — but a caller that
    /// smuggled one in through a raw `ERL_NIF_TERM` would get a dangling word, which is the C's own
    /// rule ("a term belongs to the env it was built in") and why a NIF is never handed a heap it does
    /// not own.
    fn make_copy(&mut self, term: Eterm) -> Option<Eterm> {
        Some(term)
    }

    /// `enif_send(NULL, &pid, msg_env, msg)` on a thread with no calling process.
    ///
    /// The destination is resolved with [`ProcessId::from_eterm`], which reads the **term word only**
    /// (tag test + field decodes, no heap dereference), so no foreign heap is touched: a term naming
    /// no internal local pid is the C's `0` (`Some(false)` — `erts_proc_lookup` found no process). A
    /// **remote** pid is the same `false` here where the C would deliver over distribution — the
    /// documented wrong-answer gap in the module docs.
    ///
    /// `from` is decoded the same way; `None` (the C's `am_undefined` sender) and an undecodable
    /// value both become `None` in [`ForeignSend`].
    ///
    /// The message is then encoded as an ETF image (`encode_image`) — the only sound
    /// way to move a term built on this thread's heap into the runtime's. An unencodable message is
    /// the typed `None`, which the ABI renders as `0`: no image is ever fabricated, and neither is a
    /// `1` for a message that could not be delivered.
    ///
    /// On success the send is pushed onto [`ForeignSends`] and the answer is `Some(true)` — the C's
    /// `1`, given at `enif_send` time, with the delivery itself happening at the runtime's next pump
    /// (the module docs' one deliberate substitution).
    fn send(&mut self, to: Eterm, from: Option<Eterm>, msg: Eterm) -> Option<bool> {
        let Ok(target) = ProcessId::from_eterm(to) else {
            // The C's `0`: no process to look up. `false` and not `None`, because the relation the
            // C asks about does not need a runtime here — an internal-local test is enough to know
            // this destination names no process of this runtime.
            return Some(false);
        };
        let from = from.and_then(|term| ProcessId::from_eterm(term).ok());
        let image = self.encode_image(msg)?;
        let mut queue = match self.sends.lock() {
            Ok(queue) => queue,
            // A poisoned queue is a **failed** send (a drainer panicked while holding it), never a
            // fabricated `1`; the shape mirrors `install_thread_env_factory`'s poisoned-lock answer,
            // and the queue stays refused until the runtime installs a fresh `Arc`.
            Err(_) => return None,
        };
        queue.push(ForeignSend {
            target,
            from,
            image,
        });
        Some(true)
    }

    /// A documented no-op: a NIF-created thread is **not** a scheduler thread.
    ///
    /// The hook exists so a host can pre-empt a self-rescheduling `enif_schedule_nif` chain on a
    /// scheduler thread; this thread runs no process, holds no reduction budget and cannot be
    /// pre-empted, so there is nothing to yield to. (`enif_schedule_nif` itself is a typed gap here
    /// anyway.)
    fn nif_yield_point(&mut self, flags: c_int) {
        let _ = flags;
    }

    // Deliberately **not** overridden here, each for the reason the module docs list:
    // `atom_table` (the nested-atom name order; see the gap list), `select`, `monitor_process`/
    // `demonitor_process`, `is_process_alive`/`is_port_alive`/`is_current_process_alive`,
    // `whereis_pid`, `make_ref`, `unique_integer`, `system_info`, `term_to_binary`/
    // `binary_to_term`/`binary_to_term_used`, `port_command`, `priv_data`, `consume_timeslice`,
    // `dynamic_resource_call` and the resource family — every one of them needs the runtime's process
    // table, poller, reference space, code server or resource registry, none of which this host holds.
}

/// The factory the runtime installs: the ABI seam's source of [`ForeignHost`]s.
///
/// Cheap to open from (it clones two `Arc`s and allocates one [`Heap`]) because
/// [`ThreadEnvHostFactory::open`] is called **with the ABI factory lock held**: it must not block and
/// must never re-enter `install_thread_env_factory`/`clear_thread_env_factory`. It does neither — it
/// touches no lock of its own.
///
/// The **node name** is kept here (as a shared `Arc<str>`) rather than resolved to an `Atom` once:
/// an atom index is only meaningful against the table that handed it out, and a foreign thread
/// encodes against the hub's mirror, so the name is resolved through the hub at encode time.
pub(crate) struct ThreadEnvFactory {
    /// The one atom-index allocator, shared with every host this factory opens.
    hub: Arc<ForeignAtomHub>,
    /// The queue those hosts push sends onto, shared with the runtime that drains it.
    sends: ForeignSends,
    /// The local node's sysname, shared (not cloned per open) with every host.
    node: Arc<str>,
    /// The node's live creation.
    creation: u32,
}

impl ThreadEnvFactory {
    /// Build the factory the runtime installs.
    ///
    /// `hub` is the runtime's one atom-index hub ([`ForeignAtomHub::new`] over the live table),
    /// `sends` the queue the runtime drains at its next pump, and `node`/`creation` this node's
    /// identity as [`beam_codec::Encoder::with_local_node`] wants it — the name here, the creation
    /// verbatim.
    pub(crate) fn new(
        hub: Arc<ForeignAtomHub>,
        sends: ForeignSends,
        node: String,
        creation: u32,
    ) -> ThreadEnvFactory {
        ThreadEnvFactory {
            hub,
            sends,
            node: Arc::from(node),
            creation,
        }
    }

    /// The queue this factory's hosts push [`ForeignSend`]s onto — for a runtime that installed the
    /// factory and no longer holds its own handle.
    pub(crate) fn sends(&self) -> ForeignSends {
        Arc::clone(&self.sends)
    }
}

impl ThreadEnvHostFactory for ThreadEnvFactory {
    /// Open a host for the calling thread: one [`Heap`] and two `Arc` clones, or `None` when the heap
    /// cannot be allocated (the ABI then reports the seam's honest "no host reachable").
    ///
    /// Called with the ABI factory lock held — no lock is taken here.
    fn open(&self) -> Option<Box<dyn NifHost>> {
        let heap = Heap::new(THREAD_ENV_HEAP_WORDS).ok()?;
        Some(Box::new(ForeignHost {
            hub: Arc::clone(&self.hub),
            sends: Arc::clone(&self.sends),
            envs: Vec::new(),
            heap,
            node: Arc::clone(&self.node),
            creation: self.creation,
        }))
    }
}

/// The seam's `Send + Sync` requirement is a **compile-time** property, not an implementation detail:
/// the factory lives in a process-global `Mutex` shared by every thread of the process, and the queue
/// is written by the NIF's threads and read by the runtime's. A [`ForeignHost`] itself is deliberately
/// **not** `Send` (it owns a `Heap`), which is why sharing goes through the factory's `Arc`s and each
/// thread opens its own host.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ThreadEnvFactory>();
    assert_send_sync::<ForeignSends>();
    assert_send_sync::<ForeignSend>();
    assert_send_sync::<Arc<ForeignAtomHub>>();
};
