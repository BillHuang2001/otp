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

//! The **thread-env host factory** — how `enif_alloc_env` reaches a real [`NifHost`] off the
//! NIF-call path (workspace contract **A.3**).
//!
//! `enif_alloc_env(void)` takes no argument, so it cannot name a host itself. It reaches the host of
//! the NIF call the calling thread is inside through the thread-local stack
//! [`NifEnv::new`](super::host::NifEnv::new) publishes (`super::host`'s `with_current_host`). A thread
//! a NIF *created* (`enif_thread_create`) is never inside a call, so before this seam existed that
//! thread could build no message env — and therefore send nothing.
//!
//! # What the oracle does (measured, OTP 29.1.1 / erts-17.1)
//!
//! The measurements come from the real C fixture `beam_bifs/tests/fixtures/a3_thread_env.c` run
//! against the live system `erl` (the transcript is `beam_bifs/HANDOFF_A3_NIF_HOST.md` §2):
//!
//! | probe | oracle answer |
//! |---|---|
//! | `probe_thread_send()` — an `enif_thread_create`d worker: `enif_alloc_env` → build `{from_thread,42,<<"abc">>}` → `enif_send(NULL, &pid, msg_env, msg)` → `enif_free_env` | `{{0,1,0},0}` — env **not** `NULL`, send code `1`, `enif_thread_type()` `0` (`ERL_NIF_THR_UNDEFINED`), thread created |
//! | `probe_worker_atoms()` — `enif_make_atom`/`enif_get_atom*` on the worker's env | `{0,1,1,1,0}` — env not `NULL`, length ok, text ok, send `1` |
//! | `probe_main_env()` — the same idiom on the call path | `{0,1}` — env not `NULL`, send `1` |
//! | `probe_null_env()` — `enif_send(NULL, …)` from a scheduler thread | `{0,1,1}` — legal unconditionally, not only off-scheduler |
//! | `probe_thread_send_to(Pid)` — the target parked in `receive` | delivered **and** the receiver woken |
//!
//! The C guarantees this because `enif_alloc_env` touches **no** thread state
//! (`erts_alloc_fnf` + `setup_nif_env(msg_env, NULL, NULL)`,
//! `erts/emulator/beam/erl_nif.c:636-680`): the env is process-independent, and its heap *and* the one
//! VM-global atom table are the runtime's. A thread-local "am I inside a call?" lookup is the wrong
//! shape for that precondition — which is exactly why this crate answered `NULL` there before.
//!
//! # Why the answer was `NULL` and not a crate-owned env
//!
//! A crate-owned env with a private atom table would intern an atom into a table the runtime does not
//! use, so `enif_make_atom(msg_env, "ok")` would hand the NIF an atom term carrying a **wrong atom
//! index** — a wrong *term*, delivered to another process. An honest `NULL` (the C's "cannot build an
//! env here") is strictly better than a silently wrong atom, and that reasoning is unchanged by this
//! seam: it is what the fallback in [`enif_alloc_env`](super::enif::envs::enif_alloc_env) preserves.
//!
//! # What a factory must provide
//!
//! [`ThreadEnvHostFactory::open`] hands out a **real** host, i.e. one that
//!
//! * reaches the runtime's **live** atom table — `enif_make_atom` can *create* an atom, so a snapshot
//!   of the table is not enough (`probe_worker_atoms`'s round trip is the pin), and
//! * injects a send thread-safely — `enif_send` from a NIF-created thread must land in the target's
//!   mailbox and **wake** a parked receiver, while the host itself is `!Send`.
//!
//! That is why the *factory* is `Send + Sync` and the host it opens is not: sharing goes through the
//! factory's own handles (an `Arc<…>` captured by the factory and cloned into every host is the
//! documented idiom), so each thread opens its own host over the same runtime state. Until a host
//! installs a factory, `enif_alloc_env()` off the call path stays `NULL` — an honest typed gap, never
//! a wrong term.
//!
//! # Lifetime and threading rules of the seam
//!
//! * The factory lives in a process-global `Mutex`: **last install wins**, so a second
//!   [`install_thread_env_factory`] replaces the first (the superseded `Box` is dropped under the
//!   lock) and there is exactly one factory at a time.
//! * A **poisoned** lock — a factory (or, in a test, a holder) panicked while holding it — is
//!   reported as `false`/`None`, never as a panic. [`clear_thread_env_factory`] is the explicit
//!   teardown and also restores a poisoned lock, so one panic cannot disable the seam for the life of
//!   the process.
//! * Every install and every clear bumps a global **generation** counter. Each thread caches the host
//!   it opened, stamped with the generation it was opened under, and discards it as soon as the stamp
//!   no longer matches — so a host opened by a *previous* factory is dropped rather than reused, and
//!   a cached host can never outlive the factory that produced it.
//! * [`with_thread_env_host`] runs its closure with the host cached on the calling thread, opening
//!   one on first use. The cache borrow is released before the closure runs (the same discipline —
//!   and the same `unsafe` rationale — as `super::host`'s `with_current_host`), so a closure that
//!   re-enters this seam or interns an atom cannot alias the borrow.
//!
//! # Where it is consumed
//!
//! * [`enif_alloc_env`](super::enif::envs::enif_alloc_env) — step (2) of its reachability order: the
//!   current call's host first, then this factory's host on this thread, then `NULL`.
//! * Ownership is unchanged by this seam: the env `enif_alloc_env` answers belongs to the host that
//!   allocated it, which is why `enif_free_env` needs no factory step at all — it reaches the owning
//!   host through the env pointer itself.
//!
//! # Safety
//!
//! `unsafe` here is confined to the one documented trait-object reborrow in [`with_thread_env_host`],
//! which mirrors `super::host`'s `with_current_host` and carries the same `// SAFETY:` rationale.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::nif::host::NifHost;

