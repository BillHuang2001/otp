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

//! Path-included tests for [`super`] (`nif::thread_env`) — the process-global factory seam that gives
//! a thread **outside** a NIF call a real [`NifHost`], so `enif_alloc_env` can hand out a message env
//! and `enif_send` can deliver from it (workspace contract **A.3**).
//!
//! The oracle transcript these tests pin (`beam_bifs/tests/fixtures/a3_thread_env.c`, run against the
//! live system `erl`, OTP 29.1.1 / erts-17.1) is
//! `thread_send = {{0,1,0},0}`, `worker_atoms = {0,1,1,1,0}`, `main_env = {0,1}`,
//! `null_env = {0,1,1}`: an `enif_thread_create`d worker gets a **non-NULL** env, interns and reads
//! back an atom through it, and sends with `env == NULL`, while the call path keeps working unchanged.
//!
//! # Why these tests take a lock
//!
//! The factory is **process-global** while the host cache is **per-thread**, and the crate's lib tests
//! run in parallel inside one process. Every test here therefore holds the crate-wide [`TEST_LOCK`]
//! (shared with `nif::enif::envs`'s "no reachable host ⇒ NULL" tests) and clears the factory through
//! [`FactoryGuard`], so a panicking assertion cannot leak an installed factory into the next test.
//! Where the assertion depends on a *thread-local* (the cached host), the test uses a **spawned**
//! thread and sequences it with channels — never with sleeps.

use super::{clear_thread_env_factory, install_thread_env_factory, with_thread_env_host};
use super::{lock_test_state, ThreadEnvHostFactory};

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use libc::{c_char, c_int, c_uint};

use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    atom_val, is_atom, make_atom, make_internal_pid, make_pid_data, make_small, Eterm,
};

use crate::nif::enif::n;
use crate::nif::enif::{enif_alloc_env, enif_free_env, enif_get_atom, enif_make_atom, enif_send};
use crate::nif::host::{NifEnv, NifHost};
use crate::nif::types::{ErlNifCharEncoding, ErlNifEnv, ErlNifPid};

// ---------------------------------------------------------------------------------------------
// The one lock every test in this file holds, and the teardown it guarantees
// ---------------------------------------------------------------------------------------------

/// Holds the crate-wide `TEST_LOCK` for one test and clears the factory when the test ends —
/// including when an assertion panics, which is what keeps a failure from leaking an installed
/// factory (and its host cache) into the next test. The lock guard is a field, so it is released
/// *after* the factory has been cleared.
struct FactoryGuard {
    _lock: MutexGuard<'static, ()>,
}

impl FactoryGuard {
    /// Serialise on the crate-wide lock and start from "no factory installed".
    fn acquire() -> FactoryGuard {
        let lock = lock_test_state();
        // Every test starts from the same baseline: no factory, and this thread's cache emptied (the
        // clear bumps the generation, so a host cached by an earlier test on this thread is stale).
        clear_thread_env_factory();
        FactoryGuard { _lock: lock }
    }
}

impl Drop for FactoryGuard {
    fn drop(&mut self) {
        clear_thread_env_factory();
    }
}

// ---------------------------------------------------------------------------------------------
// A factory host whose every observable is recorded in a `Send + Sync` handle
// ---------------------------------------------------------------------------------------------

/// The state a factory shares with its hosts and with the test (the factory's `Send + Sync` handle —
/// the documented way a `!Send` host reaches shared runtime state).
#[derive(Default)]
struct Shared {
    /// How often a factory `open`ed a host.
    opens: usize,
    /// How often a host's `alloc_env` actually allocated (i.e. how many message envs it handed out).
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
    /// Whether the hosts it opens support message envs (the `NifHost::alloc_env` hook).
    alloc_env: bool,
}

impl ThreadEnvHostFactory for TestFactory {
    fn open(&self) -> Option<Box<dyn NifHost>> {
        shared(&self.shared).opens += 1;
        Some(Box::new(FactoryHost {
            heap: Heap::new(1024).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            shared: Arc::clone(&self.shared),
            alloc_env: self.alloc_env,
            envs: Vec::new(),
        }))
    }
}

