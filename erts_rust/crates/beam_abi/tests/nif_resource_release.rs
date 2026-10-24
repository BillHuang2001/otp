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

//! End-to-end proof of the **resource-term release seam** — `release_resource_terms/1`
//! (`src/nif/enif/resources_release.rs`) against the C's own reference-count semantics
//! (contract A.3).
//!
//! `tests/fixtures/nif_resource_release.c` is a real C NIF — compiled at test time with the system
//! `cc` against the system `erl_nif.h` — with three entry points:
//!
//! * `probe/0` — `enif_alloc_resource` + write a payload byte + `enif_make_resource` +
//!   `enif_release_resource`, i.e. an object whose **only** remaining reference is the term's;
//! * `probe_unexported/0` — the same minus `enif_make_resource`, so the allocator's reference is the
//!   last one and the release destroys the object;
//! * `dtor_count/0` — how many times the type's destructor has run.
//!
//! Ground truth `erts/emulator/beam/erl_nif.c`: `enif_make_resource` (`:3197`) builds an
//! `ERTS_MAGIC_REF` that owns one reference on the object (`erts_bld_resource_ref` →
//! `erts_mk_magic_ref` → `erts_bin_inc_refc`), and the term GC drops that reference when the term
//! becomes unreachable — the release that reaches zero runs the type's `dtor` (`:2961-3030`).
//!
//! `beam_abi` has **no GC**, so the "the term died" signal is a caller-owned seam: the caller hands
//! `release_resource_terms` its reachability predicate, and the seam drops exactly one reference per
//! term it is told is unreachable (idempotently, per term word). This file pins that the counts it
//! produces are the C's.
//!
//! | step | oracle | this crate |
//! |---|---|---|
//! | fresh library | `0` | `0` |
//! | `probe/0`, term still held | `0` | `0` |
//! | every term called reachable ⇒ nothing released | `0` | `0` |
//! | the term called unreachable ⇒ the term's reference dropped | `+1` | `+1` |
//! | the same term pruned again | `+0` (never twice) | `+0` |
//! | `probe_unexported/0` (never a term) | `+1` | `+1` |
//!
//! **The oracle needs a `garbage_collect/0` step the seam replaces.** Its destructor is scheduled as
//! misc aux work, so the oracle half must force a collection *and* settle the deferred job; the Rust
//! half states the reachability decision explicitly and the destructor then runs synchronously. Only
//! **counts** are therefore comparable — never timing (see
//! `the_oracle_needs_a_collection_the_seam_replaces`).
//!
//! # Predicate shape (why it is `!=` and not `==`)
//!
//! The resource registry is a **process-wide static inside this test binary**, so `resource_terms()`
//! also reports terms minted by a concurrently running test. The predicate must therefore answer
//! `true` ("reachable") for every term but the one under test:
//! `release_resource_terms(|candidate| candidate.raw() != target.raw())`. Inverting it would call
//! every *other* live term unreachable and release those instead.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing (the calling test then returns). The
//! oracle half skips loudly when `erl`/`erlc` are absent. Whenever the toolchain **is** present every
//! assertion is hard.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::enif::{
    enif_get_resource, enif_open_resource_type, release_resource_terms, resource_terms,
};
use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::nif::types::{ErlNifResourceFlags, ErlNifResourceType};
use beam_abi::{nif_from_eterm, NifEnv, NifHost};
use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, is_small, make_atom, signed_value, Eterm};

use common::{compile_so, skip};

/// The committed C fixture (see its header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_resource_release.c");

/// The fixture's `ERL_NIF_INIT` name — the loader *and* the oracle require it to equal the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const MODULE: &str = "nif_resource_release";

/// The name the fixture's `load` callback opens its resource type under (see the C body).
const TYPE_NAME: &str = "release_type";

/// The measured oracle answer of the oracle module's `run/0` (OTP 29.1.1 / erts-17.1):
/// `{fresh, dropped, kept-reachable, erased, never-exported}`.
const ORACLE_RUN: &str = "{0,1,1,2,3}";

// ---------------------------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------------------------

/// A minimal [`NifHost`]: a bump heap plus an atom table.
///
/// The fixture's `load` callback opens the resource type through this host, so an env built from it
/// is also what the type's destructor receives (the crate builds one from the type's host).
struct ResourceHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
}

impl ResourceHost {
    fn new() -> ResourceHost {
        ResourceHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
        }
    }
}

impl NifHost for ResourceHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
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

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The fixture compiled into a shared object, or `None` (after a loud skip) when the toolchain is
/// absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(MODULE, FIXTURE_C)
}