/// A process-global source of [`NifHost`]s for threads that are **not** inside a NIF call.
///
/// `Send + Sync + 'static` because the factory is shared by every thread of the process (it lives in
/// a `Mutex`); the [`NifHost`] it opens is neither, because each thread that needs one opens its own.
/// Sharing between them is the factory's job — capture an `Arc<…>` of the runtime's thread-safe
/// state and clone it into every host `open` builds.
///
/// `open` is called with the factory lock held, so it must be cheap and must **not** call
/// [`install_thread_env_factory`]/[`clear_thread_env_factory`] (re-entering the lock would deadlock).
pub trait ThreadEnvHostFactory: Send + Sync + 'static {
    /// Open a fresh host for the calling thread, or `None` when this factory cannot serve it (the
    /// caller then reports the seam's honest "no host reachable", never a fabricated one).
    fn open(&self) -> Option<Box<dyn NifHost>>;
}

/// The one installed factory (`None` until a host installs one, and again after
/// [`clear_thread_env_factory`]).
static FACTORY: Mutex<Option<Box<dyn ThreadEnvHostFactory>>> = Mutex::new(None);

/// Bumped by every [`install_thread_env_factory`] and every [`clear_thread_env_factory`]: a cache
/// entry is only usable while its stamp equals this, so a host opened by a superseded factory is
/// discarded instead of leaking into the next one.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// A host opened by *this* thread through the factory that was installed when it was opened, together
/// with the [`GENERATION`] stamp that decides whether it may still be used.
type CachedHost = (u64, Box<dyn NifHost>);

thread_local! {
    /// This thread's factory-opened host (empty until the thread first asks for one).
    ///
    /// Thread-local because a [`NifHost`] is `!Send`: it carries a raw heap and the runtime's atom
    /// handles, and moving it to another thread would break the "one host borrow at a time" contract
    /// every `enif_*` body relies on.
    static CACHED: RefCell<Option<CachedHost>> = const { RefCell::new(None) };
}

/// Install `factory` as the process-global source of hosts for threads outside a NIF call.
///
/// **Last install wins**: an already installed factory is replaced (and dropped) under the lock.
/// Answers `true` when the factory was installed and `false` only when the lock is **poisoned** (a
/// holder panicked) — the seam then keeps its previous state rather than panicking in turn;
/// [`clear_thread_env_factory`] is what restores it.
///
/// Bumping [`GENERATION`] is what makes every thread re-open: a host cached from the previous factory
/// no longer matches its stamp and is dropped on that thread's next use.
pub fn install_thread_env_factory(factory: Box<dyn ThreadEnvHostFactory>) -> bool {
    match FACTORY.lock() {
        Ok(mut slot) => {
            *slot = Some(factory);
            GENERATION.fetch_add(1, Ordering::AcqRel);
            true
        }
        // A poisoned lock is a failed install, never a panic: the caller learns `false` and the
        // previously installed factory (if any) keeps answering until an explicit clear.
        Err(_) => false,
    }
}

