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

//! End-to-end proof of `enif_priv_data/1` (workspace contract A.3): the private pointer the NIF
//! library's own `load` callback stored.
//!
//! `tests/fixtures/nif_priv_data.c` is a real C NIF — compiled at test time with the system `cc`
//! against the system `erl_nif.h` — whose `load` callback records the address of a global holding
//! `11` and whose `priv_value/0` reads that int back through `enif_priv_data(env)`. The `.so` leaves
//! `enif_priv_data` (and the term helpers) **undefined**, bound at `dlopen(RTLD_NOW)` against this
//! test binary's dynamic symbol table (`build.rs` adds `-Wl,--export-dynamic`), so a successful load
//! already proves the `#[no_mangle]` definition exists and is callable.
//!
//! # The oracle's contract reproduced here
//!
//! `void* enif_priv_data(ErlNifEnv* env) { return env->mod_nif->priv_data; }`
//! (`erts/emulator/beam/erl_nif.c:609-612`) — the private pointer of the **library the call belongs
//! to**, i.e. what that library's `load` callback wrote through `void** priv_data` (the runtime keeps
//! it in `lib->priv_data`, `erl_nif.c:5003`). It is therefore independent of the host: the tests
//! below pin that the library's own pointer is what a NIF reads, that a host hook cannot override it
//! during a call, and that [`beam_abi::NifHost::priv_data`] remains the source when **no** library
//! call is in progress.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing. The oracle test additionally needs
//! `erl`/`erlc` and skips loudly without them. When a toolchain **is** present every assertion is
//! hard.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::enif::enif_priv_data;
use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::{NifEnv, NifHost};
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::compare;
use beam_term::prelude::{make_atom, make_small, Eterm};
use libc::c_void;

use common::{compile_so, skip, TestHost};

/// The committed C fixture (see the file header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_priv_data.c");

/// The fixture's `ERL_NIF_INIT` name — the loader (and the oracle) require it to match the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const MODULE: &str = "nif_priv_data";

/// The value the fixture's `load` callback made its private pointer point at.
const PRIV_VALUE: i64 = 11;

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

/// Call `name/0` through the loaded library and assert it returned the small integer `want`.
#[track_caller]
fn assert_small(env: &mut NifEnv, lib: &NifLibrary, name: &str, want: i64) {
    let got = lib
        .call(env, name, 0, &[])
        .unwrap_or_else(|err| panic!("{name}/0 must be callable: {err:?}"));
    assert!(
        compare::eq(got, make_small(want), &AtomTable::new()),
        "{name}/0: got {got:?}, want {want}"
    );
}

/// A host that manages private data itself: [`NifHost::priv_data`] answers `marker` (the trait's
/// only non-default hook this file needs; the rest keep their defaults).
struct PrivHost {
    heap: Heap,
    atoms: AtomTable,
    marker: *mut c_void,
}

impl PrivHost {
    fn new(marker: *mut c_void) -> PrivHost {
        PrivHost {
            heap: Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            marker,
        }
    }
}

impl NifHost for PrivHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !beam_term::prelude::is_atom(atom) {
            return None;
        }
        self.atoms
            .name(beam_term::prelude::atom_val(atom))
            .map(str::to_string)
    }

    fn priv_data(&mut self) -> *mut c_void {
        self.marker
    }
}

// ---------------------------------------------------------------------------------------------
// The Rust runtime
// ---------------------------------------------------------------------------------------------

/// The acceptance case: a host that leaves `NifHost::priv_data` at its default (null), a library
/// whose `load` callback stored `&priv`, and a NIF reading `enif_priv_data` back through the C ABI.
///
/// The only possible source of the non-null pointer inside the call is the **library**, so `11`
/// proves `enif_priv_data` answers `env->mod_nif->priv_data`. The same assertion after the call
/// returns pins the other half: the library's pointer is withdrawn, and the host hook (null) is the
/// source again.
#[test]
fn the_load_callbacks_private_pointer_reaches_the_c_nif() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = TestHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    assert!(
        !lib.priv_data().is_null(),
        "the fixture's `load` callback must have stored a pointer"
    );

    let mut env = NifEnv::new(&mut host);
    assert_small(&mut env, &lib, "priv_is_set", 1);
    assert_small(&mut env, &lib, "priv_value", PRIV_VALUE);
    // SAFETY: `env` is a live env.
    let after = unsafe { enif_priv_data(env.as_ptr()) };
    assert!(
        after.is_null(),
        "the library's pointer must be withdrawn once the call returns, \
         so the default host hook answers: {after:?}"
    );
}

