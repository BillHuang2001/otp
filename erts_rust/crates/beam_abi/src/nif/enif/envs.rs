//! `enif_*` — the extra-environment family: `enif_free_env`, `enif_make_copy`, `enif_priv_data`.
//!
//! Three of the four slots this family was reserved for are populated; `enif_alloc_env` stays `None`
//! for a structural reason documented under "Why `enif_alloc_env` stays `None`" below. (`enif_set_option`
//! is C-variadic — stable Rust cannot give a `...` parameter a body — see [`super`].)
//!
//! # Ground truth (`erts/emulator/beam/erl_nif.c`)
//!
//! | function | C body | reproduced here |
//! |---|---|---|
//! | `enif_free_env` | `enif_clear_env(env); erts_free(ERTS_ALC_T_NIF, env);` (`:676-680`) | delegates to [`NifHost::free_env`] — the host owns every env it allocated ([`NifHost::alloc_env`]) |
//! | `enif_make_copy` | `make_copy(dst_env, src_term, NULL)` — `size_object` + `copy_struct` onto `dst_env`'s heap (`:1194-1197`, `:843-858`) | delegates to [`NifHost::make_copy`]; when the hook is unsupported it answers `THE_NON_VALUE` |
//! | `enif_priv_data` | `env->mod_nif->priv_data` (`:609-612`) | delegates to [`NifHost::priv_data`] — the pointer the library's `load` callback produced (the loader keeps it in [`crate::nif::loader::NifLibrary::priv_data`]) |
//!
//! # Why `enif_alloc_env` stays `None`
//!
//! The C allocates a **process-independent** environment (`enif_alloc_env`, `:669-675`) and the
//! env's term heap *and* its atom table are the runtime's: atoms are global handles in ERTS, so an
//! `enif_alloc_env` env interns into the very same table `enif_make_atom` uses on the call env.
//!
//! In this crate both of those live behind [`NifHost`], and `enif_alloc_env(void)` **takes no
//! argument** — there is no env (and no other handle) to reach a host through, and the crate has no
//! host registry to fall back on. Two ways to give the slot a body were considered and both are
//! refused:
//!
//! * **Delegate to [`NifHost::alloc_env`]** — the hook exists but is unreachable from here: it needs
//!   `&mut self`, i.e. a host, which is exactly what this entry point cannot name. (The hook remains
//!   the right seam for the runtime: it is documented as owning the envs it allocates, and
//!   [`enif_free_env`] below hands them back to it.)
//! * **Allocate an env here** (a crate-owned `NifEnv` over a crate-owned heap, registered so that
//!   `enif_free_env` can drop it) — sound for `from_opaque` (the pointer really is a `NifEnv`, so the
//!   recovery cast stays valid, which is the hazard the design must avoid), but **not** atom-correct:
//!   the host behind such an env would have no access to the runtime's atom table, so
//!   `enif_make_atom(msg_env, "ok")` would fall into the crate's no-atom-support answer and a NIF
//!   would silently get `[]` where the oracle returns the atom `ok` — a *wrong term* reaching another
//!   process. A heap-owning env whose atoms are unusable is worth less than an honest `None`.
//!
//! What the slot needs is therefore a **host-discovery** mechanism, not another `enif_*` body: a
//! crate-level registry (or a thread-local "current call host") set by the runtime around each NIF
//! call, so that `enif_alloc_env` can call [`NifHost::alloc_env`] on the host the runtime prepared
//! the terms of this call with. Adding such state is a change to [`super::super::host`] and to the
//! runtime's call protocol (`beam_rt`), i.e. outside this family — reported rather than invented
//! here. Until then a NIF that *calls* `enif_alloc_env` fails **loudly** at `dlopen(RTLD_NOW)` with
//! `undefined symbol: enif_alloc_env` instead of loading and then silently mis-naming atoms.
//!
//! # Documented substitutions — never a wrong term, never a crash
//!
//! * A **`NULL` `env`** is answered with `THE_NON_VALUE` (`enif_make_copy`) / `NULL`
//!   (`enif_priv_data`) / a no-op (`enif_free_env`) where the C dereferences it (`:1196`, `:611`,
//!   `:678`) and faults.
//! * **`enif_free_env` frees nothing it does not own.** The C frees the pointer unconditionally
//!   (`erts_free`, `:679`) because it allocated it in `enif_alloc_env`; here the pointer is handed to
//!   [`NifHost::free_env`], which frees it when the host owns it and reports `false` otherwise (a
//!   per-call env, or a host with no envs of its own) — the pointer is then left alone rather than
//!   freed across allocators.
//! * **`enif_priv_data` on a message env** answers the host's value where the C would read
//!   `NULL->priv_data` and fault (`env->mod_nif` is `NULL` for an `enif_alloc_env` env,
//!   `setup_nif_env(msg_env, NULL, NULL)`, `:641`). A host without private data answers `NULL`; the
//!   C's answer for a *call* env with a `load` callback that stored nothing is `NULL` too.
//! * **`enif_make_copy` never aliases the source.** With no copy engine behind the hook the answer is
//!   `THE_NON_VALUE` (the C's own "no term" word, `erl_term.h`; the crate's `prelude::is_non_value`
//!   and every term predicate reject it), never `src_term` itself — returning the source term would
//!   hand out a term whose memory belongs to the *source* env.

