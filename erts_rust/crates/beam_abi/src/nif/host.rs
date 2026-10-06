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

use beam_alloc::Heap;
use beam_term::prelude::Eterm;

use crate::nif::types::ErlNifEnv;

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
    /// The exception reason recorded by a raise, if any (the C `enif_raise_exception` slot).
    pending_exception: Option<Eterm>,
}

impl NifEnv {
    /// Wrap `host` in a fresh per-call environment (no pending exception).
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
        NifEnv {
            host,
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
