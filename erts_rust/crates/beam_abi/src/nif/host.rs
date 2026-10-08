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

//! The host seam — the services the runtime provides to the `enif_*` host functions.
//!
//! `beam_abi` freezes the *shape* of the NIF C ABI (the `ErlNif*` types and the `enif_*` function
//! table in [`super::api`]). The `enif_*` functions themselves (added in a later step, see
//! [`super::enif`]) cannot do their job alone: they must allocate terms for the duration of one NIF
//! call, intern and look up atoms, read `erlang:self()`, and convert terms to/from the External Term
//! Format. Those are **runtime** services — `beam_abi` is a declaration layer and must **not** pull
//! in `beam_codec` (ETF), the atom table of a specific module, or the scheduler.
//!
//! [`NifHost`] is that seam: a small trait the runtime implements and hands to the `enif_*` layer.
//! [`NifEnv`] is the concrete per-call environment behind the C-facing opaque `*mut ErlNifEnv`
//! (`struct enif_environment_t`, deliberately opaque in [`super::types`]): it borrows the host for
//! its lifetime and carries the pending-exception slot that mirrors the C
//! `enif_raise_exception` / `enif_clear_env` semantics.
//!
//! `unsafe` is confined to [`NifEnv::host`] and [`NifEnv::from_opaque`], each documented with a
//! `# Safety` section / `// SAFETY:` rationale (workspace `CONTEXT.md` §3).

use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};

use libc::{c_int, c_void};

use beam_alloc::Heap;
use beam_term::prelude::Eterm;

use crate::common::{ErlDrvSysInfo, ErlNifSelectFlags};
use crate::nif::term::ERL_NIF_TERM;
use crate::nif::types::{ErlNifEnv, ErlNifEvent};
/// Services the runtime provides to the `enif_*` implementations of one NIF call.
pub trait NifHost {
    /// The term-allocation arena for the duration of the current call. Must not reallocate while a
    /// NIF holds a pointer into it (see [`beam_alloc::Heap`]).
    fn heap(&mut self) -> &mut Heap;

    /// The runtime's global atom table: intern `name`, returning the atom term.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm>;

    /// Reverse lookup: the text of `atom`, or `None` if it is not a valid atom term.
    fn atom_name(&mut self, atom: Eterm) -> Option<String>;

    /// `erlang:self()` within the NIF (default: unsupported).
    fn self_pid(&mut self) -> Option<Eterm> {
        None
    }

    /// ETF encode (host-provided; `beam_abi` must NOT depend on `beam_codec`).
    fn term_to_binary(&mut self, term: Eterm) -> Option<Vec<u8>> {
        let _ = term;
        None
    }

    /// ETF decode (host-provided).
    fn binary_to_term(&mut self, bytes: &[u8]) -> Option<Eterm> {
        let _ = bytes;
        None
    }

    /// `erlang:make_ref/0` (default: unsupported).
    fn make_ref(&mut self) -> Option<Eterm> {
        None
    }

    // -----------------------------------------------------------------------------------------
    // Hooks for the `enif_*` families that are not wired to this crate yet
    // -----------------------------------------------------------------------------------------
    //
    // Every hook below is a **declaration only**: its default reports "unsupported" (`None`, a null
    // pointer, or `false`), so nobody implementing [`NifHost`] has to change, and the family file
    // that owns the matching `enif_*` function drives real runtime behaviour once the subsystem
    // behind it exists. Each names the C function it backs.

    /// Look up an atom **without creating it** (C `enif_make_existing_atom`); default: unsupported.
    fn existing_atom(&mut self, name: &str) -> Option<Eterm> {
        let _ = name;
        None
    }

    /// The NIF library's private data pointer (C `enif_priv_data`); default: null.
    fn priv_data(&mut self) -> *mut c_void {
        core::ptr::null_mut()
    }

    /// The emulator's information block (C `enif_system_info`, which fills an `ErlNifSysInfo`,
    /// i.e. an [`ErlDrvSysInfo`]); default: unsupported.
    fn system_info(&mut self) -> Option<ErlDrvSysInfo> {
        None
    }

    /// A new unique integer (C `enif_make_unique_integer`). `positive`/`monotonic` are the
    /// `ERL_NIF_UNIQUE_POSITIVE`/`ERL_NIF_UNIQUE_MONOTONIC` properties; default: unsupported.
    fn unique_integer(&mut self, positive: bool, monotonic: bool) -> Option<Eterm> {
        let _ = (positive, monotonic);
        None
    }

