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

//! `enif_*` — the extra-environment family: `enif_alloc_env`, `enif_free_env`, `enif_make_copy`,
//! `enif_priv_data`.
//!
//! All four slots are populated. (`enif_set_option` is C-variadic — stable Rust cannot give a `...`
//! parameter a body — see [`super`].)
//!
//! # Ground truth (`erts/emulator/beam/erl_nif.c`)
//!
//! | function | C body | reproduced here |
//! |---|---|---|
//! | `enif_alloc_env` | `erts_alloc_fnf(ERTS_ALC_T_NIF, sizeof(struct enif_msg_environment_t))` + `setup_nif_env(msg_env, NULL, NULL)` (`:669-675`) | delegates to [`NifHost::alloc_env`] on the host of the **current NIF call** — see "How `enif_alloc_env` finds its host" below |
//! | `enif_free_env` | `enif_clear_env(env); erts_free(ERTS_ALC_T_NIF, env);` (`:676-680`) | delegates to [`NifHost::free_env`] — the host owns every env it allocated ([`NifHost::alloc_env`]) |
//! | `enif_make_copy` | `make_copy(dst_env, src_term, NULL)` — `size_object` + `copy_struct` onto `dst_env`'s heap (`:1194-1197`, `:843-858`) | delegates to [`NifHost::make_copy`]; when the hook is unsupported it answers `THE_NON_VALUE` |
//! | `enif_priv_data` | `env->mod_nif->priv_data` (`:609-612`) | the private pointer of the **library whose call this is** — published on the calling thread by [`crate::nif::loader::NifLibrary::call`] and read back here; [`NifHost::priv_data`] is the fallback when no library call is in progress |
//!
//! # How `enif_alloc_env` finds its host
//!
//! The C allocates a **process-independent** environment (`enif_alloc_env`, `:669-675`) whose term
//! heap *and* atom table are the runtime's: atoms are global handles in ERTS, so an
//! `enif_alloc_env` env interns into the very same table `enif_make_atom` uses on the call env.
//!
//! In this crate both of those live behind [`NifHost`], and `enif_alloc_env(void)` **takes no
//! argument** — there is no env (and no other handle) to reach a host through. [`NifHost::alloc_env`]
//! is the right seam for the allocation (the host owns what it returns, and [`enif_free_env`] hands
//! it back), so the only missing piece was *reachability*, and that is what
//! `crate::nif::host::NifEnv` supplies: the runtime builds one per NIF call, `NifEnv::new` publishes
//! its host on the calling thread, and `NifEnv`'s `Drop` withdraws it again. `enif_alloc_env` asks
//! that thread-local for "the host of the NIF call this thread is inside" and forwards to
//! [`NifHost::alloc_env`] — the C's own precondition for the call.
//!
//! The answer is therefore whatever the host allocated, which is **atom-correct whenever the host
//! supports the hook**, and `NULL` — never a wrong term — when it does not (the trait default) or
//! when no env is live (this thread is not inside a NIF call). A crate-owned env was the
//! alternative and is refused: the host behind it would have no access to the runtime's atom table,
//! so `enif_make_atom(msg_env, "ok")` would fall into the crate's no-atom-support answer and a NIF
//! would silently get `[]` where the oracle returns the atom `ok` — a *wrong term* reaching another
//! process. A heap-owning env whose atoms are unusable is worth less than an honest `NULL`.
//!
//! # Documented substitutions — never a wrong term, never a crash
//!
//! * A **`NULL` `env`** is answered with `THE_NON_VALUE` (`enif_make_copy`) / `NULL`
//!   (`enif_priv_data`) / a no-op (`enif_free_env`) where the C dereferences it (`:1196`, `:611`,
//!   `:678`) and faults.
//! * **`enif_alloc_env` outside a NIF call** answers `NULL` where the C allocates from
//!   process-global state (there is no current host to delegate to), and so does a **host without
//!   [`NifHost::alloc_env`] support**. The C never returns `NULL` there (an allocation failure
//!   aborts the emulator).
//! * **`enif_free_env` frees nothing it does not own.** The C frees the pointer unconditionally
//!   (`erts_free`, `:679`) because it allocated it in `enif_alloc_env`; here the pointer is handed to
//!   [`NifHost::free_env`], which frees it when the host owns it and reports `false` otherwise (a
//!   per-call env, or a host with no envs of its own) — the pointer is then left alone rather than
//!   freed across allocators.
//! * **`enif_priv_data` answers the in-flight library's pointer first, then the host's.** While a NIF
//!   call is in progress the answer is the calling library's private pointer (what its `load`
//!   callback stored, `erl_nif.c:5003`) — `env->mod_nif->priv_data` verbatim, including when it is
//!   `NULL`, because the library is authoritative for its own private data. With no library call in
//!   progress there is no library to name and the answer is [`NifHost::priv_data`], whose default
//!   `NULL` is also the C's answer for a call env whose `load` stored nothing.
//! * **`enif_priv_data` on a message env** answers that same library pointer where the C would read
//!   `NULL->priv_data` and fault (`env->mod_nif` is `NULL` for an `enif_alloc_env` env,
//!   `setup_nif_env(msg_env, NULL, NULL)`, `:641`); outside a library call it answers the host's
//!   value.
//! * **`enif_make_copy` never aliases the source.** With no copy engine behind the hook the answer is
//!   `THE_NON_VALUE` (the C's own "no term" word, `erl_term.h`; the crate's `prelude::is_non_value`
//!   and every term predicate reject it), never `src_term` itself — returning the source term would
//!   hand out a term whose memory belongs to the *source* env.
//!
//! # The caller contract the raw-pointer ABI cannot enforce
//!
//! Every pointer in this family is a bare C pointer. The C contract for `enif_free_env` is: the
//! pointer must come from an env allocator (`enif_alloc_env`), and **after the call it designates
//! freed memory** (`enif_clear_env(env); erts_free(ERTS_ALC_T_NIF, env)`, `erl_nif.c:676-680`) and
//! must never be used again. **This crate cannot detect a violation of that contract, and it adds no
//! tracking it could not enforce:**
//!
//! * a use-after-`enif_free_env` is a raw pointer to memory its owner has already released. The ABI
//!   carries no owner tag, no generation counter and no allocation registry, and consulting one on
//!   *every* `enif_*` call would be a hot-path cost paid by every NIF to catch a mistake the C does
//!   not catch either. Such a use is therefore undefined behaviour here, exactly as in the C.
//! * what the crate *does* guarantee is the ownership rule: **it never frees memory it did not
//!   allocate.** `enif_free_env` hands the pointer to `NifHost::free_env`, which frees only the envs
//!   it allocated (through `NifHost::alloc_env`) and reports `false` otherwise, so a per-call env, a
//!   foreign env, or a host with no message-env support leaves the pointer **untouched** (where the C
//!   would `erts_free` it and corrupt the owner's heap), and a `NULL` env is a no-op (where the C
//!   faults in `enif_clear_env`).
//! * correspondingly, `enif_make_copy` never fabricates a term from a word that is not one:
//!   `THE_NON_VALUE` and a bare header word answer `THE_NON_VALUE` (the C's `size_object` would walk
//!   the word instead).