/// A host identical in shape to the runtime's `NifHostImpl`: its `alloc_env` boxes a real `NifEnv`
/// over itself and hands out that pointer, `free_env` removes exactly that box, and every observable
/// is recorded on the factory's shared handle.
///
/// `envs` is a `Vec<Box<NifEnv>>` on purpose (the same choice `NifHostImpl` and
/// `super::super::enif::resources` make): the pointer handed to the NIF must stay valid while the
/// vector grows, so each env has to own its own allocation instead of moving with the vector.
#[allow(clippy::vec_box)]
struct FactoryHost {
    heap: Heap,
    atoms: AtomTable,
    shared: Arc<Mutex<Shared>>,
    alloc_env: bool,
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
    /// Note that `NifEnv::new` also publishes this host as the thread's *current* host, so a message
    /// env built this way looks like a call env afterwards — an artifact of the host, not of the seam.
    /// Tests that need to observe the factory path itself therefore free the env first (see each
    /// test's comment).
    fn alloc_env(&mut self) -> Option<*mut ErlNifEnv> {
        if !self.alloc_env {
            return None;
        }
        let env = Box::new(NifEnv::new(self));
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
        self.envs.swap_remove(index);
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

/// A **call-path** host (`NifEnv::new(&mut host)`): `alloc_env` answers whatever the test configured,
/// and counts how often it was asked — which is how "the call host won" is proved.
struct CallHost {
    heap: Heap,
    atoms: AtomTable,
    env: Option<*mut ErlNifEnv>,
    alloc_calls: usize,
}

impl CallHost {
    fn new(env: Option<*mut ErlNifEnv>) -> CallHost {
        CallHost {
            heap: Heap::new(1024).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            env,
            alloc_calls: 0,
        }
    }
}

impl NifHost for CallHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }

    fn alloc_env(&mut self) -> Option<*mut ErlNifEnv> {
        self.alloc_calls += 1;
        self.env
    }
}

/// The pointer `enif_alloc_env` answers for a call host that supports the hook. Nothing dereferences
/// it (the seam hands the host's own env straight back), so a sentinel is enough to tell the two
/// sources apart.
fn sentinel(addr: usize) -> *mut ErlNifEnv {
    addr as *mut ErlNifEnv
}

/// An internal pid term for process number `num` (the receiver of a send).
fn pid(num: u64) -> Eterm {
    make_internal_pid(make_pid_data(0, num))
}

// ---------------------------------------------------------------------------------------------
// 1. No factory ⇒ still NULL, exactly as before the seam existed
// ---------------------------------------------------------------------------------------------