use libc::c_void;

use super::*;

/// `THE_NON_VALUE` (`erts/emulator/beam/erl_term.h`: `#define THE_NON_VALUE 0`) — the C's "no term"
/// word. The C uses it for the same purpose in the term-producing operations that can fail without
/// raising (e.g. `enif_map_iterator_get_pair`'s out value, `erl_nif.c:3689`), and
/// `term == THE_NON_VALUE` is how a failed operation is detected (`enif_is_exception`, `:1290`).
const THE_NON_VALUE: ERL_NIF_TERM = ERL_NIF_TERM::from_raw(0);

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
///
/// `enif_alloc_env` is deliberately absent (see "Why `enif_alloc_env` stays `None`").
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_free_env = Some(enif_free_env);
    api.enif_make_copy = Some(enif_make_copy);
    api.enif_priv_data = Some(enif_priv_data);
    api
}

// ---------------------------------------------------------------------------------------------
// Extra environments and private data
// ---------------------------------------------------------------------------------------------

/// `void enif_free_env(ErlNifEnv* env);`
///
/// Hands the env back to the host that owns it ([`NifHost::free_env`]); a host that does not own it
/// (a per-call env, or no [`NifHost::alloc_env`] support) reports `false` and the pointer is left
/// alone — this crate never frees memory it did not allocate.
///
/// # Safety
///
/// `env` must be null, or a live env pointer produced by an env allocator (the C contract: a pointer
/// from `enif_alloc_env`); a null `env` is a no-op.
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
/// The private data pointer the library's `load` callback stored ([`NifHost::priv_data`]); a host
/// without private data answers `NULL` (as the C does for a call env whose `load` stored nothing).
///
/// # Safety
///
/// `env` must be null or a live env pointer.
#[no_mangle]
pub unsafe extern "C" fn enif_priv_data(
    env: *mut crate::nif::types::ErlNifEnv,
) -> *mut c_void {
    match unsafe { env_mut(env) } {
        Some(e) => e.host().priv_data(),
        // The C reads `env->mod_nif->priv_data` (`erl_nif.c:609-612`) and faults on a null env.
        None => core::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::nif::host::{NifEnv, NifHost};
    use beam_term::prelude::{is_small, make_small, Eterm};

    /// The slots this file populates are the three documented ones (and the fourth stays `None`).
    #[test]
    fn populate_fills_free_env_make_copy_and_priv_data_only() {
        let api = populate(ErlNifApi::empty());
        assert!(api.enif_free_env.is_some());
        assert!(api.enif_make_copy.is_some());
        assert!(api.enif_priv_data.is_some());
        assert!(api.enif_alloc_env.is_none());
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