    /// Charge `percent` of a timeslice against the calling process (C `enif_consume_timeslice`);
    /// `true` when the process should yield the scheduler. Default: `false`.
    fn consume_timeslice(&mut self, percent: c_int) -> bool {
        let _ = percent;
        false
    }

    /// Whether the process `pid` is alive (C `enif_is_process_alive`); default: unsupported.
    fn is_process_alive(&mut self, pid: Eterm) -> Option<bool> {
        let _ = pid;
        None
    }

    /// Whether the port `port` is alive (C `enif_is_port_alive`); default: unsupported.
    fn is_port_alive(&mut self, port: Eterm) -> Option<bool> {
        let _ = port;
        None
    }

    /// Whether the calling process is alive (C `enif_is_current_process_alive`); default:
    /// unsupported.
    fn is_current_process_alive(&mut self) -> Option<bool> {
        None
    }

    /// Send `msg` to the pid `to` (C `enif_send`). `from` is the sending pid — the owner of the
    /// `msg_env` the message terms belong to — or `None` for the calling process. Default:
    /// unsupported.
    fn send(&mut self, to: Eterm, from: Option<Eterm>, msg: Eterm) -> Option<bool> {
        let _ = (to, from, msg);
        None
    }

    /// Send `data` to the port `port` (C `enif_port_command`); default: unsupported.
    fn port_command(&mut self, port: Eterm, data: Eterm) -> Option<bool> {
        let _ = (port, data);
        None
    }

    /// Resolve the **registered name** `name` to a pid (C `enif_whereis_pid`); default:
    /// unsupported.
    fn whereis_pid(&mut self, name: Eterm) -> Option<Eterm> {
        let _ = name;
        None
    }

    /// Resolve the **registered name** `name` to a port (C `enif_whereis_port`); default:
    /// unsupported.
    fn whereis_port(&mut self, name: Eterm) -> Option<Eterm> {
        let _ = name;
        None
    }

    /// Monitor the process `pid`, returning the monitor term the caller stores (C
    /// `enif_monitor_process`); default: unsupported.
    fn monitor_process(&mut self, pid: Eterm) -> Option<Eterm> {
        let _ = pid;
        None
    }

    /// Cancel the monitor identified by `monitor` (C `enif_demonitor_process`); default:
    /// unsupported.
    fn demonitor_process(&mut self, monitor: Eterm) -> Option<bool> {
        let _ = monitor;
        None
    }

    /// Register (or cancel) poll interest in the OS event `event` (C `enif_select`): `obj` is the
    /// resource the notification is delivered to, `pid` the recipient (`None` = the calling
    /// process) and `reference` the ref that comes with the notification. Returns the C result
    /// code; default: unsupported.
    fn select(
        &mut self,
        event: ErlNifEvent,
        mode: ErlNifSelectFlags,
        obj: *mut c_void,
        pid: Option<Eterm>,
        reference: Eterm,
    ) -> Option<c_int> {
        let _ = (event, mode, obj, pid, reference);
        None
    }

    /// Schedule `fptr` to run as `fun_name`, possibly on a dirty scheduler (C `enif_schedule_nif`),
    /// returning the term of the scheduled call; default: unsupported.
    fn schedule_nif(
        &mut self,
        fun_name: &str,
        flags: c_int,
        fptr: unsafe extern "C" fn(*mut ErlNifEnv, c_int, *const ERL_NIF_TERM) -> ERL_NIF_TERM,
        argc: usize,
    ) -> Option<Eterm> {
        let _ = (fun_name, flags, fptr, argc);
        None
    }

    /// Create a fresh, process-independent term environment (C `enif_alloc_env`) for the
    /// `enif_send`/`enif_make_copy` "message env" idiom; the host owns the returned env (which is
    /// what the C-facing opaque `*mut ErlNifEnv` designates). Default: unsupported.
    fn alloc_env(&mut self) -> Option<*mut ErlNifEnv> {
        None
    }

    /// Destroy an environment created by [`NifHost::alloc_env`] (C `enif_free_env`); default:
    /// unsupported.
    fn free_env(&mut self, env: *mut ErlNifEnv) -> bool {
        let _ = env;
        false
    }