/// With **no factory installed**, a fresh thread outside a NIF call still gets `NULL`: the seam's
/// fallback is an honest refusal, never a fabricated env (a crate-owned env would carry a wrong atom
/// index). The spawned thread is what makes this a real assertion — its host cache is empty.
#[test]
fn no_factory_means_alloc_env_is_still_null_on_a_fresh_thread() {
    let _factory = FactoryGuard::acquire();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // SAFETY: `enif_alloc_env` dereferences no caller state; with no factory and no live env the
        // seam answers NULL.
        let env = unsafe { enif_alloc_env() };
        let reached = with_thread_env_host(|host| host.intern_atom("unreachable")).is_some();
        tx.send((env.is_null(), reached)).unwrap();
    })
    .join()
    .expect("the worker thread does not panic");
    assert_eq!(
        rx.recv().expect("the worker reported"),
        (true, false),
        "no factory => no reachable host off the call path: enif_alloc_env is NULL"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. A factory-installed thread gets a genuinely usable message env
// ---------------------------------------------------------------------------------------------

/// The oracle's `probe_thread_send`/`probe_worker_atoms` shape: an `enif_thread_create`d worker
/// (`{{0,1,0},0}`, env **not** NULL) gets a non-NULL env from the factory, the env's host really is
/// the factory's host (atoms round-trip through it), and `enif_free_env` hands the env back to that
/// very host.
#[test]
fn a_factory_installed_thread_gets_a_usable_env_and_hands_it_back() {
    let _factory = FactoryGuard::acquire();
    let handle = Arc::new(Mutex::new(Shared::default()));
    assert!(
        install_thread_env_factory(Box::new(TestFactory {
            shared: Arc::clone(&handle),
            alloc_env: true,
        })),
        "a fresh lock accepts a factory"
    );

    let worker_handle = Arc::clone(&handle);
    let text = std::thread::spawn(move || {
        // SAFETY: no env is live on this thread, so the factory path is what answers; the pointer,
        // when non-null, is a live env owned by the host behind it.
        let env = unsafe { enif_alloc_env() };
        assert!(!env.is_null(), "a factory-installed thread gets a message env");

        // The env's host is the factory's host: interning through the recovered env lands in the
        // factory's shared record.
        {
            // SAFETY: `env` is a live env (from `enif_alloc_env`), used and released in this thread.
            let recovered = unsafe { NifEnv::from_opaque(env) }.expect("non-null env");
            let atom = recovered
                .host()
                .intern_atom("through_the_env")
                .expect("the factory host interns");
            assert!(is_atom(atom));
        }

        // The oracle's atom round trip on a worker's env: `enif_make_atom` creates (or finds) the
        // atom in the runtime's one atom table and `enif_get_atom` reads the text back.
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
        let name = String::from_utf8(
            buf[..written.saturating_sub(1) as usize]
                .iter()
                .map(|byte| *byte as u8)
                .collect(),
        )
        .expect("the name is ASCII");

        // SAFETY: `env` came from `enif_alloc_env` and is not used again.
        unsafe { enif_free_env(env) };
        name
    })
    .join()
    .expect("the worker thread does not panic");

    assert_eq!(text, "worker_atom", "the atom round-trips through the worker's env");
    let recorded = shared(&worker_handle);
    assert_eq!(recorded.opens, 1, "the worker opened exactly one host");
    assert_eq!(
        recorded.env_allocs, 1,
        "exactly one message env was allocated on the factory path"
    );
    assert_eq!(
        recorded.env_frees,
        recorded.env_ptrs,
        "enif_free_env handed the env back to the host that allocated it"
    );
    assert!(
        recorded
            .interned
            .iter()
            .any(|name| name == "through_the_env"),
        "the env's host is the factory's host"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. enif_send(NULL, &pid, msg_env, msg) — the message-env form
// ---------------------------------------------------------------------------------------------

/// The oracle's `probe_thread_send`/`probe_null_env` send (`{0,1,0}` / `{0,1,1}`: code **1**): from a
/// factory-installed thread, `enif_send(NULL, &pid, msg_env, msg)` resolves its host from `msg_env`
/// and the hook sees the message; a **null** `to_pid` is still the C's `0`.
#[test]
fn enif_send_with_a_null_env_resolves_the_host_from_the_message_env() {
    let _factory = FactoryGuard::acquire();
    let handle = Arc::new(Mutex::new(Shared::default()));
    assert!(install_thread_env_factory(Box::new(TestFactory {
        shared: Arc::clone(&handle),
        alloc_env: true,
    })));

    let receiver = pid(7);
    let message = make_small(42);
    let worker_handle = Arc::clone(&handle);
    let codes = std::thread::spawn(move || {
        // SAFETY: no env is live here, so the factory provides the host.
        let msg_env = unsafe { enif_alloc_env() };
        assert!(!msg_env.is_null(), "the worker's message env");
        let to = ErlNifPid { pid: n(receiver) };
        // SAFETY: `msg_env` is a live env and `to` is an initialised local; `env` is NULL, which is
        // the documented `enif_send(NULL, &pid, msg_env, msg)` form.
        let sent = unsafe { enif_send(core::ptr::null_mut(), &to, msg_env, n(message)) };
        // SAFETY: a null `to_pid` is explicitly allowed by the contract (the C dereferences it, this
        // boundary answers 0).
        let null_pid =
            unsafe { enif_send(core::ptr::null_mut(), core::ptr::null(), msg_env, n(message)) };
        // SAFETY: neither env is live and no receiver is given: the C's "not sent".
        let both_null = unsafe {
            enif_send(
                core::ptr::null_mut(),
                &to,
                core::ptr::null_mut(),
                n(message),
            )
        };
        // SAFETY: `msg_env` came from `enif_alloc_env` and is not used again.
        unsafe { enif_free_env(msg_env) };
        (sent, null_pid, both_null)
    })
    .join()
    .expect("the worker thread does not panic");

    assert_eq!(
        codes,
        (1, 0, 0),
        "env == NULL still sends (host from msg_env); a null to_pid and two unusable envs are 0"
    );
    assert_eq!(
        shared(&worker_handle).sends,
        vec![(receiver.raw(), None, message.raw())],
        "the hook saw exactly the one delivered message, with no fabricated sender"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. The current call host wins over the factory
// ---------------------------------------------------------------------------------------------

/// The call path is unchanged: inside a live `NifEnv`, `enif_alloc_env` uses the **call** host even
/// when a factory is installed (the factory is never opened on this thread).
#[test]
fn the_current_call_host_wins_over_the_factory() {
    let _factory = FactoryGuard::acquire();
    let handle = Arc::new(Mutex::new(Shared::default()));
    assert!(install_thread_env_factory(Box::new(TestFactory {
        shared: Arc::clone(&handle),
        alloc_env: true,
    })));

    let want = sentinel(0x1234_5678);
    let mut call_host = CallHost::new(Some(want));
    {
        let _call_env = NifEnv::new(&mut call_host);
        // SAFETY: the call env is live, so a current host exists; nothing else is dereferenced.
        assert_eq!(
            unsafe { enif_alloc_env() },
            want,
            "the call host's own env comes back verbatim"
        );
    }
    assert_eq!(call_host.alloc_calls, 1, "the call host was consulted");
    let recorded = shared(&handle);
    assert_eq!(recorded.opens, 0, "the factory was never opened on this thread");
    assert_eq!(recorded.env_allocs, 0, "the factory allocated nothing");
}

// ---------------------------------------------------------------------------------------------
// 5. A current host without the hook falls through to the factory
// ---------------------------------------------------------------------------------------------

/// A call host whose `alloc_env` is the trait default (`None`) does not force an honest `NULL`: the
/// seam falls through to the factory, which is strictly better than a refusal (a host that cannot
/// allocate can never supply a wrong term). The call host is still consulted first.
#[test]
fn a_current_host_without_alloc_env_falls_through_to_the_factory() {
    let _factory = FactoryGuard::acquire();
    let handle = Arc::new(Mutex::new(Shared::default()));
    assert!(install_thread_env_factory(Box::new(TestFactory {
        shared: Arc::clone(&handle),
        alloc_env: true,
    })));

    let mut call_host = CallHost::new(None);
    let env = {
        let _call_env = NifEnv::new(&mut call_host);
        // SAFETY: the call env is live and the factory is installed, so both sources are reachable.
        let env = unsafe { enif_alloc_env() };
        assert!(!env.is_null(), "the factory answered where the call host could not");
        env
    };
    assert_eq!(call_host.alloc_calls, 1, "the call host was tried first");
    {
        let recorded = shared(&handle);
        assert_eq!(recorded.opens, 1, "the factory opened a host on the fall-through");
        assert_eq!(recorded.env_allocs, 1, "the factory host allocated the env");
        assert_eq!(recorded.env_ptrs, vec![env as usize]);
    }
    // Free it while the thread is alive: the env lives in the factory host's box, and a message env
    // dropped during thread-local teardown would touch already-destroyed thread state.
    // SAFETY: `env` came from `enif_alloc_env` and is not used again.
    unsafe { enif_free_env(env) };
    assert_eq!(shared(&handle).env_frees, vec![env as usize]);
}

// ---------------------------------------------------------------------------------------------
// 6. A new install supersedes the cached host
// ---------------------------------------------------------------------------------------------

/// The generation stamp is what makes a superseded factory stop answering: a host cached by a worker
/// under factory 1 is dropped and re-opened from factory 2 on the worker's very next call. Sequenced
/// with channels (the worker reports, the test installs, the worker calls again) — never with sleeps.
#[test]
fn a_new_install_supersedes_the_cached_host() {
    let _factory = FactoryGuard::acquire();
    let first = Arc::new(Mutex::new(Shared::default()));
    let second = Arc::new(Mutex::new(Shared::default()));
    assert!(install_thread_env_factory(Box::new(TestFactory {
        shared: Arc::clone(&first),
        alloc_env: true,
    })));

    let (report_tx, report_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let worker = std::thread::spawn(move || {
        // First call: opens a host from factory 1 and caches it on this thread.
        let first_ok = with_thread_env_host(|host| host.intern_atom("first")).is_some();
        report_tx.send(first_ok).unwrap();
        go_rx.recv().expect("the test installed the second factory");

        // Second call: the cache is stamped with the old generation, so it is dropped and factory 2
        // opens a fresh host — even though both factories intern the same name.
        let second_ok = with_thread_env_host(|host| host.intern_atom("second")).is_some();
        report_tx.send(second_ok).unwrap();
    });

    assert!(
        report_rx.recv().expect("the worker's first report"),
        "the worker's first call ran on factory 1's host"
    );
    assert_eq!(shared(&first).opens, 1, "factory 1 opened the worker's host");
    assert!(shared(&first).interned.iter().any(|name| name == "first"));

    assert!(
        install_thread_env_factory(Box::new(TestFactory {
            shared: Arc::clone(&second),
            alloc_env: true,
        })),
        "the second factory replaces the first"
    );
    go_tx.send(()).expect("the worker is waiting");

    assert!(report_rx.recv().expect("the worker's second report"));
    worker.join().expect("the worker thread does not panic");
    assert_eq!(
        shared(&first).opens,
        1,
        "the superseded factory is never re-opened"
    );
    assert_eq!(shared(&second).opens, 1, "the new factory opened a fresh host");
    assert!(
        shared(&second)
            .interned
            .iter()
            .any(|name| name == "second"),
        "the worker's second call ran on factory 2's host"
    );
    assert!(
        !shared(&second).interned.iter().any(|name| name == "first"),
        "factory 2's host did not serve the pre-install call"
    );
}

// ---------------------------------------------------------------------------------------------
// 7. A poisoned lock refuses, never panics — and is recoverable
// ---------------------------------------------------------------------------------------------

/// A poisoned factory lock (a holder panicked) makes `install` answer `false` and
/// `with_thread_env_host` answer `None` — the seam never panics in a foreign thread — and
/// [`clear_thread_env_factory`] restores it, so one panic cannot disable the seam for the process.
#[test]
fn a_poisoned_factory_lock_refuses_instead_of_panicking() {
    let _factory = FactoryGuard::acquire();

    // Poison the lock on purpose: panic while holding the factory guard. The default panic hook is
    // silenced for the duration so this expected panic does not print a backtrace into the suite's
    // output (and, if the hook itself were to unwind, it is restored by the guard below).
    let poisoned = {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(|| {
            let _guard = super::FACTORY.lock().expect("the lock starts unpoisoned");
            panic!("poison the factory lock on purpose");
        });
        std::panic::set_hook(hook);
        outcome
    };
    assert!(poisoned.is_err(), "the lock is now poisoned");
    assert!(
        super::FACTORY.is_poisoned(),
        "a panic while holding the guard poisons the factory lock"
    );

    // A poisoned lock refuses: no panic, no fabricated host.
    assert!(
        !install_thread_env_factory(Box::new(TestFactory {
            shared: Arc::new(Mutex::new(Shared::default())),
            alloc_env: true,
        })),
        "an install on a poisoned lock answers false"
    );
    assert!(
        with_thread_env_host(|host| host.intern_atom("refused")).is_none(),
        "a poisoned lock yields no host"
    );

    // The explicit teardown recovers the lock...
    clear_thread_env_factory();
    assert!(
        !super::FACTORY.is_poisoned(),
        "clear_thread_env_factory restores a poisoned lock"
    );

    // ...so the seam works again for the rest of the suite.
    let handle = Arc::new(Mutex::new(Shared::default()));
    assert!(
        install_thread_env_factory(Box::new(TestFactory {
            shared: Arc::clone(&handle),
            alloc_env: true,
        })),
        "a factory can be installed again after the teardown"
    );
    assert_eq!(
        with_thread_env_host(|host| host.intern_atom("recovered").is_some()),
        Some(true)
    );
    assert_eq!(shared(&handle).opens, 1);
    assert!(shared(&handle)
        .interned
        .iter()
        .any(|name| name == "recovered"));
}

/// The seam's own sanity checks, kept next to the behaviour tests: a factory that declines to serve a
/// thread yields `None` (never a fabricated host), and the `enif_send` signature type used above is
/// the frozen one (`c_int`).
#[test]
fn a_factory_that_declines_yields_no_host() {
    let _factory = FactoryGuard::acquire();
    assert!(install_thread_env_factory(Box::new(DecliningFactory)));

    assert!(
        with_thread_env_host(|_| ()).is_none(),
        "a factory that declines leaves the thread without a host"
    );
    // SAFETY: nothing is dereferenced; with a declining factory the seam answers NULL.
    assert!(
        unsafe { enif_alloc_env() }.is_null(),
        "and enif_alloc_env keeps its honest NULL"
    );
    assert_eq!(core::mem::size_of::<c_int>(), 4, "the C `int` the send returns");
}

/// A factory that cannot serve any thread.
struct DecliningFactory;

impl ThreadEnvHostFactory for DecliningFactory {
    fn open(&self) -> Option<Box<dyn NifHost>> {
        None
    }
}
