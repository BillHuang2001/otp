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

//! End-to-end proof of the NIF **resource family** (workspace contract **A.3**): a real C NIF shared
//! object — `tests/fixtures/nif_resource.c`, compiled with the system `cc` against the system
//! `erl_nif.h` — is `dlopen`ed and driven through `beam_abi`'s own loader, and its observable
//! behaviour is asserted from Rust.
//!
//! # Why this is load-bearing
//!
//! The fixture's `enif_open_resource_type` / `enif_alloc_resource` / `enif_make_resource` … calls are
//! left **undefined** in the `.so`; the loading test binary binds them out of its dynamic symbol
//! table (`build.rs` emits `-Wl,--export-dynamic` for the test targets, and
//! `enif_api_table()` is touched below to keep the `enif_*`-carrying codegen unit in the link). So a
//! successful `dlopen(RTLD_NOW)` proves the eleven resource slots really exist as exported symbols,
//! and the assertions prove they behave.
//!
//! # Module identity, keys and liveness (documented substitutions)
//!
//! The resource-term representation is an internal reference minted by this crate and recorded in a
//! process-global registry (`src/nif/enif/resources.rs` docs). Consequences visible in this test —
//! and asserted as such, with the oracle's value called out:
//!
//! * a created type is visible immediately, so re-opening an existing name reports
//!   `ERL_NIF_RT_TAKEOVER` (the `*tried` value agrees with the oracle; the oracle additionally
//!   refuses any open outside a `load` callback, returning `NULL`);
//! * the type is keyed by *name* only, so `enif_dynamic_resource_call` ignores the module atom;
//! * a resource TERM holds its own reference (exactly the C magic ref's reference), so a released
//!   object stays alive while the term is observable and `enif_get_resource` keeps resolving it —
//!   the oracle's behaviour (`post_release/0`). **The deviation:** this crate has no GC, so the
//!   term's reference is never dropped — an object ever exported as a term is retained for the
//!   process lifetime and its destructor never runs (the C reclaims it when the term is collected);
//! * the destructor runs **synchronously** here; the C schedules it on a misc aux-work thread, so an
//!   in-call destructor count is `1` here and `0` on the oracle. The count and ordering match; only
//!   the timing differs. The count is pinned (Rust-side only) by `dtor_probe/0` + `dtor_state/0`,
//!   which the oracle cannot run at all: the C dereferences the `NULL` keep/release in that probe
//!   and segfaults.
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixture needs a C compiler and the system `erl_nif.h`. When either is missing the
//! test prints a loud `SKIP:` banner (via `compile_so`) and returns; when the toolchain is present,
//! every assertion is hard.

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use beam_abi::nif::enif::enif_api_table;
use beam_abi::nif::enif::resources::enif_get_resource;
use beam_abi::nif::loader::load;
use beam_abi::nif::term::nif_from_eterm;
use beam_abi::NifEnv;
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    self, atom_val, is_atom, make_atom, make_boxed, make_header, make_internal_pid, make_pid_data,
    make_small, Eterm, NIL,
};
use beam_term::REF_SUBTAG;
use libc::c_void;

use common::{compile_so, skip};

/// The committed C fixture (a real NIF module; see the file header for the A.3 rationale).
const FIXTURE_C: &str = include_str!("fixtures/nif_resource.c");

// ---------------------------------------------------------------------------------------------
// A `NifHost` that can mint resource terms (`make_ref`)
// ---------------------------------------------------------------------------------------------

/// A test host: a bump heap, an atom table, a process identity, and a liveness answer.
///
/// `tests/common/mod.rs::TestHost` is deliberately minimal (no `self_pid`, no `is_process_alive`), so
/// the resource test supplies its own host here. The resource *term* no longer comes from this host —
/// `enif_make_resource` mints it in `src/nif/enif/resources.rs` (the `make_ref` hook below is what
/// `enif_make_ref` alone would use, and is kept so the host is a faithful stand-in).
///
/// `live` is shared with the test because the host is mutably borrowed by the [`NifEnv`] for the
/// whole test, so the test cannot set it on the host directly.
struct ResourceHost {
    heap: Heap,
    atoms: AtomTable,
    next_ref: usize,
    /// The pid `NifHost::self_pid` reports (the calling process).
    self_pid: Eterm,
    /// The pids `NifHost::is_process_alive` reports as alive (the "other" processes).
    live: Rc<RefCell<Vec<Eterm>>>,
}

impl ResourceHost {
    /// A host with a process identity and no live "other" processes.
    fn new() -> ResourceHost {
        ResourceHost::with_processes(Vec::new())
    }