    /// Copy `term` out of a foreign environment into this host's heap (C `enif_make_copy`); default:
    /// unsupported.
    fn make_copy(&mut self, term: Eterm) -> Option<Eterm> {
        let _ = term;
        None
    }

    /// Resolve a resource term owned by **another** NIF module and call that type's `dyncall`
    /// (C `enif_dynamic_resource_call`), returning the C result code (`0` = the call was made).
    /// `env` is the live calling environment, `module`/`name` the atoms identifying the target
    /// resource type, `resource` the resource term and `call_data` the opaque callback argument.
    /// Default: unsupported (`None`), which the caller turns into the C's failure code `1`. A
    /// resource term this crate itself minted is handled before this hook is consulted (see
    /// [`super::enif::resources`]), so the hook is only reached for foreign types.
    fn dynamic_resource_call(
        &mut self,
        env: *mut ErlNifEnv,
        module: Eterm,
        name: Eterm,
        resource: Eterm,
        call_data: *mut c_void,
    ) -> Option<c_int> {
        let _ = (env, module, name, resource, call_data);
        None
    }
}

// ---------------------------------------------------------------------------------------------
// The current-NIF-call host stack — how `enif_alloc_env(void)` finds a host
// ---------------------------------------------------------------------------------------------
//
// `enif_alloc_env(void)` takes no argument, so — alone among the `enif_*` entry points — it has no
// env handle to reach a [`NifHost`] through, and a crate-owned env would have no access to the
// runtime's atom table (it would hand a NIF a *wrong term*: `nif::enif::envs` has the full
// argument). The C solves the reachability problem with process-global allocator state; here the
// equivalent is "the host of the NIF call the calling thread is inside".
//
// [`NifEnv`] is exactly that call's handle, and [`NifEnv::new`] is the **single** construction point
// for a real call env (the runtime and [`crate::nif::loader`] both go through it), so `new`
// publishes its host on this thread's stack and `NifEnv`'s `Drop` removes it again. While any env is
// live, the top of the stack is precisely the host of the current NIF call; once the outermost one
// is dropped the stack is empty and `enif_alloc_env` answers `NULL`.
//
// A **stack** rather than a single slot, because a NIF call can nest (the loader's `unload`
// callback runs inside `NifLibrary::drop`, which may run while an outer call env is still live).
// Each entry carries the token of the [`NifEnv`] that published it, so an out-of-order drop removes
// exactly its own entry instead of whichever one happens to be on top.
//
// The store is **thread-local**, not global: a NIF call is made on one thread and [`NifEnv`] is not
// `Send` (it holds a raw host pointer), so the host a thread publishes is only ever read back on
// that same thread.

thread_local! {
    /// The hosts published by the [`NifEnv`]s live on this thread, innermost (current) last.
    static CURRENT_HOSTS: RefCell<Vec<(u64, *mut dyn NifHost)>> = const { RefCell::new(Vec::new()) };
}

/// Identity of the next [`NifEnv`] (see [`CURRENT_HOSTS`]); drawn once per env, never reused.
static NEXT_ENV_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Publish `host` as the host of the NIF call whose env carries `token` ([`NifEnv::new`]).
fn push_current_host(token: u64, host: *mut dyn NifHost) {
    CURRENT_HOSTS.with(|stack| stack.borrow_mut().push((token, host)));
}

/// Remove `token`'s entry, wherever it sits ([`NifEnv`]'s `Drop`).
///
/// Removing a *position* rather than the top keeps the stack sound when envs are dropped out of
/// nesting order: every env removes its own entry, so the count can only reach zero when every env
/// is gone — the invariant the "no live env ⇒ `enif_alloc_env` answers `NULL`" rule rests on.
fn pop_current_host(token: u64) {
    CURRENT_HOSTS.with(|stack| {
        let mut stack = stack.borrow_mut();
        if let Some(pos) = stack.iter().position(|&(t, _)| t == token) {
            stack.remove(pos);
        }
    });
}