use libc::c_void;

use super::*;

/// `THE_NON_VALUE` (`erts/emulator/beam/erl_term.h`: `#define THE_NON_VALUE 0`) — the C's "no term"
/// word. The C uses it for the same purpose in the term-producing operations that can fail without
/// raising (e.g. `enif_map_iterator_get_pair`'s out value, `erl_nif.c:3689`), and
/// `term == THE_NON_VALUE` is how a failed operation is detected (`enif_is_exception`, `:1290`).
const THE_NON_VALUE: ERL_NIF_TERM = ERL_NIF_TERM::from_raw(0);

/// Populate the `ErlNifApi` slots this file owns — all four of the extra-environment family.
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_alloc_env = Some(enif_alloc_env);
    api.enif_free_env = Some(enif_free_env);
    api.enif_make_copy = Some(enif_make_copy);
    api.enif_priv_data = Some(enif_priv_data);
    api
}

// ---------------------------------------------------------------------------------------------
// Extra environments and private data
// ---------------------------------------------------------------------------------------------

/// `ErlNifEnv* enif_alloc_env(void);`
///
/// The C allocates a message environment itself (`erts_alloc_fnf(ERTS_ALC_T_NIF, …)` +
/// `setup_nif_env(msg_env, NULL, NULL)`, `erl_nif.c:669-675`); here the allocation and ownership
/// belong to the runtime, i.e. to [`NifHost::alloc_env`] — the same hook [`enif_free_env`] hands the
/// pointer back to. Since this entry point takes **no argument**, it cannot name a host itself: it
/// reaches the host of the NIF call the calling thread is inside through the thread-local
/// current-host stack `NifEnv::new` publishes and `NifEnv`'s `Drop` withdraws
/// (`crate::nif::host`), and forwards to the hook.
///
/// `NULL` is answered when the thread is not inside a NIF call (no live env, so no host to delegate
/// to) and for a host that does not support message envs (the trait default) — never a crate-owned
/// env, which could not intern atoms into the runtime's atom table and would hand a NIF a *wrong
/// term* (see the module docs).
///
/// # Safety
///
/// The C caller must be inside a NIF call, which is the documented `enif_alloc_env` precondition;
/// outside one the answer is `NULL` rather than a fabricated env. The returned pointer is owned by
/// the host: it must be released with [`enif_free_env`] and not used afterwards (the C contract).
#[no_mangle]
pub unsafe extern "C" fn enif_alloc_env() -> *mut crate::nif::types::ErlNifEnv {
    // `with_current_host` answers `None` when no env is live on this thread, and the closure's
    // `alloc_env` answers `None` when the host has no support for message envs: both are `NULL`.
    crate::nif::host::with_current_host(|host| host.alloc_env())
        .flatten()
        .unwrap_or(core::ptr::null_mut())
}

