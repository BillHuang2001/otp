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

//! The **release-profile aliasing guard** for [`NifEnv`] (contract A.3).
//!
//! A NIF callback is C: it reaches the host through the [`NifEnv`] behind the `ErlNifEnv` it was
//! handed, while the Rust frame that started the callback is still on the stack. `super::host`'s
//! module docs (*The aliasing contract*) state the rule this suite enforces behaviourally: the
//! callback-visible state lives behind interior mutability, so a live `&AliasingHost` held by that
//! Rust frame cannot be `noalias readonly` and the callback's stores survive the optimiser.
//!
//! # Why an optimised run is the point
//!
//! The failure this guards is **silent, and invisible in an unoptimised build**: with a host whose
//! callback-visible state is a plain field, and a caller-side `&H` live across the call, LLVM legally
//! treats the write the callback makes through the environment as unobservable and answers the
//! caller's later read from the value it already had. Measured with the exact shape of [`session`]
//! below — a `#[inline(never)]` callback frame storing through the environment's pointer, an
//! `#[inline(never)]` caller frame reading through its own `&Host` — by swapping the shipped
//! `RefCell` field for a plain `Vec` and running both:
//!
//! | host shape | `opt-level = 0` | `--profile dev` (`opt-level = 1`) | `--release` |
//! |---|---|---|---|
//! | plain field (not interior-mutable) | `(1, true)` | `(0, false)` — store **lost** | `(0, false)` — store **lost** |
//! | `RefCell` field (the shipped shape) | `(1, true)` | `(1, true)` | `(1, true)` |
//!
//! (cells are the `session` result *after* the callback: names interned, `"published"` present.)
//! So an optimised run — `--release` **and** this workspace's `opt-level = 1` dev profile — is the
//! gate; a `--release` run is not optional for this file. Nothing here drives a real C fixture: the
//! hazard is a property of the Rust frames around the callback, and reproducing it needs no
//! `dlopen` (the `tests/nif_*` suites are the ones that build and load real C fixtures).

use core::cell::RefCell;

use super::*;
use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, make_atom};

use crate::nif::enif::{enif_has_pending_exception, enif_make_atom};
use crate::nif::term::{eterm_from_nif, ERL_NIF_TERM};

/// A host whose **callback-visible** state — the names it has interned — is behind a [`RefCell`],
/// which is what keeps it non-`Freeze` and therefore what keeps a store made through the env
/// observable to a caller holding `&AliasingHost`.
struct AliasingHost {
    heap: Heap,
    atoms: AtomTable,
    interned: RefCell<Vec<String>>,
}

impl AliasingHost {
    fn new() -> AliasingHost {
        AliasingHost {
            heap: Heap::new(256).expect("a small test heap is allocatable"),
            atoms: AtomTable::new(),
            interned: RefCell::new(Vec::new()),
        }
    }

    /// How many names a callback has interned so far — read through a **shared** borrow.
    fn interned_count(&self) -> usize {
        self.interned.borrow().len()
    }

    /// Whether `name` is among them — read through a **shared** borrow.
    fn interned(&self, name: &str) -> bool {
        self.interned.borrow().iter().any(|n| n == name)
    }
}

impl NifHost for AliasingHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// The store a callback performs: the atom goes into the table (private to the host) **and**
    /// the name is appended to the shared, interior-mutable record the caller reads back.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        let atom = self.atoms.intern(name);
        self.interned.borrow_mut().push(name.to_string());
        Some(make_atom(atom))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }
}

/// What a NIF callback publishes through the `ErlNifEnv` it was handed.
///
/// One real `enif_*` entry point, reached through the opaque environment pointer exactly as a C NIF
/// would reach it: `enif_make_atom` interns the name on the host (`NifHost::intern_atom`), so the
/// store lands in the host's callback-visible state. `#[inline(never)]` is load-bearing: it keeps
/// the store in a frame the caller's optimiser analyses as an opaque call, which is exactly the
/// C-callback situation.
#[inline(never)]
fn callback_publishes(env: &NifEnv) {
    // SAFETY: `env` is live for the whole test, and the name is a NUL-terminated literal.
    let atom = unsafe { enif_make_atom(env.as_ptr(), c"published".as_ptr()) };
    // The `badarg` a host that cannot intern would produce is irrelevant here: what the test reads
    // is the state the store wrote.
    assert!(
        is_atom(eterm_from_nif(atom)),
        "the callback interned the name through the host"
    );
}

