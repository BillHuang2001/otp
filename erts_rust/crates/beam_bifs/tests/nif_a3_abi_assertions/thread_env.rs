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

//! §2 — `enif_alloc_env` **off the NIF-call path**, and `enif_send(NULL, &pid, msg_env, msg)`.
//!
//! The oracle transcript behind this module is `../HANDOFF_A3_NIF_HOST.md` §2 and its replay is
//! `../nif_a3_oracle_replay.rs`: an `enif_thread_create`d worker gets a real message env
//! (`{{0,1,0},0}` for `probe_thread_send`), atoms round-trip through it (`probe_worker_atoms` →
//! `{0,1,1,1,0}`) and the message reaches a parked receiver. The C can do that because
//! `enif_alloc_env` touches **no** thread state: the env's heap and the one VM-global atom table are
//! the runtime's.
//!
//! Both halves are asserted here, because both are the contract:
//!
//! * **no factory** ⇒ `enif_alloc_env()` is `NULL` and nothing is reachable. That is not a regression
//!   but the seam's honest answer: a crate-owned env would carry a *private* atom table, so
//!   `enif_make_atom(msg_env, "ok")` would hand a NIF an atom with a wrong index — a wrong term
//!   delivered to another process. `beam_abi`'s `nif::thread_env` module docs state the same.
//! * **a factory** (`ThreadEnvHostFactory::open` answering a host that shares an
//!   `Arc<Mutex<…>>` with the test) ⇒ a real env, an atom round trip through that host's own table, a
//!   send with the C's `undefined` sender (never a fabricated pid) and a free that lands back on the
//!   host that allocated the env.
//!
//! **Boundary:** ABI level only. `beam_abi` has no live `AtomTable`, no mailbox and no wake-up here —
//! a host's `NifHost::send` is where that lives, and this module asserts the seam the host is called
//! through, not the runtime behind it.
//!
//! The factory is **process-global** while the host it opens is cached per thread, so every assertion
//! here runs behind [`FACTORY_GUARD_LOCK`] and under a [`FactoryGuard`] that clears the factory when
//! the test ends (on panic too). The message env is freed **before** the worker thread ends, because
//! `NifEnv::new` publishes its host as the thread's current host and a drop during thread-local
//! teardown would touch destroyed thread state.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use libc::{c_char, c_int, c_uint};

use beam_abi::nif::enif::envs::{enif_alloc_env, enif_free_env};
use beam_abi::nif::enif::get::enif_get_atom;
use beam_abi::nif::enif::make::enif_make_atom;
use beam_abi::nif::enif::process::enif_send;
use beam_abi::nif::thread_env::{
    clear_thread_env_factory, install_thread_env_factory, with_thread_env_host,
    ThreadEnvHostFactory,
};
use beam_abi::nif::types::{ErlNifCharEncoding, ErlNifEnv, ErlNifPid};
use beam_abi::{nif_from_eterm, NifEnv, NifHost};
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    atom_val, is_atom, make_atom, make_internal_pid, make_pid_data, make_small, Eterm,
};

/// The one lock every factory test in this binary holds: the factory is process-global while the host
/// cache is per-thread, so two tests installing one would answer for each other.
static FACTORY_GUARD_LOCK: Mutex<()> = Mutex::new(());

/// Holds [`FACTORY_GUARD_LOCK`] for one test and clears the factory when the test ends — including
/// when an assertion panics, which is what keeps a failed test from leaking an installed factory (and
/// a cached host) into the next one. The lock guard is a field, so it is released *after* the clear.
struct FactoryGuard {
    _lock: MutexGuard<'static, ()>,
}

impl FactoryGuard {
    /// Serialise and start from "no factory installed".
    fn acquire() -> FactoryGuard {
        let lock = FACTORY_GUARD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The clear bumps the factory generation, so a host cached by an earlier test on this thread
        // is stale and is dropped on the next use.
        clear_thread_env_factory();
        FactoryGuard { _lock: lock }
    }
}

impl Drop for FactoryGuard {
    fn drop(&mut self) {
        clear_thread_env_factory();
    }
}

/// The state a factory shares with the hosts it opens and with the test — the documented
/// `Send + Sync` handle through which a `!Send` host reaches shared runtime state.
#[derive(Default)]
struct Shared {
    /// How often a factory `open`ed a host.
    opens: usize,
    /// How often a host's `alloc_env` actually allocated.
    env_allocs: usize,
    /// Every env pointer handed out, in call order.
    env_ptrs: Vec<usize>,
    /// Every env pointer `free_env` was called with.
    env_frees: Vec<usize>,
    /// Every atom name interned through a host of this factory.
    interned: Vec<String>,
    /// Every send a host accepted: `(receiver, sender, message)`, raw words.
    sends: Vec<(usize, Option<usize>, usize)>,
}