/// Run `f` with the host of the innermost live [`NifEnv`] on this thread, or answer `None` when no
/// env is live (i.e. this thread is not inside a NIF call).
///
/// This is the reachability mechanism behind `enif_alloc_env` (`nif::enif::envs`), the one `enif_*`
/// entry point that takes no argument and so cannot name a host itself. The thread-local borrow is
/// released before `f` runs, so `f` may itself construct an env (a nested push).
pub(crate) fn with_current_host<R>(f: impl FnOnce(&mut dyn NifHost) -> R) -> Option<R> {
    let host = CURRENT_HOSTS.with(|stack| stack.borrow().last().map(|&(_, host)| host))?;
    // SAFETY: the entry was pushed by a live `NifEnv`, which by its own contract keeps the host
    // alive until that env is dropped, and dropping the env removes the entry — so a host named by
    // an entry on the stack is live. An env holds only the raw pointer (`NifEnv::host` is the only
    // place it is turned back into a reference, and `NifEnv` is not `Clone`), so no other
    // `&mut dyn NifHost` into that host is outstanding here, and this reborrow is scoped to `f`.
    let host: &mut dyn NifHost = unsafe { &mut *host };
    Some(f(host))
}

/// The concrete per-call NIF environment behind the C-facing opaque `*mut ErlNifEnv`.
///
/// Holds a raw pointer to the host (so the environment itself is lifetime-free and can be recovered
/// from the opaque C pointer by [`NifEnv::from_opaque`]) plus the pending-exception slot.
pub struct NifEnv {
    /// The host, borrowed for the lifetime of this environment.
    ///
    /// A raw fat pointer rather than `&'a mut dyn NifHost`: the C side only ever sees
    /// `*mut ErlNifEnv`, and the recover path ([`NifEnv::from_opaque`]) has no way to name the
    /// borrow's lifetime. The caller must keep the host alive for as long as the environment is.
    /// (The `'static` that a `*mut dyn Trait` spells by default is erased by the documented
    /// lifetime-erasing cast in [`NifEnv::new`].)
    host: *mut dyn NifHost,
    /// Identity of this env's [`CURRENT_HOSTS`] entry, so `Drop` removes exactly its own.
    token: u64,
    /// The exception reason recorded by a raise, if any (the C `enif_raise_exception` slot).
    pending_exception: Option<Eterm>,
}

impl NifEnv {
    /// Wrap `host` in a fresh per-call environment (no pending exception).
    ///
    /// The env also **publishes `host` as the current NIF-call host on this thread** (removed again
    /// by `Drop`) — the mechanism `enif_alloc_env` uses to reach a host from an entry point that
    /// takes no argument (see `with_current_host` below). Constructing one is therefore reserved for
    /// a real call env: it is what "this thread is inside a NIF call" means to this crate.
    pub fn new(host: &mut dyn NifHost) -> NifEnv {
        // SAFETY: the only thing reinterpreted here is the erased trait-object lifetime. The
        // C-facing env has no lifetime parameter (`struct enif_environment_t` is plain C), so the
        // borrow cannot be expressed in the type; `host as *mut dyn NifHost` alone would be
        // rejected because a raw pointer to a trait object is invariant and defaults to `'static`
        // (raw pointer *casts* may not extend a trait-object lifetime either — rust#141402).
        // `&mut dyn NifHost` and `*mut dyn NifHost` are layout-identical (data word + vtable word),
        // and `mem::transmute` between them is a bit-preserving cast, so the caller's borrow is
        // reproduced exactly by `NifEnv::host`. The caller must keep the host alive for as long as
        // the environment is used (`NifEnv` is not `Clone`, so there is exactly one handle).
        let host: *mut dyn NifHost = unsafe { core::mem::transmute(host) };
        let token = NEXT_ENV_TOKEN.fetch_add(1, Ordering::Relaxed);
        push_current_host(token, host);
        NifEnv {
            host,
            token,
            pending_exception: None,
        }
    }

    /// Reborrow the host (the env borrows the host for its lifetime).
    pub fn host(&mut self) -> &mut dyn NifHost {
        // SAFETY: `host` was produced from a live `&mut dyn NifHost` in `new` (or a previous
        // `host`/`as_ptr`/`from_opaque` round-trip), and `NifEnv` is not `Clone`, so this is the
        // only outstanding handle to it; the returned reborrow is tied to `&mut self`.
        unsafe { &mut *self.host }
    }

    /// Convenience: `self.host().heap()`.
    pub fn heap(&mut self) -> &mut Heap {
        self.host().heap()
    }