    /// A host whose calling process is an internal pid and which reports `live` as alive.
    fn with_processes(live: Vec<Eterm>) -> ResourceHost {
        ResourceHost {
            heap: Heap::new(4096).expect("a small test heap is allocatable"),
            atoms: AtomTable::new(),
            next_ref: 0,
            self_pid: make_internal_pid(make_pid_data(0, 1)),
            live: Rc::new(RefCell::new(live)),
        }
    }
}

impl beam_abi::NifHost for ResourceHost {
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

    /// The calling process — what `enif_self` and the self case of `enif_is_process_alive` need.
    fn self_pid(&mut self) -> Option<Eterm> {
        Some(self.self_pid)
    }

    /// Mint a fresh internal reference (what `enif_make_ref` uses).
    fn make_ref(&mut self) -> Option<Eterm> {
        let id = self.next_ref;
        self.next_ref += 1;
        // SAFETY: `alloc_words` hands out four exclusively-owned, writable words.
        let p = unsafe { self.heap.alloc_words(4) }.ok()?;
        // SAFETY: `p` addresses the four words just handed out; every one is written before the
        // pointer escapes this block.
        unsafe {
            *p = make_header(3, REF_SUBTAG);
            *p.add(1) = make_small(0); // node: the local node
            *p.add(2) = make_small(0); // serial
            *p.add(3) = make_small(id as i64); // a distinct id per call
        }
        Some(make_boxed(p as *const Eterm))
    }

    /// The only answer this host can give for an *other* process.
    fn is_process_alive(&mut self, pid: Eterm) -> Option<bool> {
        Some(self.live.borrow().contains(&pid))
    }
}

// ---------------------------------------------------------------------------------------------
// Decoding a result tuple of small integers
// ---------------------------------------------------------------------------------------------

/// The integer value of a small-int term.
fn as_int(t: Eterm) -> i64 {
    assert!(
        prelude::is_small(t),
        "every element of a fixture result tuple is a small integer; got raw {:#x}",
        t.raw()
    );
    prelude::signed_value(t)
}