/// A poisoned `Shared` never happens by design; tolerating it keeps a failing test's panic from
/// cascading into an unwrap here.
fn shared(handle: &Arc<Mutex<Shared>>) -> MutexGuard<'_, Shared> {
    handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The factory: opens one [`FactoryHost`] per thread that asks, sharing this handle.
struct TestFactory {
    shared: Arc<Mutex<Shared>>,
}

impl ThreadEnvHostFactory for TestFactory {
    fn open(&self) -> Option<Box<dyn NifHost>> {
        shared(&self.shared).opens += 1;
        Some(Box::new(FactoryHost {
            heap: Heap::new(1024).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            shared: Arc::clone(&self.shared),
            envs: Vec::new(),
        }))
    }
}

/// A host shaped like the runtime's `NifHostImpl`: its `alloc_env` boxes a real [`NifEnv`] over
/// itself and hands out that pointer, `free_env` removes exactly that box, and every observable is
/// recorded on the factory's shared handle.
///
/// `envs` is a `Vec<Box<NifEnv>>` on purpose (the same choice `NifHostImpl` and `beam_abi`'s resource
/// registry make): the pointer a NIF holds must stay valid while the vector grows, so each env owns
/// its own allocation instead of moving with the vector.
#[allow(clippy::vec_box)]
struct FactoryHost {
    heap: Heap,
    atoms: AtomTable,
    shared: Arc<Mutex<Shared>>,
    envs: Vec<Box<NifEnv>>,
}

impl NifHost for FactoryHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        let atom = make_atom(self.atoms.intern(name));
        let handle = Arc::clone(&self.shared);
        let mut recorded = shared(&handle);
        if !recorded.interned.iter().any(|interned| interned == name) {
            recorded.interned.push(name.to_string());
        }
        Some(atom)
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }

    /// The message-env hook, built the way `beam_rt`'s `NifHostImpl::alloc_env` builds it: box a real
    /// `NifEnv` over this host, keep the box alive here, hand out its pointer.
    ///
    /// `NifEnv::new` also publishes this host as the thread's *current* host, so a message env built
    /// this way looks like a call env afterwards. That is a host artifact, not a seam one — which is
    /// why the test frees the env before it asserts anything about which path answered.
    fn alloc_env(&mut self) -> Option<*mut ErlNifEnv> {
        let mut env = Box::new(NifEnv::new(self));
        let ptr = env.as_ptr();
        self.envs.push(env);
        let handle = Arc::clone(&self.shared);
        let mut recorded = shared(&handle);
        recorded.env_allocs += 1;
        recorded.env_ptrs.push(ptr as usize);
        Some(ptr)
    }

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
        // The boxed env is the one the host handed out; dropping it here is what the C's
        // `erts_free(env)` does (`NifEnv`'s own `Drop` withdraws the thread-current host). The
        // caller's contract is that the pointer is not used again.
        drop(self.envs.swap_remove(index));
        let handle = Arc::clone(&self.shared);
        shared(&handle).env_frees.push(env as usize);
        true
    }

    fn send(&mut self, to: Eterm, from: Option<Eterm>, msg: Eterm) -> Option<bool> {
        let handle = Arc::clone(&self.shared);
        shared(&handle)
            .sends
            .push((to.raw(), from.map(Eterm::raw), msg.raw()));
        Some(true)
    }
}

/// An internal pid term for process number `num` (the receiver of a send).
fn pid(num: u64) -> Eterm {
    make_internal_pid(make_pid_data(0, num))
}