/// `void enif_free_env(ErlNifEnv* env);`
///
/// Hands the env back to the host that owns it ([`NifHost::free_env`]); a host that does not own it
/// (a per-call env, or no [`NifHost::alloc_env`] support) reports `false` and the pointer is left
/// alone — this crate never frees memory it did not allocate.
///
/// # Safety
///
/// `env` must be null, or a live env pointer produced by an env allocator (the C contract: a pointer
/// from `enif_alloc_env`); a null `env` is a no-op. After the call the pointer must not be used
/// again — a **use-after-`enif_free_env` is a caller-contract violation this boundary cannot detect**
/// (see the module docs, "The caller contract the raw-pointer ABI cannot enforce").
#[no_mangle]
pub unsafe extern "C" fn enif_free_env(env: *mut crate::nif::types::ErlNifEnv) {
    // The C dereferences `env` (`enif_clear_env(env)`, `erl_nif.c:678`): null is a fault there and a
    // no-op here.
    if env.is_null() {
        return;
    }
    if let Some(e) = unsafe { env_mut(env) } {
        // The host frees the envs it allocated (`NifHost::alloc_env`, whose contract already says the
        // host owns the returned env). A `false` report means `env` is not one of them: the C would
        // `erts_free` the pointer here, but freeing an allocation this crate did not make would
        // corrupt the owner's heap, so that case is a deliberate no-op.
        //
        // The borrow of `e` ends with this call, exactly like the C's `erts_free(env)`: nothing reads
        // `env` afterwards.
        let _freed = e.host().free_env(env);
    }
}

/// `ERL_NIF_TERM enif_make_copy(ErlNifEnv* dst_env, ERL_NIF_TERM src_term);`
///
/// Delegates the term movement to [`NifHost::make_copy`] (the crate has no copy engine: `copy_struct`
/// belongs to `beam_message`, and depending on it would reroute the workspace crate DAG). With no
/// support behind the hook the answer is `THE_NON_VALUE` — never `src_term`, which would alias the
/// source env's memory.
///
/// # Safety
///
/// `dst_env` must be null or a live env; `src_term` must be a term (a non-term word is refused).
#[no_mangle]
pub unsafe extern "C" fn enif_make_copy(
    dst_env: *mut crate::nif::types::ErlNifEnv,
    src_term: ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    // A word that is not a term at all (`THE_NON_VALUE`, a bare header word) has no size to copy —
    // the C's `size_object` would walk it. Answer the C's own "no term".
    if !is_term_word(t(src_term)) {
        return THE_NON_VALUE;
    }
    let Some(e) = (unsafe { env_mut(dst_env) }) else {
        return THE_NON_VALUE;
    };
    match e.host().make_copy(t(src_term)) {
        Some(copied) => n(copied),
        None => THE_NON_VALUE,
    }
}