/// Remove the installed factory — the explicit teardown.
///
/// Also drops the calling thread's cached host (so the effect is immediate here; every other thread
/// drops its own on its next use, the generation stamp no longer matching) and restores a poisoned
/// lock, so a panic while the factory was held cannot leave the seam permanently unusable.
pub fn clear_thread_env_factory() {
    let mut slot = match FACTORY.lock() {
        Ok(slot) => slot,
        Err(poisoned) => {
            // A poisoned lock still owns its data; recovering it here is what makes teardown a real
            // teardown rather than a no-op after someone panicked.
            FACTORY.clear_poison();
            poisoned.into_inner()
        }
    };
    *slot = None;
    drop(slot);
    // Clearing the flag even on the un-poisoned path is a no-op; doing it unconditionally is what
    // recovers a lock that another holder panicked under between the `lock` above and this line.
    FACTORY.clear_poison();
    GENERATION.fetch_add(1, Ordering::AcqRel);
    CACHED.with(|cached| *cached.borrow_mut() = None);
}

/// Run `f` with the host the installed factory opened for **this thread**, or answer `None` when no
/// host is reachable (no factory installed, the factory declined, or the lock is poisoned).
///
/// The host is opened on first use and cached for the thread; it is dropped and re-opened whenever
/// the factory changes (see [`GENERATION`]). The cache borrow is released before `f` runs, so `f` may
/// re-enter this seam or build an env that publishes itself as the current host.
pub fn with_thread_env_host<R>(f: impl FnOnce(&mut dyn NifHost) -> R) -> Option<R> {
    let host = current_thread_host()?;
    // SAFETY: `current_thread_host` answers the pointer of a `Box<dyn NifHost>` this thread's
    // `CACHED` slot owns. That box is only replaced or dropped by `open_thread_host` (which the
    // calling thread alone runs) or by `clear_thread_env_factory` on this thread, and neither can
    // run while `f` holds the reborrow unless `f` itself re-enters the seam — the same "one
    // outstanding host borrow" caller contract `super::host`'s `with_current_host` documents. The
    // reborrow is scoped to `f`.
    let host: &mut dyn NifHost = unsafe { &mut *host };
    Some(f(host))
}

/// The pointer of this thread's cached host when it belongs to the currently installed factory, else
/// one newly opened (and cached) host.
fn current_thread_host() -> Option<*mut dyn NifHost> {
    let generation = GENERATION.load(Ordering::Acquire);
    // Fast path: the borrow of `CACHED` ends inside `with`, so the pointer is used with no thread-local
    // borrow outstanding (the discipline `host::with_current_host` documents).
    let cached = CACHED.with(|cached| {
        let mut cached = cached.borrow_mut();
        match cached.as_mut() {
            Some((stamp, host)) if *stamp == generation => {
                Some(&mut **host as *mut dyn NifHost)
            }
            // A stale entry (its factory has been replaced or cleared) is not used; it is dropped in
            // `open_thread_host` so a superseded host cannot outlive its factory.
            _ => None,
        }
    });
    if cached.is_some() {
        return cached;
    }
    open_thread_host()
}

/// Drop any stale cached host and open a fresh one from the installed factory.
fn open_thread_host() -> Option<*mut dyn NifHost> {
    // Drop the superseded host first: it belongs to a factory that has been replaced or cleared, and
    // keeping it alive would leak it into the next factory's lifetime.
    CACHED.with(|cached| *cached.borrow_mut() = None);
    let factory = match FACTORY.lock() {
        Ok(factory) => factory,
        // Poisoned: no host reachable. The seam answers `None` instead of panicking in a foreign
        // thread; `clear_thread_env_factory` restores the lock.
        Err(_) => return None,
    };
    // Read under the lock: only `install`/`clear` bump the generation and both hold this lock, so the
    // stamp names exactly the factory this host came from.
    let generation = GENERATION.load(Ordering::Acquire);
    let mut host = factory.as_ref()?.open()?;
    let ptr = &mut *host as *mut dyn NifHost;
    // Moving the `Box` does not move its allocation, so `ptr` stays valid for as long as the slot
    // holds it.
    CACHED.with(|cached| *cached.borrow_mut() = Some((generation, host)));
    Some(ptr)
}

/// The lock that serialises every in-crate test which installs or observes the process-global
/// factory (this module's own tests and `nif::enif::envs`'s reachability tests).
///
/// The factory is process-global while the host cache is per-thread, and the crate's lib tests run in
/// parallel inside one process, so a test that installs a factory would otherwise change what another
/// test's `enif_alloc_env()` answers.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`TEST_LOCK`], tolerating poison: a test that panics while holding it must make the next
/// one fail on its own assertions, not on an unwrap.
#[cfg(test)]
pub(crate) fn lock_test_state() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
#[path = "thread_env_tests.rs"]
mod tests;