/// The path `load` takes — the compiled `.so` **without** its suffix, which the loader appends
/// itself — or `None` (after a loud skip) when it is not valid UTF-8.
fn load_path(so: &Path) -> Option<String> {
    match so.with_extension("").to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// Call a zero-arity fixture entry that answers a small integer.
#[track_caller]
fn call_int(lib: &NifLibrary, host: &mut ResourceHost, name: &str) -> i64 {
    let env = NifEnv::new(host);
    let term = lib
        .call(&env, name, 0, &[])
        .unwrap_or_else(|err| panic!("{name}/0 must be callable: {err:?}"));
    assert!(is_small(term), "{name}/0 must answer a small integer");
    signed_value(term)
}

/// The live resource type the fixture opened, reopened **by name** with `TAKEOVER` so the term can be
/// resolved with `enif_get_resource`.
///
/// The crate's registry is keyed by name and returns the existing type for a `TAKEOVER`, so the
/// handle (and its destructor) is the fixture's own — the call only refreshes the host the destructor
/// environment is built from.
#[track_caller]
fn reopen_type(env: &NifEnv) -> *mut ErlNifResourceType {
    let name = std::ffi::CString::new(TYPE_NAME).expect("the type name has no interior NUL");
    let mut tried = ErlNifResourceFlags::from_bits(0);
    // SAFETY: `env` is live, `name` is NUL-terminated and `tried` is a live local.
    let ty = unsafe {
        enif_open_resource_type(
            env.as_ptr(),
            std::ptr::null(),
            name.as_ptr(),
            None,
            ErlNifResourceFlags::TAKEOVER,
            &mut tried,
        )
    };
    assert!(!ty.is_null(), "the fixture's type must be reopened");
    assert_eq!(
        tried,
        ErlNifResourceFlags::TAKEOVER,
        "the type already existed, so the C reports a takeover"
    );
    ty
}

/// `enif_get_resource(env, term, ty, &mut objp)` as the C's `1`/`0`.
///
/// # Safety
///
/// `env` must be live and `ty` a type this crate opened.
#[track_caller]
unsafe fn get_resource(env: &NifEnv, term: Eterm, ty: *mut ErlNifResourceType) -> i32 {
    let mut objp: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: forwarded from the caller's contract; `objp` is a live local.
    unsafe { enif_get_resource(env.as_ptr(), nif_from_eterm(term), ty, &mut objp) }
}

// ---------------------------------------------------------------------------------------------
// The Rust runtime
// ---------------------------------------------------------------------------------------------

/// **The whole contract.** Every count the seam produces is the C's, and the prune is idempotent per
/// term word.
#[test]
fn the_seam_reproduces_the_cs_reference_counts() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = ResourceHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");

    // (0) Fresh library: the `load` callback opened the type, no object exists yet.
    assert_eq!(call_int(&lib, &mut host, "dtor_count"), 0);

    // (1) `probe/0` mints a term that holds the object's ONLY remaining reference.
    let target = {
        let env = NifEnv::new(&mut host);
        lib.call(&env, "probe", 0, &[])
            .expect("probe/0 must be callable")
    };
    assert_eq!(
        call_int(&lib, &mut host, "dtor_count"),
        0,
        "the term still holds the object, so no destructor may run"
    );

    // (2) Every live term is called reachable: nothing is released, nothing is destroyed, and the
    //     term still resolves to its payload.
    assert_eq!(
        release_resource_terms(|_| true),
        0,
        "no term is called unreachable, so nothing is released"
    );
    assert_eq!(
        call_int(&lib, &mut host, "dtor_count"),
        0,
        "and no destructor runs"
    );
    {
        let env = NifEnv::new(&mut host);
        let ty = reopen_type(&env);
        // SAFETY: `env` is live and `ty` is the fixture's own type.
        assert_eq!(
            unsafe { get_resource(&env, target, ty) },
            1,
            "a reachable term still resolves through enif_get_resource"
        );
    }

    // (3) The same term, now called unreachable: exactly one reference is dropped and the destructor
    //     runs because that was the last one.
    assert_eq!(
        release_resource_terms(|candidate| candidate.raw() != target.raw()),
        1,
        "only the term under test is called unreachable"
    );
    assert_eq!(
        call_int(&lib, &mut host, "dtor_count"),
        1,
        "the term's reference was the last one, so the destructor ran"
    );
    {
        let env = NifEnv::new(&mut host);
        let ty = reopen_type(&env);
        // SAFETY: `env` is live and `ty` is the fixture's own type; the term's binding was removed,
        // so this only misses in the registry and never reads freed memory.
        assert_eq!(
            unsafe { get_resource(&env, target, ty) },
            0,
            "a released term no longer resolves (the binding removal is the guard)"
        );
    }
    assert!(
        !resource_terms().contains(&target),
        "and it left the seam's own live-term snapshot"
    );

    // (4) Idempotent: the binding is gone, so a second prune releases nothing and the destructor runs
    //     exactly once in total.
    assert_eq!(
        release_resource_terms(|candidate| candidate.raw() != target.raw()),
        0,
        "a released term is never released twice"
    );
    assert_eq!(
        call_int(&lib, &mut host, "dtor_count"),
        1,
        "the destructor never runs twice"
    );

    // (5) An object that was NEVER exported as a term has only the allocator's reference, so the
    //     fixture's own `enif_release_resource` destroys it — no prune involved.
    assert_eq!(call_int(&lib, &mut host, "probe_unexported"), 1);
    assert_eq!(
        call_int(&lib, &mut host, "dtor_count"),
        2,
        "the never-exported object was destroyed on its last release"
    );
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Whether `cmd` can be spawned with `args` at all (the probe for the oracle toolchain; a non-zero
/// exit is fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// The Erlang module the oracle test generates: the NIF stubs plus `run/0`, which walks the same five
/// steps as the Rust test — with the collection the C needs and the seam does not.
fn oracle_module(nif_path: &str) -> String {
    format!(
        r#"-module({MODULE}).
-export([dtor_count/0, probe/0, probe_unexported/0, run/0]).
-on_load(init/0).

init() ->
    erlang:load_nif("{nif_path}", 0).

dtor_count() -> erlang:nif_error(nif_not_loaded).
probe() -> erlang:nif_error(nif_not_loaded).
probe_unexported() -> erlang:nif_error(nif_not_loaded).

%% The five steps, in the Rust test's order. The C's destructor is misc aux work, so every count is
%% taken after `settle/0` has forced a collection AND waited for the deferred job -- the step the
%% Rust seam replaces with an explicit reachability predicate.
run() ->
    C0 = dtor_count(),
    C1 = drop_step(),
    C2 = keep_step(),
    C3 = settle(),
    _ = probe_unexported(),
    C4 = settle(),
    {{C0, C1, C2, C3, C4}}.

%% The term is minted and immediately discarded: one object, one destructor.
drop_step() ->
    _ = probe(),
    settle().

%% The term is held across a collection, so no destructor may run while it is live.
keep_step() ->
    Term = probe(),
    live_after_gc(Term).

live_after_gc(Term) ->
    _ = erlang:garbage_collect(),
    _ = erlang:yield(),
    Count = dtor_count(),
    %% Keep the term reachable past the count read.
    _ = erlang:phash2(Term),
    Count.

settle() ->
    settle(-1, 0).

settle(_Previous, Round) when Round >= 200 ->
    dtor_count();
settle(Previous, Round) ->
    _ = erlang:garbage_collect(),
    _ = erlang:yield(),
    case dtor_count() of
        Previous -> Previous;
        Current -> settle(Current, Round + 1)
    end.
"#
    )
}

/// Run the oracle module's `run/0` under the **system** Erlang, returning the printed line.
fn oracle_run(so: &Path) -> Option<String> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem);
    let nif_path = nif_path.to_str()?;

    let erl_file = dir.join(format!("{MODULE}.erl"));
    std::fs::write(&erl_file, oracle_module(nif_path)).expect("the oracle module is writable");

    let compiled = Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .arg(&erl_file)
        .output()
        .expect("erlc is runnable (probed above)");
    assert!(
        compiled.status.success(),
        "erlc failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let expr = format!(
        "io:format(\"~w~n\", [{MODULE}:run()]),\nhalt(0).\n"
    );
    let out = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-eval")
        .arg(&expr)
        .output()
        .expect("erl is runnable (probed above)");
    assert!(
        out.status.success(),
        "the oracle run failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string);
    println!("oracle: {line:?}");
    line
}

/// The oracle's own counts — the constants the Rust test above asserts, measured on the real runtime.
///
/// Note the difference in *mechanism*, which is why only counts are compared: the oracle has to force
/// a collection and settle the deferred destructor at every step, while the seam is handed the
/// reachability decision outright.
#[test]
fn the_oracle_needs_a_collection_the_seam_replaces() {
    let Some(so) = compile_fixture() else { return };
    let Some(line) = oracle_run(&so) else { return };
    assert_eq!(
        line, ORACLE_RUN,
        "the measured oracle destructor counts (fresh, dropped, kept, erased, never-exported)"
    );
}