/// A host **with** private data of its own cannot override the library's pointer during a call: the
/// library is authoritative for its own `priv_data` (`lib->priv_data`), and the host hook is only
/// the no-call fallback.
#[test]
fn the_host_hook_does_not_override_the_library_pointer_during_a_call() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let marker = 0x2a2ausize as *mut c_void;
    let mut host = PrivHost::new(marker);
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load against a host that manages private data itself");
    assert_ne!(
        lib.priv_data(),
        marker,
        "the fixture stores its own pointer, so the host marker is a distinguishable value"
    );

    let mut env = NifEnv::new(&mut host);
    // The library's pointer, not the host's marker: `enif_priv_data` reads `env->mod_nif->priv_data`
    // inside the call, so `11` comes back through the library's own `&priv`.
    assert_small(&mut env, &lib, "priv_value", PRIV_VALUE);
    // The host hook still answers for a caller outside any library call.
    // SAFETY: `env` is a live env.
    let outside = unsafe { enif_priv_data(env.as_ptr()) };
    assert_eq!(outside, marker, "the host hook is the no-call source");
}

/// No library call in progress: the host hook is the only remaining source — a host that manages its
/// own private data keeps working, and a default host answers NULL (the C's answer for a call env
/// whose `load` stored nothing).
#[test]
fn outside_a_library_call_the_host_hook_is_the_source() {
    let marker = 0x7f7fusize as *mut c_void;
    let mut host = PrivHost::new(marker);
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is a live env.
    let got = unsafe { enif_priv_data(env.as_ptr()) };
    assert_eq!(got, marker, "with no library call, the host hook answers");
    drop(env);

    let mut host = PrivHost::new(core::ptr::null_mut());
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is a live env.
    assert!(unsafe { enif_priv_data(env.as_ptr()) }.is_null());
}

/// A null env answers NULL (the C dereferences `env->mod_nif` and faults).
#[test]
fn priv_data_of_a_null_env_is_null() {
    // SAFETY: a null env is explicitly allowed by the contract.
    assert!(unsafe { enif_priv_data(core::ptr::null_mut()) }.is_null());
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Whether `cmd` can be spawned with `args` at all (the probe for the oracle toolchain; a non-zero
/// exit is fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// Load the fixture into the **system** Erlang and print the two results, one per line.
///
/// Returns `None` (after a loud skip) when `erl`/`erlc` are unavailable or the generated module could
/// not be built; a *runtime* failure of the Erlang side is a hard error, because that would mean the
/// fixture is not a valid NIF rather than that the toolchain is missing.
fn oracle_print(so: &Path) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem);
    let nif_path = nif_path.to_str()?;

    // The oracle module: the stub bodies are replaced by the NIFs on `load_nif`. The compiler marks
    // every function `nif_start` because `init/0` calls `load_nif/2`, which is what makes the
    // replacement possible.
    let erl = format!(
        "-module({MODULE}).\n\
         -export([priv_value/0, priv_is_set/0]).\n\
         -on_load(init/0).\n\n\
         init() ->\n    erlang:load_nif(\"{nif_path}\", 0).\n\n\
         priv_value() -> erlang:nif_error(nif_not_loaded).\n\
         priv_is_set() -> erlang:nif_error(nif_not_loaded).\n"
    );
    let erl_file = dir.join(format!("{MODULE}.erl"));
    std::fs::write(&erl_file, erl).expect("the oracle module is writable");

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
        "io:format(\"~w~n\", [{MODULE}:priv_value()]),\n\
         io:format(\"~w~n\", [{MODULE}:priv_is_set()]),\n\
         halt(0).\n"
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

    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// The system Erlang, running the very same `.so`, must report the pointer **its** `load` callback
/// stored — the value this runtime's assertions use, so the two agree.
#[test]
fn the_same_fixture_agrees_under_the_erl_oracle() {
    let Some(so) = compile_fixture() else { return };
    let Some(lines) = oracle_print(&so) else { return };

    assert_eq!(
        lines,
        vec![PRIV_VALUE.to_string(), "1".to_string()],
        "the system Erlang must report the private pointer its `load` callback stored"
    );
}
