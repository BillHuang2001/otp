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

//! Unit tests for the **foreign-thread host** ([`super`]) — the [`NifHost`] a thread a NIF created
//! (`enif_thread_create`) builds message envs and sends on (contract **A.3**) — plus the three
//! runtime seams that make it observable: [`Runtime::install_nif_thread_env`],
//! [`Runtime::clear_nif_thread_env`] and [`Runtime::drain_nif_thread_sends`].
//!
//! No C toolchain and no oracle is involved: the host, the atom hub and the queue are driven
//! directly, and the two deliveries are observed on the runtime's **own** processes.
//!
//! # What each test pins
//!
//! | test | the property |
//! |---|---|
//! | [`the_factory_host_hands_out_the_private_spaces_atom_indices`] | the `enif_make_atom` hook answers exactly the index [`ForeignAtomHub::reserve`] reserved **in the private space**, `enif_get_atom`/`enif_make_existing_atom` read that same space, a name the space already holds is **not** duplicated and an unknown one is never fabricated |
//! | [`an_atom_a_factory_thread_creates_reaches_the_live_table_by_name_not_by_index`] | a real `std::thread` gets a message env from the installed factory, interns a **new** atom (private index), and the name reaches the **live** table only when the decoded image carries it — at the live table's own index, never the private one |
//! | [`a_send_queues_an_image_that_decodes_to_the_exact_message`] | `enif_send` answers the C's `1`, the queue holds one [`ForeignSend`] with the expected target and **no** sender, and its image (`131`-led) decodes to a term structurally equal to the message |
//! | [`a_drained_foreign_send_wakes_a_parked_receiver`] | the scheduler pass that drains delivers into the mailbox of a receiver parked in a compiled `receive` **and wakes it** — its computed result is the message |
//! | [`a_send_to_a_term_that_names_no_process_answers_false`] | the C's `0` for a destination naming no pid, with nothing queued (a pid term for a process nobody owns is the drain's liveness question, below) |
//! | [`an_unencodable_message_answers_none`] | a message the codec refuses is the typed `None` (never an invented image) |
//! | [`undecodable_images_and_a_dead_target_are_skipped_and_never_reach_a_mailbox`] | the drain consumes and refuses garbage/truncated images and a dead target: nothing is delivered, nothing reaches a mailbox |
//! | [`install_is_idempotent_and_clear_really_clears`] | install answers `true` twice (last-install-wins, a **fresh** queue), and a clear really withdraws the seam from a fresh thread and from the runtime's drain |
//! | [`a_runtime_that_never_installed_answers_zero_from_the_drain`] | the `None` fast path: no factory ⇒ the drain is inert (one `Option` test, no lock) |
//!
//! # Why some tests take a lock
//!
//! The host factory is **process-global** (`beam_abi`'s `Mutex<Option<Box<dyn
//! ThreadEnvHostFactory>>>`, last-install-wins) while the crate's lib tests run in parallel inside
//! one process, so every test that installs or clears it holds
//! [`Runtime::lock_nif_thread_env`] for the whole install → exercise → clear window
//! ([`EnvGuard`]) and clears the slot on the way out **even when an assertion panics**. A test that
//! needs no factory (a locally built one, or the `None` fast path) takes no lock at all.
//!
//! # What a unit test cannot reach, and what is asserted instead
//!
//! * `enif_alloc_env`/`enif_make_atom` are `unsafe` (`beam_rt` is `#![deny(unsafe_code)]`), so the
//!   tests call the **safe** hooks the `enif_*` bodies drive — [`NifHost::alloc_env`] and
//!   [`NifHost::intern_atom`] — and, for "a thread outside a call reaches no host", the ABI's own
//!   safe accessor [`beam_abi::nif::thread_env::with_thread_env_host`], which is exactly the step
//!   `enif_alloc_env` takes before it would answer `NULL`.
//! * A **local (`make_fun3`) closure** cannot be built here (`beam_term::fun::write_fun_local` is
//!   `unsafe`), so the "the codec refuses this message" leg uses the other documented refusal: an
//!   atom the private space cannot name.
//!
//! Everything else is real. In particular the drain's **skip** arm is driven through the runtime's
//! own queue: `Runtime::nif_thread_sends` is a `#[cfg(test)]` read accessor for the very `Arc` the
//! installed hosts write to, so this suite can feed it a raw image (the drain is the only party that
//! can refuse one) instead of re-implementing the decode check the drain performs.