/// `void* enif_priv_data(ErlNifEnv* env);`
///
/// The private data pointer of the **library the call belongs to** — the value that library's `load`
/// callback stored, which [`crate::nif::loader::NifLibrary::call`] publishes for the duration of the
/// call and this function reads back. That *is* the C's `env->mod_nif->priv_data`
/// (`erl_nif.c:609-612`), so it wins over [`NifHost::priv_data`]. With **no library call in
/// progress** on this thread (no library to name) the answer falls back to [`NifHost::priv_data`],
/// which is what a host that manages private data itself reports.
///
/// A published `NULL` is answered as `NULL` rather than falling through to the host: the library is
/// authoritative for its own private data, exactly as `lib->priv_data` is in the C.
///
/// # Safety
///
/// `env` must be null or a live env pointer.
#[no_mangle]
pub unsafe extern "C" fn enif_priv_data(
    env: *mut crate::nif::types::ErlNifEnv,
) -> *mut c_void {
    // The C reads `env->mod_nif->priv_data` (`erl_nif.c:609-612`) and faults on a null env; `NULL` is
    // this boundary's answer where the C dereferences it (see the module docs).
    let Some(e) = (unsafe { env_mut(env) }) else {
        return core::ptr::null_mut();
    };
    // `Some` iff a library call is in progress on this thread. That library's pointer **is**
    // `env->mod_nif->priv_data`, whatever the host hook would say.
    if let Some(priv_data) = crate::nif::loader::current_priv_data() {
        return priv_data;
    }
    // No library call in progress: the host hook is the only remaining source.
    e.host().priv_data()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::nif::host::{NifEnv, NifHost};
    use crate::nif::types::ErlNifEnv;
    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{is_atom, is_small, make_small, Eterm};

    /// The slots this file populates are all four of the extra-environment family.
    #[test]
    fn populate_fills_all_four_extra_environment_slots() {
        let api = populate(ErlNifApi::empty());
        assert!(api.enif_alloc_env.is_some());
        assert!(api.enif_free_env.is_some());
        assert!(api.enif_make_copy.is_some());
        assert!(api.enif_priv_data.is_some());
    }

    // -----------------------------------------------------------------------------------------
    // enif_alloc_env — the host of the current NIF call, reached through the thread-local stack
    // -----------------------------------------------------------------------------------------

    /// A non-null pointer that only ever has to round-trip: the value `enif_alloc_env` answers is
    /// the *host's* allocation (`NifHost::alloc_env`), which this crate neither dereferences nor
    /// owns — it is the host's to describe, hand out and free.
    fn sentinel(addr: usize) -> *mut ErlNifEnv {
        addr as *mut ErlNifEnv
    }

    /// A host whose `alloc_env`/`free_env` hooks are test-controlled; every other hook keeps its
    /// default. `owns` decides whether `free_env` claims the env it is handed (and so "frees" it);
    /// `freed` records every pointer `free_env` was called with, so a test can assert on the exact
    /// delivered pointer after the env borrow has ended (the `Rc` is cloned out before an env
    /// borrows the host).
    struct AllocEnvHost {
        heap: Heap,
        atoms: AtomTable,
        /// What [`NifHost::alloc_env`] answers (`None` = the trait default: unsupported).
        env: Option<*mut ErlNifEnv>,
        /// Whether `free_env` reports that it owns (and frees) the env it is handed.
        owns: bool,
        /// Every pointer `free_env` has been called with, in call order.
        freed: Rc<RefCell<Vec<*mut ErlNifEnv>>>,
    }

    impl AllocEnvHost {
        fn new(env: Option<*mut ErlNifEnv>) -> AllocEnvHost {
            AllocEnvHost {
                heap: Heap::new(256).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                env,
                owns: false,
                freed: Rc::new(RefCell::new(Vec::new())),
            }
        }

        /// Claim ownership of the envs this host is handed: `free_env` then reports `true`.
        fn owning(mut self) -> AllocEnvHost {
            self.owns = true;
            self
        }

        /// A handle on the `free_env` call log, taken **before** an env borrows this host.
        fn free_log(&self) -> Rc<RefCell<Vec<*mut ErlNifEnv>>> {
            Rc::clone(&self.freed)
        }
    }

    impl NifHost for AllocEnvHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(beam_term::prelude::make_atom(self.atoms.intern(name)))
        }

        fn atom_name(&mut self, atom: Eterm) -> Option<String> {
            if !beam_term::prelude::is_atom(atom) {
                return None;
            }
            self.atoms
                .name(beam_term::prelude::atom_val(atom))
                .map(str::to_string)
        }

        fn alloc_env(&mut self) -> Option<*mut ErlNifEnv> {
            self.env
        }

        /// Record the delivered pointer and report ownership (the C `erts_free`s what
        /// `enif_alloc_env` allocated; here only a host that claims the env frees it).
        fn free_env(&mut self, env: *mut ErlNifEnv) -> bool {
            self.freed.borrow_mut().push(env);
            self.owns
        }
    }

    /// The **message-env lifecycle** at the host seam: `enif_alloc_env` hands out the host's env, the
    /// pointer is a genuinely usable env (its host's atom table included), and `enif_free_env` hands
    /// it back to the host that **owns** it — once, with the exact pointer.
    ///
    /// The owner is the *message env's* host — the env the pointer designates, recovered by
    /// `enif_free_env` — not the call env's: the C frees the pointer in the allocator that made it.
    #[test]
    fn alloc_use_free_hands_the_env_back_to_its_owner() {
        let mut msg_host = AllocEnvHost::new(None).owning();
        let msg_freed = msg_host.free_log();
        let mut msg_env = NifEnv::new(&mut msg_host);
        let msg_ptr = msg_env.as_ptr();

        let mut call_host = AllocEnvHost::new(Some(msg_ptr));
        let call_freed = call_host.free_log();
        let mut call_env = NifEnv::new(&mut call_host);
        let call_ptr = call_env.as_ptr();

        // alloc: the host of the current NIF call hands out its own env.
        // SAFETY: `call_env` is live, so a host is current.
        let p = unsafe { enif_alloc_env() };
        assert_eq!(p, msg_ptr, "the host's env comes back verbatim");

        // use: the pointer really is a usable env.
        {
            // SAFETY: `p` came from `NifEnv::as_ptr` on `msg_env`, which is alive here.
            let recovered = unsafe { NifEnv::from_opaque(p) }.expect("non-null");
            let atom = recovered.host().intern_atom("msg_env_atom").expect("interns");
            assert!(is_atom(atom), "the message env's host is reachable through it");
        }

        // free: the owning host is asked exactly once, for the exact pointer.
        // SAFETY: `p` came from `enif_alloc_env` — the C contract for `enif_free_env`.
        unsafe { enif_free_env(p) };
        assert_eq!(*msg_freed.borrow(), vec![msg_ptr]);

        // free(NULL) is a no-op: no host is consulted at all.
        // SAFETY: a null env is explicitly allowed by the contract.
        unsafe { enif_free_env(core::ptr::null_mut()) };
        assert_eq!(msg_freed.borrow().len(), 1, "a NULL free reaches no host");

        // An env the host does not own: the hook reports `false`, so the pointer is left alone.
        // SAFETY: `call_env` is a live env.
        unsafe { enif_free_env(call_ptr) };
        assert_eq!(*call_freed.borrow(), vec![call_ptr]);
        let atom = call_env.host().intern_atom("still_usable").expect("interns");
        assert!(is_atom(atom), "a refused free leaves the env usable");

        // Outside a NIF call there is no current host, so the answer is NULL.
        drop(call_env);
        drop(msg_env);
        // SAFETY: no env is live on this thread here, which is exactly what the call reports.
        assert!(unsafe { enif_alloc_env() }.is_null());
    }

    /// The pointer `enif_alloc_env` answers really is the env a supporting host handed out: it is
    /// recoverable as a `NifEnv` and the host behind it works (its atom table is reachable), which is
    /// the whole point of delegating instead of allocating a crate-owned env.
    ///
    /// A host cannot *build* a genuine `NifEnv` for itself through this crate's public API (the
    /// constructor wants a `&mut dyn NifHost`, which `fn alloc_env(&mut self)` cannot hand itself), so
    /// this test builds one in the test body over a second host and has the current host hand out
    /// that pointer.
    #[test]
    fn alloc_env_hands_out_a_genuinely_usable_env() {
        let mut msg_host = AllocEnvHost::new(None);
        let mut msg_env = NifEnv::new(&mut msg_host);
        let msg_ptr = msg_env.as_ptr();

        let mut call_host = AllocEnvHost::new(Some(msg_ptr));
        let call_env = NifEnv::new(&mut call_host);

        // SAFETY: `call_env` is live, so this reaches `call_host`.
        let p = unsafe { enif_alloc_env() };
        assert_eq!(p, msg_ptr, "the host's own pointer comes back verbatim");

        // It really is a `NifEnv`: recover it and use the host behind it.
        {
            // SAFETY: `p` came from `NifEnv::as_ptr` on `msg_env`, which is alive here.
            let recovered = unsafe { NifEnv::from_opaque(p) }.expect("non-null");
            assert_eq!(recovered.pending_exception(), None, "a message env starts clean");
            let atom = recovered
                .host()
                .intern_atom("msg_env_atom")
                .expect("the recovered host interns");
            assert!(is_atom(atom));
            assert_eq!(
                recovered.host().atom_name(atom).as_deref(),
                Some("msg_env_atom"),
                "the recovered env is usable, atoms included"
            );
        }

        // Out-of-order teardown: the first-published env goes first, and only its own entry leaves.
        drop(msg_env);
        // SAFETY: `call_env` is still live, so its host is still current.
        assert_eq!(unsafe { enif_alloc_env() }, msg_ptr);
        drop(call_env);
        // SAFETY: no env is live on this thread any more.
        assert!(unsafe { enif_alloc_env() }.is_null());
    }

    /// Outside a NIF call there is no current host, so the answer is `NULL` — never a fabricated
    /// env (the C allocates from process-global state and never fails).
    #[test]
    fn alloc_env_outside_a_nif_call_is_null() {
        // A fresh test thread has published no host.
        // SAFETY: this entry point dereferences no caller state; with nothing published it answers
        // NULL.
        assert!(unsafe { enif_alloc_env() }.is_null());
    }

    /// The call's host is reached while its env is live, and is gone once the env is dropped.
    #[test]
    fn alloc_env_forwards_to_the_current_hosts_hook() {
        let want = sentinel(0x1234_5678);
        let mut host = AllocEnvHost::new(Some(want));
        {
            let _env = NifEnv::new(&mut host);
            // SAFETY: `_env` is live (its host is published), so this reads the host it names.
            assert_eq!(unsafe { enif_alloc_env() }, want, "the host's env comes back verbatim");
        }
        // SAFETY: `_env` is dropped, so this thread is outside a NIF call again.
        assert!(
            unsafe { enif_alloc_env() }.is_null(),
            "the host is withdrawn with its env"
        );
    }

    /// A host that does not support message envs answers `NULL` (its `alloc_env` default) — the
    /// honest answer, never a crate-owned env with unusable atoms.
    #[test]
    fn alloc_env_is_null_for_a_host_without_support() {
        let mut host = AllocEnvHost::new(None);
        let _env = NifEnv::new(&mut host);
        // SAFETY: `_env` is live, so the host is reached; the hook itself is unsupported.
        assert!(unsafe { enif_alloc_env() }.is_null());
    }

    /// Nested envs: the innermost host is current, and dropping it restores the enclosing one.
    #[test]
    fn nested_envs_restore_the_enclosing_host() {
        let outer_want = sentinel(0x1111_2222);
        let inner_want = sentinel(0x3333_4444);
        let mut outer_host = AllocEnvHost::new(Some(outer_want));
        let mut inner_host = AllocEnvHost::new(Some(inner_want));

        let outer_env = NifEnv::new(&mut outer_host);
        // SAFETY: `outer_env` is live.
        assert_eq!(unsafe { enif_alloc_env() }, outer_want);
        let inner_env = NifEnv::new(&mut inner_host);
        // SAFETY: both envs are live; the innermost one is current.
        assert_eq!(unsafe { enif_alloc_env() }, inner_want);
        drop(inner_env);
        // SAFETY: `outer_env` is live again on the top of the stack.
        assert_eq!(unsafe { enif_alloc_env() }, outer_want);
        drop(outer_env);
        // SAFETY: no env is live on this thread.
        assert!(unsafe { enif_alloc_env() }.is_null());
    }

    /// An out-of-order drop removes exactly the dropping env's own entry — the live env keeps its
    /// host current, and the stack still empties (the "no live env ⇒ `NULL`" invariant).
    #[test]
    fn an_out_of_order_drop_removes_only_its_own_entry() {
        let first_want = sentinel(0x5555_6666);
        let second_want = sentinel(0x7777_8888);
        let mut first_host = AllocEnvHost::new(Some(first_want));
        let mut second_host = AllocEnvHost::new(Some(second_want));

        let first_env = NifEnv::new(&mut first_host);
        let second_env = NifEnv::new(&mut second_host);
        // SAFETY: both envs are live; the innermost one is current.
        assert_eq!(unsafe { enif_alloc_env() }, second_want);
        drop(first_env);
        // SAFETY: `second_env` is still live, so its host stays current.
        assert_eq!(
            unsafe { enif_alloc_env() },
            second_want,
            "the surviving env keeps its host current"
        );
        drop(second_env);
        // SAFETY: no env is live on this thread, so the stack really did empty.
        assert!(unsafe { enif_alloc_env() }.is_null());
    }
    /// `enif_make_copy` of a non-term (`THE_NON_VALUE`, a bare header word) is `THE_NON_VALUE`.
    #[test]
    fn make_copy_refuses_a_non_term() {
        // SAFETY: a null dst env is explicitly allowed by the contract, and the source word is
        // rejected before any env is touched.
        let out = unsafe { enif_make_copy(core::ptr::null_mut(), THE_NON_VALUE) };
        assert_eq!(out.raw(), 0);
        // SAFETY: as above.
        let out = unsafe { enif_make_copy(core::ptr::null_mut(), ERL_NIF_TERM::from_raw(0x08)) };
        assert_eq!(out.raw(), 0, "a bare header word is not a term");
    }

    /// A null env answers `NULL` (the C faults in `env->mod_nif->priv_data`).
    #[test]
    fn priv_data_of_a_null_env_is_null() {
        // SAFETY: a null env is explicitly allowed by the contract.
        assert!(unsafe { enif_priv_data(core::ptr::null_mut()) }.is_null());
    }

    /// A null env is a no-op (the C faults in `enif_clear_env`).
    #[test]
    fn free_env_of_a_null_env_does_nothing() {
        // SAFETY: a null env is explicitly allowed by the contract.
        unsafe { enif_free_env(core::ptr::null_mut()) };
    }

    /// The host seam is really reached: the default host answers "no private data" (a null pointer)
    /// and "no copy" (`THE_NON_VALUE`), and the exception/pending state of the env is untouched.
    #[test]
    fn the_host_seam_answers_the_defaults() {
        use beam_alloc::Heap;
        use beam_term::atom::AtomTable;

        /// A minimal host: a bump heap + an atom table (the crate's test convention).
        struct TestHost {
            heap: Heap,
            atoms: AtomTable,
        }

        impl NifHost for TestHost {
            fn heap(&mut self) -> &mut Heap {
                &mut self.heap
            }

            fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
                Some(beam_term::prelude::make_atom(self.atoms.intern(name)))
            }

            fn atom_name(&mut self, atom: Eterm) -> Option<String> {
                if !beam_term::prelude::is_atom(atom) {
                    return None;
                }
                self.atoms
                    .name(beam_term::prelude::atom_val(atom))
                    .map(str::to_string)
            }
        }

        let mut host = TestHost {
            heap: Heap::new(256).expect("a small test heap is allocatable"),
            atoms: AtomTable::new(),
        };
        let mut env = NifEnv::new(&mut host);
        // The default hooks: no private data, no copy engine.
        // SAFETY: `env` is a live env.
        assert!(unsafe { enif_priv_data(env.as_ptr()) }.is_null());
        // SAFETY: `env` is a live env and `make_small(7)` is a term.
        let copied = unsafe { enif_make_copy(env.as_ptr(), n(make_small(7))) };
        assert_eq!(copied.raw(), 0, "no copy engine => THE_NON_VALUE");
        assert!(
            env.pending_exception().is_none(),
            "a failed copy raises nothing"
        );
        // A `free_env` the host does not recognise is a no-op (the env is still usable).
        // SAFETY: `env` is a live env.
        unsafe { enif_free_env(env.as_ptr()) };
        assert!(is_small(make_small(7)));
        let atom = env.host().intern_atom("after_free").expect("interns");
        assert!(beam_term::prelude::is_atom(atom), "the env still works");
    }
}