/// Decode a tuple of small integers into a `Vec<i64>`.
fn tuple_ints(t: Eterm) -> Vec<i64> {
    // SAFETY: `t` is a well-formed tuple produced by the fixture.
    let n = unsafe { prelude::tuple_size(t) };
    (0..n)
        .map(|i| {
            // SAFETY: `i < n == tuple_size(t)`, so the element is in bounds.
            as_int(unsafe { prelude::tuple_element(t, i) })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so("nif_resource", FIXTURE_C)
}

// ---------------------------------------------------------------------------------------------
// A cc-independent unit check of the resource term contract
// ---------------------------------------------------------------------------------------------

/// `enif_get_resource` with a `NULL` type never matches — and never dereferences the term.
#[test]
fn a_null_resource_type_never_matches() {
    let mut host = ResourceHost::new();
    let mut env = NifEnv::new(&mut host);
    let mut objp: *mut c_void = std::ptr::null_mut();
    let term = nif_from_eterm(make_small(1));
    // SAFETY: `env` is live; a null `type` is explicitly handled by the implementation; `objp` is a
    // live local.
    let matched = unsafe { enif_get_resource(env.as_ptr(), term, std::ptr::null_mut(), &mut objp) };
    assert_eq!(matched, 0);
    assert!(objp.is_null(), "*objp must be left untouched");
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load a real NIF `.so` and drive its resource family
// ---------------------------------------------------------------------------------------------

#[test]
fn resource_family_end_to_end() {
    // Touching the table keeps the `enif_*`-carrying codegen unit in the link, so the `.so`'s
    // imports resolve and `dlopen(RTLD_NOW)` can succeed. (No slot is asserted on: the table grows
    // as other families land.)
    std::hint::black_box(enif_api_table());

    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    let mut host = ResourceHost::new();
    // The host outlives the library (drop order), which is what the library's `unload` callback and
    // the types' captured host pointer require.
    let lib = load(&path, "nif_resource", &mut host, NIL)
        .expect("a real NIF library must load: its enif_* imports resolve from the test binary");
    let mut env = NifEnv::new(&mut host);

    // --- the three open functions and the `*tried` contract -------------------------------
    let status = lib.call(&mut env, "load_status", 0, &[]).expect("load_status/0");
    assert_eq!(
        tuple_ints(status),
        vec![
            1, /* tried_main          == ERL_NIF_RT_CREATE  */
            2, /* tried_absent        == ERL_NIF_RT_TAKEOVER */
            1, /* absent open returned NULL */
            1, /* tried_x             == ERL_NIF_RT_CREATE  */
            1, /* tried_dy            == ERL_NIF_RT_CREATE  */
            1, 1, 1, /* the three types are non-NULL */
        ]
    );

    // Re-opening an existing name with TAKEOVER only: `*tried` agrees with the oracle; the
    // non-NULL-ness does not (there is no load-session boundary here — see the module docs).
    let reopened = lib.call(&mut env, "reopen_main", 0, &[]).expect("reopen_main/0");
    assert_eq!(tuple_ints(reopened), vec![1, 2]);

    // Re-opening an existing name with CREATE only: not taken over, so NULL and `*tried == flags`.
    // This agrees with the oracle.
    let created = lib
        .call(&mut env, "create_only_existing", 0, &[])
        .expect("create_only_existing/0");
    assert_eq!(tuple_ints(created), vec![0, 1]);

    // --- allocate / size / term / inspect / keep / release / dtor -------------------------
    // Oracle value (measured against OTP 29.1.1 / erts-17.1): {13,1,1,1,1,0,0,-1,1,1,0,0,0,1,9,0}.
    // The final element is the one documented timing divergence: the oracle's `x_dtor_runs` is `0`
    // because the C only *schedules* the destructor off-thread; this runtime runs it synchronously.
    let rt = lib.call(&mut env, "roundtrip", 0, &[]).expect("roundtrip/0");
    assert_eq!(
        tuple_ints(rt),
        vec![
            13, /* enif_sizeof_resource == the payload size                     */
            1,  /* enif_get_resource wrote the same payload pointer             */
            1,  /* ... *objp untouched on a type mismatch                       */
            1,  /* the resource-binary term also resolves to the payload        */
            1,  /* ... and its bytes are the four given bytes                   */
            0,  /* keep/release balanced: the destructor has NOT run            */
            0,  /* the final release does NOT reach zero (the terms hold refs)  */
            -1, /* ... so no destructor ran: the recorded byte is untouched      */
            1,  /* the term still resolves — it holds the object's reference     */
            1, 0, 0, 0, 1, /* r_same, r_wrongtype, r_int, r_binary, r_resbin    */
            9,  /* a type carrying a `down` callback also reports its payload    */
            1,  /* res_x's (unexported) object ran its destructor synchronously */
        ]
    );

    // `enif_make_resource` + `enif_get_resource` round-trip the payload a NIF wrote.
    // Oracle: {1,42,0,1,0}.
    let rtv = lib
        .call(&mut env, "roundtrip_value", 0, &[])
        .expect("roundtrip_value/0");
    assert_eq!(tuple_ints(rtv), vec![1, 42, 0, 1, 0]);

    // --- the A.3 divergence: a resource TERM keeps its object alive across a release -------
    // alloc -> enif_make_resource -> enif_release_resource -> enif_get_resource
    // Oracle: {1,42,0} — the term holds its own reference, so the release only drops the
    // allocator's, the payload stays readable and no destructor runs.
    let post = lib
        .call(&mut env, "post_release", 0, &[])
        .expect("post_release/0");
    assert_eq!(
        tuple_ints(post),
        vec![
            1,  /* enif_get_resource still resolves the released term        */
            42, /* ... and the int the NIF wrote reads back intact            */
            0,  /* ... with no destructor run (the term's reference remains)  */
        ]
    );

    // --- enif_dynamic_resource_call -------------------------------------------------------
    let dynr = lib.call(&mut env, "dyn_call", 0, &[]).expect("dyn_call/0");
    assert_eq!(
        tuple_ints(dynr),
        vec![
            0,    /* the call was made                                            */
            4242, /* ... and the dyncall wrote through call_data                  */
            1,    /* unknown type name                                            */
            1,    /* a non-resource term                                          */
            1,    /* matching name but the type has no dyncall                    */
            1,    /* a dyncall-bearing name but a resource of another type        */
            0,    /* the module atom is ignored (documented substitution); the
                   * oracle returns non-zero for a module mismatch             */
            2,    /* dyncall ran for the two matching calls                       */
        ]
    );

    // --- the destructor for an UNEXPORTED object (Rust-only, documented) ------------------
    // Each object here is released to zero without ever being made into a term, so the destructor
    // runs on the last release. This runtime runs it synchronously, so by the time
    // `dtor_state/0` runs the count is complete: two runs, in probe order (`0x11` then `0x22`),
    // with the last payload byte `0x22`. (The oracle cannot run this probe: its C dereferences the
    // `NULL` keep/release and segfaults — see the fixture comment.)
    let probe = lib.call(&mut env, "dtor_probe", 0, &[]).expect("dtor_probe/0");
    assert_eq!(tuple_ints(probe), vec![1, 2]);
    let state = lib.call(&mut env, "dtor_state", 0, &[]).expect("dtor_state/0");
    assert_eq!(
        tuple_ints(state),
        vec![
            2,      /* both destructors ran                                       */
            0x1122, /* in release order: the 0x11 object before the 0x22 one       */
            0x22,   /* the most recent payload byte                                */
        ]
    );
}