/// The caller frame: `shared` (a **live shared borrow of the host**) is held across the call into
/// the NIF, and the callback's effect is read back through that same borrow.
#[inline(never)]
fn session(shared: &AliasingHost, env: &NifEnv) -> ((usize, bool), (usize, bool)) {
    let observe = || (shared.interned_count(), shared.interned("published"));
    let before = observe();
    callback_publishes(env);
    let after = observe();
    (before, after)
}

/// **The trap guard.** A callback's stores through the environment must be visible to a caller that
/// held `&AliasingHost` across the call, in `--release` as well as `debug`.
///
/// This is the assertion a release-only A.3 defect would fail: with a non-interior-mutable host the
/// `after` tuple comes back equal to `before` once optimised (see the table in this module's docs),
/// and the runtime then behaves as though the NIF never interned through the env at all — a silent
/// no-op, not a wrong value and not a crash.
#[test]
fn a_callback_store_through_the_env_survives_a_live_shared_borrow() {
    let mut host = AliasingHost::new();
    let env = NifEnv::new(&mut host);
    // The caller's own reference to the host, live across `session`'s call into the "NIF". This is
    // the borrow whose `noalias`/`dereferenceable` attributes the optimiser derives, and the reason
    // the host's callback-visible state must be interior-mutable.
    let shared: &AliasingHost = &host;

    let (before, after) = session(shared, &env);

    assert_eq!(
        before,
        (0, false),
        "a fresh host has interned nothing and recorded nothing"
    );
    assert_eq!(
        after,
        (1, true),
        "the callback's store through the environment was dropped: the host's callback-visible \
         state lost its interior mutability (see this module's docs, and `super::host`'s *The \
         aliasing contract*). This is an A.3 correctness regression, not a style change."
    );
}

/// **The structural guard.** Every piece of callback-visible state is reachable, and mutable,
/// through a **shared** borrow of the host.
///
/// A `Freeze` host cannot pass this test at all — the write below would need `&mut AliasingHost`,
/// which is exactly what a NIF callback's caller must not have. So a "cleanup" that turns the
/// `RefCell` back into a plain field fails to compile here, before the optimiser ever gets a chance
/// to drop a store.
#[test]
fn the_callback_visible_state_is_reachable_through_a_shared_borrow() {
    let host = AliasingHost::new();
    let shared: &AliasingHost = &host;

    // A write through the shared borrow — the shape `NifHost::intern_atom` has from inside a
    // callback (`self` is `&mut`, but the *caller's* view of the same object stays shared).
    shared.interned.borrow_mut().push("direct".to_string());

    assert_eq!(shared.interned_count(), 1);
    assert!(shared.interned("direct"));
    assert!(!shared.interned("published"));
}

/// The pending-exception slot is interior-mutable too: a raise stores through a **shared** env
/// reference, and the caller reads it back through the same one.
#[test]
fn a_raise_through_a_shared_env_reference_is_observable() {
    let mut host = AliasingHost::new();
    let env = NifEnv::new(&mut host);
    let shared: &NifEnv = &env;
    assert_eq!(shared.pending_exception(), None, "nothing raised yet");

    // A second view of the same env, exactly the shape a resource `stop`/`dtor` callback or a
    // scheduled-NIF continuation re-enters through.
    // SAFETY: `shared.as_ptr()` names `env`, which is live for the whole test.
    let reentered = unsafe { NifEnv::from_opaque(shared.as_ptr()) }.expect("non-null env");
    let reason = host
        .intern_atom("boom")
        .expect("this host interns unconditionally");
    reentered.set_exception(reason);

    assert_eq!(
        shared.pending_exception(),
        Some(reason),
        "the raise a re-entered view recorded is visible through the outer one"
    );
    // And the `enif_*` entry point agrees (it reads the same slot).
    // SAFETY: `shared.as_ptr()` names a live env; `clear` only resets the helper's own view.
    let mut got = ERL_NIF_TERM::default();
    // SAFETY: as above — `env` is live and `got` is a live local.
    assert_eq!(
        unsafe { enif_has_pending_exception(shared.as_ptr(), &mut got) },
        1
    );
}