/// `enif_alloc_env` off the call path: an honest `NULL` with no factory, a real, usable message env
/// with one — and `enif_send(NULL, &pid, msg_env, msg)` delivering through it.
#[test]
fn enif_alloc_env_off_the_call_path_needs_a_factory_and_then_works() {
    // -----------------------------------------------------------------------------------------
    // A. No factory: a fresh thread (empty host cache) still gets the honest NULL, and the only
    //    `enif_send` shapes that need an env answer 0. This is the seam's fallback, not a
    //    regression: a crate-owned env would carry a PRIVATE atom table and hand a NIF a wrong atom
    //    index, which is strictly worse than "cannot build an env here".
    // -----------------------------------------------------------------------------------------
    {
        let _factory = FactoryGuard::acquire();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: no env is live on this fresh thread and no factory is installed, so the seam's
            // documented answers are NULL / None / 0 respectively.
            let env = unsafe { enif_alloc_env() };
            let reached = with_thread_env_host(|host| host.intern_atom("unreachable")).is_some();
            let to = ErlNifPid {
                pid: nif_from_eterm(pid(7)),
            };
            // SAFETY: `env` is NULL (the C's documented "no calling env" form), `to` is a readable
            // local and `msg_env` is NULL, so nothing is dereferenced.
            let sent = unsafe {
                enif_send(
                    core::ptr::null_mut(),
                    &to,
                    core::ptr::null_mut(),
                    nif_from_eterm(make_small(42)),
                )
            };
            tx.send((env.is_null(), reached, sent)).unwrap();
        })
        .join()
        .expect("the worker thread does not panic");
        assert_eq!(
            rx.recv().expect("the worker reported"),
            (true, false, 0),
            "no factory ⇒ enif_alloc_env is NULL, no host is reachable, and enif_send delivers \
             nothing (the C's 0)"
        );
    }

    // -----------------------------------------------------------------------------------------
    // B. With a factory: the oracle's `probe_worker_atoms`/`probe_thread_send` shape — an env that
    //    round-trips an atom through the factory's own table, a send through the `msg_env` form with
    //    the C's `undefined` sender, and a free that lands back on the host that allocated it.
    // -----------------------------------------------------------------------------------------
    let _factory = FactoryGuard::acquire();
    let handle = Arc::new(Mutex::new(Shared::default()));
    assert!(
        install_thread_env_factory(Box::new(TestFactory {
            shared: Arc::clone(&handle),
        })),
        "a fresh lock accepts a factory"
    );

    let receiver = pid(7);
    let message = make_small(42);
    let worker_handle = Arc::clone(&handle);
    let reported = std::thread::spawn(move || {
        // SAFETY: no env is live on this thread, so the factory path is what answers; the pointer,
        // when non-null, is a live env owned by the host behind it.
        let env = unsafe { enif_alloc_env() };
        assert!(!env.is_null(), "a factory-installed thread gets a message env");

        // The oracle's atom round trip on a worker's env: `enif_make_atom` creates (or finds) the
        // atom in the runtime's one atom table and `enif_get_atom` reads the text back through the
        // very host that interned it (the factory's, not a snapshot).
        // SAFETY: `env` is a live env and the name is a NUL-terminated literal.
        let atom = unsafe { enif_make_atom(env, c"worker_atom".as_ptr()) };
        let mut buf = [0 as c_char; 32];
        // SAFETY: `buf` is 32 writable bytes and `env`/`atom` are live.
        let written = unsafe {
            enif_get_atom(
                env,
                atom,
                buf.as_mut_ptr(),
                buf.len() as c_uint,
                ErlNifCharEncoding::Latin1,
            )
        };
        let text = String::from_utf8(
            buf[..written.saturating_sub(1) as usize]
                .iter()
                .map(|byte| *byte as u8)
                .collect(),
        )
        .expect("the name is ASCII");

        let to = ErlNifPid {
            pid: nif_from_eterm(receiver),
        };
        // SAFETY: `msg_env` is a live env and `to` is an initialised local; `env` is NULL, which is
        // the documented `enif_send(NULL, &pid, msg_env, msg)` form.
        let sent = unsafe {
            enif_send(
                core::ptr::null_mut(),
                &to,
                env,
                nif_from_eterm(message),
            )
        };
        // SAFETY: a null `to_pid` is explicitly allowed by the contract (the C dereferences it; this
        // boundary answers 0).
        let null_pid = unsafe {
            enif_send(
                core::ptr::null_mut(),
                core::ptr::null(),
                env,
                nif_from_eterm(message),
            )
        };
        // SAFETY: neither env is live and no receiver is given: the C's "not sent".
        let both_null = unsafe {
            enif_send(
                core::ptr::null_mut(),
                &to,
                core::ptr::null_mut(),
                nif_from_eterm(message),
            )
        };
        // Free it while this thread is still alive: a message env dropped during thread-local
        // teardown would touch already-destroyed thread state.
        // SAFETY: `env` came from `enif_alloc_env` and is not used again.
        unsafe { enif_free_env(env) };
        (written, text, (sent, null_pid, both_null))
    })
    .join()
    .expect("the worker thread does not panic");

    assert_eq!(
        reported.0,
        "worker_atom".len() as c_int + 1,
        "enif_get_atom answers len + 1 (the C's NUL-inclusive count)"
    );
    assert_eq!(
        reported.1, "worker_atom",
        "the atom round-trips through the factory host's own table"
    );
    assert_eq!(
        reported.2,
        (1, 0, 0),
        "env == NULL still sends (the host comes from msg_env); a null to_pid and two unusable \
         envs are the C's 0"
    );
    let recorded = shared(&worker_handle);
    assert_eq!(recorded.opens, 1, "the worker opened exactly one host");
    assert_eq!(
        recorded.env_allocs, 1,
        "exactly one message env was allocated, on the factory path"
    );
    assert_eq!(
        recorded.sends,
        vec![(receiver.raw(), None, message.raw())],
        "the hook saw exactly the one delivered message, with NO fabricated sender (the C's \
         undefined)"
    );
    assert_eq!(
        recorded.env_frees, recorded.env_ptrs,
        "enif_free_env handed the env back to the host that allocated it"
    );
    assert!(
        recorded
            .interned
            .iter()
            .any(|name| name == "worker_atom"),
        "the env's host really is the factory's host"
    );
}