use super::*;

use std::sync::MutexGuard;

use beam_abi::nif::thread_env::with_thread_env_host;
use beam_process::ProcessState;
use beam_term::prelude::make_small;
use beam_term::AtomTable;

use crate::runtime::Runtime;

/// The node name every factory in this module encodes against: no runtime here is a node.
const LOCAL_NODE: &str = "nonode@nohost";

/// The creation those factories write into a pid/port/reference image.
const CREATION: u32 = 7;

/// The committed receive fixture (`../beam_engine/tests/fixtures/recv.beam`, reused by relative path
/// — never copied): `recv:wait/0` is `receive X -> X end`, so a message delivered to it becomes the
/// process's result — the observable proof that the drain woke it.
const RECV_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_engine/tests/fixtures/recv.beam"
);

// ---------------------------------------------------------------------------------------------
// The lock every installing test holds, and the teardown it guarantees
// ---------------------------------------------------------------------------------------------

/// Holds the crate-wide install lock for one test and clears the process-global factory when the
/// test ends — including when an assertion panics, which is what keeps a failure from leaking an
/// installed factory (and its hosts) into the next test in the same process.
///
/// The guard starts from "no factory installed" (the acquire clears the slot) and the lock guard is
/// a field, so the slot is cleared **before** the lock is released.
struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    /// Serialise on the crate-wide lock and start from a clean slot.
    fn acquire() -> EnvGuard {
        let lock = Runtime::lock_nif_thread_env();
        beam_abi::nif::clear_thread_env_factory();
        EnvGuard { _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        beam_abi::nif::clear_thread_env_factory();
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers: a queue, a factory-host triple, a message builder and the drain's own decoder
// ---------------------------------------------------------------------------------------------

/// An empty [`ForeignSends`] queue of this test's own — the writer side of a **locally built**
/// factory, so a test can see exactly what a host pushed without touching the global slot.
fn queue() -> ForeignSends {
    Arc::new(Mutex::new(Vec::new()))
}

/// A **private** atom space seeded with `names`, a factory over it (this test's own, never installed)
/// and the host that factory opens — the same shape `beam_abi` gives a NIF-created thread.
///
/// Seeding goes through [`ForeignAtomHub::reserve`], so the space's indices are its own: there is no
/// live table here, and no live index is involved.
fn factory_host(names: &[&str]) -> (Arc<ForeignAtomHub>, ForeignSends, Box<dyn NifHost>) {
    let hub = ForeignAtomHub::new();
    for name in names {
        hub.reserve(name);
    }
    let sends = queue();
    let factory = ThreadEnvFactory::new(
        Arc::clone(&hub),
        Arc::clone(&sends),
        LOCAL_NODE.to_string(),
        CREATION,
    );
    let host = factory
        .open()
        .expect("a thread-env heap is allocatable, so the factory opens a host");
    (hub, sends, host)
}

/// `{Tag, Int, Bin}` built on `host`'s **own** arena — the message the send tests use.
fn tuple_message(host: &mut dyn NifHost, tag: &str, int: i64, bytes: &[u8]) -> Eterm {
    let atom = host.intern_atom(tag).expect("the host interns a name");
    let heap = host.heap();
    let bin = beam_codec::term::make_binary(heap, bytes).expect("the binary fits this heap");
    let number = make_small(int);
    heap.tuple(&[atom, number, bin])
        .expect("the tuple fits this heap")
}

/// Decode `image` the way [`Runtime::drain_nif_thread_sends`] does — onto a fresh scratch heap,
/// against `atoms`, with this node's identity installed — and hand the heap back so the decoded
/// term stays alive (`beam_alloc`'s carrier does not move, but the term points into it).
fn decode(image: &[u8], atoms: &mut AtomTable) -> Option<(Heap, Eterm)> {
    let node = atoms.intern(LOCAL_NODE);
    let mut heap = Heap::new(4096).expect("the decode heap");
    let term =
        beam_codec::decode_term_with_local_node(image, &mut heap, atoms, node, CREATION, None)
            .ok()?;
    Some((heap, term))
}

// ---------------------------------------------------------------------------------------------
// 1. The host's atom surface is the hub's one index space
// ---------------------------------------------------------------------------------------------

/// The `enif_make_atom`/`enif_get_atom`/`enif_make_existing_atom` hooks all answer out of the
/// **factory's private** atom space: a created atom takes exactly the index
/// [`ForeignAtomHub::reserve`] reserved for it, its name round-trips, a name the space already holds
/// keeps its index (so it is never duplicated), and a name nobody created answers `None` rather than a
/// fabricated index.
#[test]
fn the_factory_host_hands_out_the_private_spaces_atom_indices() {
    let hub = ForeignAtomHub::new();
    hub.reserve("alpha");
    hub.reserve("beta");
    let factory =
        ThreadEnvFactory::new(Arc::clone(&hub), queue(), LOCAL_NODE.to_string(), CREATION);
    let mut host = factory.open().expect("the factory opens a host");

    // A name the space already holds is read, never duplicated.
    assert_eq!(
        host.existing_atom("alpha").map(|atom| atom_val(atom).raw()),
        Some(0),
        "a name the space holds resolves through it"
    );
    assert_eq!(
        host.intern_atom("alpha").map(|atom| atom_val(atom).raw()),
        Some(0),
        "interning a known name keeps its index"
    );
    assert_eq!(
        hub.reserve("beta"),
        1,
        "neither lookup appended a name to the space"
    );

    // A NEW name takes exactly the index `reserve` gives it — a **private** index.
    let reserved = hub.reserve("novel_name");
    assert_eq!(reserved, 2, "the space's own next index");
    let created = host.intern_atom("novel_name").expect("the host interns");
    assert_eq!(
        atom_val(created).raw(),
        reserved,
        "the atom a foreign thread builds carries the private index"
    );
    assert_eq!(
        host.atom_name(created).as_deref(),
        Some("novel_name"),
        "the text round-trips through the same space"
    );

    // `enif_make_existing_atom` never fabricates an index — in the host or in the space …
    assert_eq!(host.existing_atom("never_seen"), None);
    assert_eq!(hub.lookup("never_seen"), None);
    assert_eq!(
        host.atom_name(created).as_deref(),
        Some("novel_name"),
        "a refused lookup left the space untouched"
    );
    // … until something really creates it.
    let seen = host.intern_atom("never_seen").expect("the host interns");
    assert_eq!(host.existing_atom("never_seen"), Some(seen));
    assert_eq!(host.atom_name(seen).as_deref(), Some("never_seen"));
}

// ---------------------------------------------------------------------------------------------
// 2. A factory-thread atom reaches the LIVE table **by name**, never at its private index
// ---------------------------------------------------------------------------------------------

/// The handoff is **by name**: a NIF-created thread creates an atom in the factory's private space,
/// and that name reaches the runtime's live `AtomTable` only when a decoded image carries it — never
/// because the private index is a live index. The private index is therefore *not* the live one.
#[test]
fn an_atom_a_factory_thread_creates_reaches_the_live_table_by_name_not_by_index() {
    let _env = EnvGuard::acquire();
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    assert!(
        runtime.install_nif_thread_env(),
        "the factory installs (over its own private atom space)"
    );
    let base_len = runtime.atoms().len();
    runtime.load_module(RECV_BEAM).expect("recv.beam loads");
    let receiver = runtime
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");
    assert_eq!(runtime.run_one_slice().expect("one slice"), 1);

    // The worker answers only the **private** index it interned the name at: there is no live index
    // to hand back, because the space it built the atom in is not the runtime's table.
    let private_index = std::thread::spawn(move || {
        with_thread_env_host(|host| {
            // The message env `enif_alloc_env(void)` asks this seam for — step (2) of its
            // reachability order, the only one a thread outside a call can satisfy.
            let env = host.alloc_env().expect("a message env");
            assert!(
                !env.is_null(),
                "the factory host serves an env off the call path"
            );
            // `enif_make_atom(msg_env, "thread_made_atom")`: a name that exists nowhere else.
            let atom = host
                .intern_atom("thread_made_atom")
                .expect("the host interns");
            let index = atom_val(atom).raw();
            // The one crossing: an `enif_send`, whose image spells every atom by NAME.
            assert_eq!(
                host.send(receiver.to_eterm(), None, atom),
                Some(true),
                "the send is accepted (the C's `1`) and queues an image"
            );
            assert!(
                host.free_env(env),
                "the env came from this host, so it is handed back to it"
            );
            index
        })
        .expect("a factory is installed, so a host is reachable")
    })
    .join()
    .expect("the worker thread does not panic");

    // The private index is small and says nothing about the live table: the atom is not there yet.
    assert!(
        private_index < 8,
        "the private space is its own — {private_index} is not a live index"
    );
    assert_eq!(
        runtime.atoms().lookup("thread_made_atom"),
        None,
        "nothing has carried the name into the live table yet"
    );

    // The drain decodes the image against the live table, which interns the name — and the delivery
    // lands, so the message really carried that atom across the thread boundary.
    assert_eq!(
        runtime.drain_nif_thread_sends(None),
        1,
        "the image decodes and delivers"
    );
    let live_index = runtime
        .atoms()
        .lookup("thread_made_atom")
        .expect("the decoder interned the name into the live table");
    assert!(
        live_index.raw() as usize >= base_len,
        "it landed at the live table's own new index ({live_index:?} < {base_len})"
    );
    assert_ne!(
        live_index,
        Atom::from_raw(private_index),
        "and that is NOT the private index the foreign thread used"
    );
}// ---------------------------------------------------------------------------------------------
// 3. `enif_send` queues a complete ETF image of the exact message
// ---------------------------------------------------------------------------------------------

/// `ForeignHost::send` is the C's `enif_send(NULL, &pid, msg_env, msg)`: it answers `1`
/// (`Some(true)`) at call time and queues **one** [`ForeignSend`] for the runtime's next pump. The
/// queued image is a complete External Term Format image (leading `131`) of the message — `{atom,
/// int, binary}` here — and decoding it the way the drain does reproduces the message exactly, with
/// the atom travelling **by name** (it is interned into the decoding table).
#[test]
fn a_send_queues_an_image_that_decodes_to_the_exact_message() {
    let (hub, sends, mut host) = factory_host(&["base_atom"]);
    let target = ProcessId::new(11, 0);
    let message = tuple_message(&mut *host, "thread_msg", 42, b"abc");

    assert_eq!(
        host.send(target.to_eterm(), None, message),
        Some(true),
        "the C's `1`: accepted at enif_send time, delivered at the runtime's next pump"
    );

    let queued = sends.lock().expect("this test owns the queue");
    assert_eq!(queued.len(), 1, "exactly one send is queued");
    let send = &queued[0];
    assert_eq!(
        send.target, target,
        "the destination is the pid it was given"
    );
    assert!(
        send.from.is_none(),
        "a message env has no calling process: the C's am_undefined sender"
    );
    assert_eq!(
        send.image.first(),
        Some(&131),
        "a complete ETF image starts with VERSION_MAGIC"
    );
    let mut atoms = AtomTable::new();
    let (_heap, decoded) = decode(&send.image, &mut atoms).expect("the image decodes");
    assert!(
        atoms.lookup("thread_msg").is_some(),
        "the atom crosses the thread boundary by NAME, so the decoder interns it"
    );
    assert!(
        beam_term::compare::eq(decoded, message, &atoms),
        "the decoded term is structurally the message that was sent"
    );
    assert_eq!(
        beam_term::size::size_object(decoded),
        beam_term::size::size_object(message),
        "and it has the message's own shape (tuple, atom, int, binary)"
    );
    assert!(
        hub.lookup(LOCAL_NODE).is_some(),
        "the encoder reserved this node's sysname in the private space"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. The pass that drains delivers AND wakes a parked receiver
// ---------------------------------------------------------------------------------------------

/// The end-to-end substitution: a receiver parked in a compiled `receive`, a real `std::thread`
/// sending through the installed factory's host, and the scheduler pass's drain. The delivery lands
/// in the mailbox **and** wakes the receiver — `recv:wait/0` returns the message, printed here so
/// its atom, integer and binary are all checked.
#[test]
fn a_drained_foreign_send_wakes_a_parked_receiver() {
    let _env = EnvGuard::acquire();
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    assert!(runtime.install_nif_thread_env(), "the factory installs");
    runtime.load_module(RECV_BEAM).expect("recv.beam loads");
    let receiver = runtime
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");

    // 1. The receiver really is parked: one slice took it to `wait` with an empty mailbox.
    assert_eq!(runtime.run_one_slice().expect("one slice"), 1);
    assert_eq!(
        runtime
            .processes()
            .get(receiver)
            .expect("the receiver is alive")
            .state(),
        ProcessState::Waiting,
        "recv:wait/0 is blocked in its receive"
    );
    assert!(
        runtime.process_result(receiver).is_none(),
        "a parked process has not returned"
    );

    // 2. The send a NIF-created thread issues: its own thread, its own factory host, no sender.
    let target = receiver.to_eterm();
    let sent = std::thread::spawn(move || {
        with_thread_env_host(|host| {
            let env = host.alloc_env().expect("a message env");
            let message = tuple_message(host, "thread_msg", 42, b"abc");
            let code = host.send(target, None, message);
            let freed = host.free_env(env);
            (code, freed)
        })
        .expect("the installed factory serves this thread")
    })
    .join()
    .expect("the worker thread does not panic");
    assert_eq!(sent, (Some(true), true), "the C's `1`, env released");

    // 3. Queued, not delivered: the drain is what moves it, and nothing has reached a mailbox yet.
    let queue = runtime
        .nif_thread_sends()
        .expect("an installed runtime holds its factory's queue")
        .clone();
    assert_eq!(queue.lock().expect("this test owns the queue").len(), 1);
    assert!(
        runtime
            .processes()
            .get(receiver)
            .expect("the receiver is alive")
            .mailbox()
            .iter()
            .count()
            == 0,
        "the send is only queued at enif_send time"
    );
    assert!(
        runtime.process_result(receiver).is_none(),
        "the receiver is still parked before the pass"
    );

    // 4. The scheduler pass's NIF-thread step drains, delivers into the mailbox and resumes the
    //    receiver through the runtime's own resume-on-send wake list.
    assert!(
        runtime
            .run_scheduler()
            .expect("the pass runs the woken receiver")
            >= 1,
        "the drain turned into at least one slice"
    );
    assert!(
        queue.lock().expect("this test owns the queue").is_empty(),
        "a drained queue is empty"
    );
    let value = runtime
        .process_result(receiver)
        .expect("the woken receiver returned the message");
    assert_eq!(
        runtime
            .format_term(value, None)
            .expect("the delivered message prints")
            .trim(),
        "{thread_msg,42,<<\"abc\">>}",
        "the message that woke it is exactly what the foreign thread sent"
    );
}

// ---------------------------------------------------------------------------------------------
// 5. Refusals are typed, never fabricated
// ---------------------------------------------------------------------------------------------

/// A destination that is **not an internal local pid** is the C's `0` (`Some(false)`) and queues
/// nothing: the relation the C asks about needs no runtime, the internal-local test is enough.
///
/// A well-formed pid term for a process nobody owns is a different shape here, and the difference is
/// the module's documented substitution rather than a fabrication: this host resolves the term word
/// only (no heap dereference) and answers the C's `1` at `enif_send` time, while the **liveness**
/// answer is the drain's — where such a message is a counted skip and never reaches a mailbox (pinned
/// by [`undecodable_images_and_a_dead_target_are_skipped_and_never_reach_a_mailbox`]).
#[test]
fn a_send_to_a_term_that_names_no_process_answers_false() {
    let (_hub, sends, mut host) = factory_host(&["base_atom"]);
    let message = tuple_message(&mut *host, "thread_msg", 1, b"x");

    assert_eq!(
        host.send(make_small(7), None, message),
        Some(false),
        "a term that names no pid is the C's `0`, not a fabricated `1`"
    );
    assert!(
        sends.lock().expect("this test owns the queue").is_empty(),
        "a refused send queues nothing"
    );

    // A pid term for a process this RUNTIME does not have is accepted here (the term resolves);
    // the dead-target refusal is the drain's, and it is counted there.
    let dead = ProcessId::new(424_242, 0);
    assert_eq!(
        host.send(dead.to_eterm(), None, message),
        Some(true),
        "the host resolves the destination term; liveness is the drain's question"
    );
    assert_eq!(
        sends.lock().expect("this test owns the queue").len(),
        1,
        "so the message is queued for the pump that can answer it"
    );
}

/// A message the codec refuses is the typed `None` (the ABI's `0`), never an image with an invented
/// identity: nothing is queued. The documented refusal reachable from this crate is an atom the
/// private space cannot name (`beam_term::fun::write_fun_local`'s local closure is `unsafe`).
#[test]
fn an_unencodable_message_answers_none() {
    let (hub, sends, mut host) = factory_host(&["base_atom"]);
    let ghost = make_atom(Atom::from_raw(hub.scratch_table().len() as u32 + 1_000));
    assert_eq!(
        hub.name_of(atom_val(ghost).raw()),
        None,
        "the hub really cannot name the atom the message carries"
    );

    assert_eq!(
        host.send(ProcessId::new(1, 0).to_eterm(), None, ghost),
        None,
        "an unencodable message is the typed refusal, never a partial image"
    );
    assert!(
        sends.lock().expect("this test owns the queue").is_empty(),
        "and it queues nothing"
    );
}

/// The drain's refusal half: an image that cannot decode and a destination that is gone are both
/// **skipped and counted** — the return value counts only the messages that really landed, so it is
/// `0`, the queue is consumed, and nothing reaches a mailbox. Both items are fed to the runtime's
/// own queue (the suite's handle on it), so this is the drain's real skip arm, not a re-implemented
/// decoder check.
#[test]
fn undecodable_images_and_a_dead_target_are_skipped_and_never_reach_a_mailbox() {
    let _env = EnvGuard::acquire();
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    assert!(runtime.install_nif_thread_env(), "the factory installs");
    runtime.load_module(RECV_BEAM).expect("recv.beam loads");
    let receiver = runtime
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");
    assert_eq!(runtime.run_one_slice().expect("one slice"), 1);

    // One genuine image, so the truncated copy below is a real image cut short.
    let (_hub, local, mut host) = factory_host(&["base_atom"]);
    let message = tuple_message(&mut *host, "thread_msg", 7, b"payload");
    assert_eq!(
        host.send(receiver.to_eterm(), None, message),
        Some(true),
        "a real image to queue a truncated twin of"
    );
    let image = {
        let mut queue = local.lock().expect("this test owns the queue");
        std::mem::take(&mut *queue)
            .pop()
            .expect("the one send the host queued")
            .image
    };
    assert!(image.len() > 2, "the image has a term to truncate");

    let runtime_queue = runtime
        .nif_thread_sends()
        .expect("an installed runtime holds its factory's queue")
        .clone();
    {
        let mut queue = runtime_queue.lock().expect("this test owns the queue");
        queue.push(ForeignSend {
            target: receiver,
            from: None,
            // A `131` with no term behind it: nothing decodes.
            image: vec![131],
        });
        queue.push(ForeignSend {
            target: receiver,
            from: None,
            image: image[..image.len() - 2].to_vec(),
        });
        queue.push(ForeignSend {
            target: ProcessId::new(999_999, 0),
            from: None,
            // A perfectly good image… for a process this runtime does not have.
            image,
        });
    }

    assert_eq!(
        runtime.drain_nif_thread_sends(None),
        0,
        "the count is what really landed: every one of the three was refused"
    );
    assert!(
        runtime_queue
            .lock()
            .expect("this test owns the queue")
            .is_empty(),
        "the drain consumes what it refuses instead of retrying it forever"
    );
    assert_eq!(
        runtime
            .processes()
            .get(receiver)
            .expect("the receiver is alive")
            .mailbox()
            .iter()
            .count(),
        0,
        "no term reached the mailbox"
    );
    assert!(
        runtime.process_result(receiver).is_none(),
        "and the receiver is still parked"
    );
}

// ---------------------------------------------------------------------------------------------
// 6. Install / clear
// ---------------------------------------------------------------------------------------------

/// Installation is **last-install-wins** and a second install is a success, not a refusal — and it
/// hands this runtime a **fresh** queue. A clear is a real teardown: the runtime's handle is gone
/// (so the drain answers `0` for a message a pre-clear host queued, which stays in that queue), and
/// a fresh thread outside a NIF call is back to the seam's honest "no host".
#[test]
fn install_is_idempotent_and_clear_really_clears() {
    let _env = EnvGuard::acquire();
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    assert!(runtime.install_nif_thread_env(), "the first install");
    let first = runtime.nif_thread_sends().expect("installed").clone();
    first.lock().expect("this test owns the queue").clear();

    assert!(
        runtime.install_nif_thread_env(),
        "a second install replaces the factory; it is not a refusal"
    );
    let second = runtime.nif_thread_sends().expect("installed").clone();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "a re-install hands this runtime a fresh queue"
    );

    // A host of the *installed* factory queues a message for a receiver that really exists …
    runtime.load_module(RECV_BEAM).expect("recv.beam loads");
    let receiver = runtime
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");
    assert_eq!(runtime.run_one_slice().expect("one slice"), 1);
    let queued = with_thread_env_host(|host| {
        let message = tuple_message(host, "queued_before_clear", 1, b"x");
        host.send(receiver.to_eterm(), None, message)
    })
    .expect("the installed factory serves this thread");
    assert_eq!(queued, Some(true), "accepted, as the C's `1`");
    assert_eq!(
        second.lock().expect("this test owns the queue").len(),
        1,
        "the installed factory's queue is the runtime's"
    );

    // … and the clear withdraws the seam: no queue for the runtime, no host for a fresh thread.
    runtime.clear_nif_thread_env();
    assert!(
        runtime.nif_thread_sends().is_none(),
        "clear drops the runtime's queue handle"
    );
    assert_eq!(
        runtime.drain_nif_thread_sends(None),
        0,
        "so the drain delivers nothing, even though the message is still queued"
    );
    assert_eq!(
        second.lock().expect("this test owns the queue").len(),
        1,
        "the message was not consumed — only the runtime's handle to it went"
    );
    assert_eq!(
        runtime
            .processes()
            .get(receiver)
            .expect("the receiver is alive")
            .mailbox()
            .iter()
            .count(),
        0,
        "the pre-clear message never reached a mailbox"
    );

    // `enif_alloc_env` is `unsafe`, so the assertion a `#![deny(unsafe_code)]` crate can make is the
    // ABI's safe accessor — the very step that decides between a host and the C's `NULL`.
    let reached = std::thread::spawn(|| with_thread_env_host(|_host| ()).is_some())
        .join()
        .expect("the fresh thread does not panic");
    assert!(
        !reached,
        "after clear, a thread outside a NIF call reaches no host — the seam's honest NULL"
    );
}

// ---------------------------------------------------------------------------------------------
// 7. The `None` fast path is inert
// ---------------------------------------------------------------------------------------------

/// A runtime that never installed a factory holds no queue at all, so its drain is one `Option`
/// test: no lock, no allocation, nothing observable. This test takes **no** lock, because the
/// property is a field of this runtime and is independent of the process-global slot.
#[test]
fn a_runtime_that_never_installed_answers_zero_from_the_drain() {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    assert!(
        runtime.nif_thread_sends().is_none(),
        "no factory was ever installed for this runtime"
    );
    assert_eq!(
        runtime.drain_nif_thread_sends(None),
        0,
        "the fast path answers 0 without taking a lock"
    );
}