/// `as_ptr`/`from_opaque` round-trip through a **shared** reference, and two views of one env may
/// be live at the same time (the case a `&mut NifEnv` return could not express).
#[test]
fn from_opaque_hands_back_shared_references() {
    let mut host = AliasingHost::new();
    let env = NifEnv::new(&mut host);
    let ptr = env.as_ptr();
    assert!(!ptr.is_null(), "the env pointer is never null");

    // SAFETY: `ptr` came from `as_ptr` on `env`, which is live for the whole test.
    let first = unsafe { NifEnv::from_opaque(ptr) }.expect("non-null pointer");
    // SAFETY: the same contract; two shared views of one env are sound, two exclusive ones are not.
    let second = unsafe { NifEnv::from_opaque(ptr) }.expect("non-null pointer");

    assert_eq!(first.pending_exception(), None);
    assert_eq!(second.pending_exception(), None);
    assert_eq!(
        first.as_ptr(),
        second.as_ptr(),
        "both views name the same env"
    );
    // A store through one view is visible through the other.
    first.set_exception(make_atom(AtomTable::new().intern("x")));
    assert_eq!(second.pending_exception(), first.pending_exception());
}

/// The sanctioned scoped form: `with_host` runs its closure with the host and the `&mut dyn
/// NifHost` never escapes.
#[test]
fn with_host_runs_its_closure_over_the_env_s_own_pointer() {
    let mut host = AliasingHost::new();
    let env = NifEnv::new(&mut host);
    let shared: &AliasingHost = &host;

    let atom = env.with_host(|h| h.intern_atom("scoped")).expect("interns");

    assert!(is_atom(atom));
    assert!(
        shared.interned("scoped"),
        "the closure reached the host through the env's pointer"
    );
    // `heap()` is the same route, one convenience down.
    assert!(env.heap().available() > 0, "the host heap is reachable");
}

/// The concrete-type recovery is available **only** where it is sound: an env built by `new_typed`
/// answers its own host's type, an env built by `new` (every borrowed per-call host — `beam_rt`'s
/// `NifHostImpl<'a>` cannot satisfy the `'static` bound the driver's `AsAnyHost` upcast needs)
/// answers `None` rather than guessing.
#[test]
fn host_as_recovers_the_concrete_host_only_when_it_was_captured() {
    let mut host = AliasingHost::new();
    let typed = NifEnv::new_typed(&mut host);
    let recovered = typed.host_as::<AliasingHost>().expect("captured at construction");
    let atom = recovered.intern_atom("typed").expect("interns");
    assert!(is_atom(atom));
    // The scoped form, and the same host object: the write is visible through the caller's own
    // `&AliasingHost`.
    let scoped = typed
        .with_host_as::<AliasingHost, _>(|h| h.intern_atom("typed_scoped"))
        .expect("captured at construction")
        .expect("this host interns unconditionally");
    assert!(is_atom(scoped));
    assert!(
        typed.host_as::<TestHostNotThisOne>().is_none(),
        "another host type never matches"
    );
    // A `new`-built env over a *different* host captures nothing either, so the wrong-type probe
    // above cannot be confused with a real recovery.
    let mut other = TestHostNotThisOne(Heap::new(256).expect("a small test heap is allocatable"));
    let other_env = NifEnv::new(&mut other);
    assert!(
        other_env.host_as::<TestHostNotThisOne>().is_none(),
        "`NifEnv::new` captures no concrete type, not even the right one"
    );
    drop(other_env);
    drop(typed);

    // A wrong-type probe must answer `None`, and a plain `new` env captures nothing at all.
    let shared = NifEnv::new(&mut host);
    assert!(
        shared.host_as::<AliasingHost>().is_none(),
        "`NifEnv::new` captures no concrete type (a borrowed per-call host could not be `'static`)"
    );
    assert_eq!(host.interned_count(), 2);
    drop(shared);
    assert!(host.interned("typed") && host.interned("typed_scoped"));
}

/// A second `NifHost` type, so the `host_as` downcast has a negative case.
struct TestHostNotThisOne(Heap);

impl NifHost for TestHostNotThisOne {
    fn heap(&mut self) -> &mut Heap {
        &mut self.0
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(AtomTable::new().intern(name)))
    }

    fn atom_name(&mut self, _atom: Eterm) -> Option<String> {
        None
    }
}