    /// The C-facing opaque pointer. Casts `self as *mut NifEnv as *mut ErlNifEnv`.
    pub fn as_ptr(&mut self) -> *mut ErlNifEnv {
        self as *mut NifEnv as *mut ErlNifEnv
    }

    /// Recover the concrete env from an opaque pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a pointer previously returned by [`NifEnv::as_ptr`] for an env that is still
    /// alive (and whose host is still alive). Returns `None` for a null pointer.
    pub unsafe fn from_opaque<'a>(ptr: *mut ErlNifEnv) -> Option<&'a mut NifEnv> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees `ptr` came from `NifEnv::as_ptr` on a live env, i.e. it is
        // a `*mut NifEnv` with the provenance required to reborrow the same allocation; `'a` is the
        // caller's promise that the env outlives the returned reference.
        Some(unsafe { &mut *(ptr as *mut NifEnv) })
    }

    /// The pending exception reason, if a raise is recorded.
    pub fn pending_exception(&self) -> Option<Eterm> {
        self.pending_exception
    }

    /// Record `reason` as the pending exception (C `enif_raise_exception` semantics).
    pub fn set_exception(&mut self, reason: Eterm) {
        self.pending_exception = Some(reason);
    }

    /// Clear the pending exception (C `enif_clear_env` semantics).
    pub fn clear(&mut self) {
        self.pending_exception = None;
    }
}

impl Drop for NifEnv {
    /// Withdraw this env's entry from the current-call host stack (the counterpart of the push in
    /// [`NifEnv::new`]), so `enif_alloc_env` sees the host of the enclosing call again — or no host
    /// at all, once the outermost env is gone.
    ///
    /// The env is the only publisher of its own token and is not `Clone`, so exactly one entry can
    /// carry it.
    fn drop(&mut self) {
        pop_current_host(self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::atom::{Atom, AtomTable};
    use beam_term::prelude::{atom_val, is_atom, make_atom, make_small};

    /// A minimal host: a bump heap plus an atom table.
    struct TestHost {
        heap: Heap,
        atoms: AtomTable,
    }

    impl TestHost {
        fn new() -> TestHost {
            TestHost {
                heap: Heap::new(256).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
            }
        }
    }

    impl NifHost for TestHost {
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
    }

    #[test]
    fn as_ptr_round_trips_through_from_opaque() {
        let mut host = TestHost::new();
        let reason = host.intern_atom("boom").expect("interns unconditionally");
        let mut env = NifEnv::new(&mut host);
        env.set_exception(reason);
        let ptr = env.as_ptr();
        assert!(!ptr.is_null(), "the env pointer is never null");
        // SAFETY: `ptr` came from `NifEnv::as_ptr` on `env`, which is still alive here.
        let recovered = unsafe { NifEnv::from_opaque(ptr) }.expect("non-null pointer");
        assert_eq!(
            recovered.pending_exception(),
            Some(reason),
            "the exception recorded before the round-trip survives it"
        );
    }

    #[test]
    fn from_opaque_of_null_is_none() {
        // SAFETY: a null pointer is explicitly allowed by the contract.
        let recovered = unsafe { NifEnv::from_opaque(std::ptr::null_mut()) };
        assert!(recovered.is_none());
    }

    #[test]
    fn clear_empties_the_exception_slot() {
        let mut host = TestHost::new();
        let reason = host.intern_atom("boom").expect("interns unconditionally");
        let mut env = NifEnv::new(&mut host);
        assert_eq!(env.pending_exception(), None);
        env.set_exception(reason);
        assert_eq!(env.pending_exception(), Some(reason));
        env.clear();
        assert_eq!(env.pending_exception(), None);
    }

    #[test]
    fn host_services_are_reachable_through_the_env() {
        let mut host = TestHost::new();
        let mut env = NifEnv::new(&mut host);
        assert!(env.heap().available() > 0, "the host heap is reachable");
        let atom = env.host().intern_atom("hello").expect("interns");
        assert!(is_atom(atom));
        assert_eq!(env.host().atom_name(atom).as_deref(), Some("hello"));
        // An atom term whose index is not in this host's table has no name...
        assert_eq!(env.host().atom_name(make_atom(Atom::from_raw(999))), None);
        // ...and a non-atom term is never a valid atom.
        assert_eq!(env.host().atom_name(make_small(7)), None);
    }
}
