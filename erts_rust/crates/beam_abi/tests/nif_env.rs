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

//! End-to-end proof of the **NIF env / select / existing-atom** slots (workspace contract A.3).
//!
//! `tests/fixtures/nif_env.c` is a real C NIF — compiled at test time with the system `cc` against
//! the system `erl_nif.h` — whose `enif_make_existing_atom(_len)`, `enif_select`, `enif_select_x`,
//! `enif_priv_data`, `enif_make_copy` and `enif_alloc_env` calls are **undefined** in the `.so` and
//! are bound at `dlopen(RTLD_NOW)` against this test binary's dynamic symbol table (`build.rs` adds
//! `-Wl,--export-dynamic`). So a successful load already proves those `#[no_mangle]` definitions
//! exist and are callable; the assertions below then pin the *observable behaviour* of each.
//!
//! # The oracle's contracts reproduced here
//!
//! * `enif_make_existing_atom(_len)` — `erts_atom_get` (`erts/emulator/beam/atom.c:383-435`):
//!   `res = i < 0 ? 0 : (*ap = make_atom(i), 1)`, i.e. **1 and the atom written on a hit, 0 and the
//!   out-pointer untouched on a miss**; it never creates the atom. The `_len` form honours the
//!   explicit length, so a name need not be NUL-terminated.
//! * `enif_select`/`enif_select_x` — `erts/emulator/sys/common/erl_check_io.c`: a negative event is
//!   `INT_MIN | ERL_NIF_SELECT_INVALID_EVENT` (`:1263-1268`), decided before any fd state is
//!   touched; a serviced registration returns the host's non-negative code (`:1464`, `:1467`), and
//!   an unserviceable one is negative (the documented `retval < 0` failure idiom).
//! * `enif_priv_data` — `env->mod_nif->priv_data` (`erl_nif.c:609-612`), the pointer the library's
//!   `load` callback stored; the host's [`beam_abi::NifHost::priv_data`] hook is the source only when
//!   no library call is in progress.
//! * `enif_make_copy` — `make_copy(dst_env, src_term, NULL)` (`erl_nif.c:1194-1197`). With no copy
//!   engine behind the `NifHost::make_copy` hook this crate answers `THE_NON_VALUE` (raw 0) — never
//!   a term aliasing the source env's memory.
//! * `enif_alloc_env` — `erts_alloc_fnf(ERTS_ALC_T_NIF, …)` + `setup_nif_env` (`erl_nif.c:669-675`).
//!   This crate delegates the allocation to `NifHost::alloc_env` on the host of the **current NIF
//!   call** (the thread-local stack `NifEnv::new` publishes and `NifEnv`'s `Drop` withdraws), so a
//!   supporting host's own pointer comes back verbatim, and a host without support — or a call made
//!   outside any NIF call — is `NULL` (never a crate-owned env with unusable atoms).
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing. When the toolchain **is** present every
//! assertion is hard.

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use beam_abi::common::{ErlNifSelectFlags, ERL_NIF_SELECT_INVALID_EVENT};
use beam_abi::nif::enif::{enif_alloc_env, enif_free_env, enif_priv_data};
use beam_abi::nif::loader::load;
use beam_abi::nif::types::{ErlNifEnv, ErlNifEvent};
use beam_abi::{NifEnv, NifHost};
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::compare;
use beam_term::prelude::{is_atom, make_atom, make_small, Eterm, NIL};
use libc::{c_int, c_void};

use common::{compile_so, skip};

/// The committed C fixture (see the file header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_env.c");

/// The fixture's `ERL_NIF_INIT` name — the loader requires it to match the calling module.
const MODULE: &str = "test_nif_env";

// ---------------------------------------------------------------------------------------------
// The test host
// ---------------------------------------------------------------------------------------------

/// The pointer the host reports from `priv_data` when it has private data.
static PRIV_MARKER: u8 = 0;

/// A host whose `existing_atom`/`priv_data`/`select`/`alloc_env` hooks are test-controlled, so the
/// `enif_*` layer's delegation is observable end-to-end.
struct EnvHost {
    heap: Heap,
    atoms: AtomTable,
    /// The code `select` answers (`None` = unsupported, the trait default).
    select_code: Option<c_int>,
    /// Whether the host has private data (`enif_priv_data` non-NULL).
    with_priv: bool,
    /// What `alloc_env` answers (`None` = unsupported, the trait default).
    env_ptr: Option<*mut ErlNifEnv>,
    /// Whether `free_env` claims to own (and so "free") the env it is handed.
    owns_env: bool,
    /// Every pointer `free_env` has been called with, in call order (shared with the test, because
    /// the host is mutably borrowed by its `NifEnv` for the whole test).
    freed_envs: Rc<RefCell<Vec<*mut ErlNifEnv>>>,
}

impl EnvHost {
    fn new(select_code: Option<c_int>, with_priv: bool) -> EnvHost {
        EnvHost {
            heap: Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            select_code,
            with_priv,
            env_ptr: None,
            owns_env: false,
            freed_envs: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Give the host a message-env allocation to hand out from `alloc_env`.
    ///
    /// The pointer is a bare sentinel: a host cannot build a genuine `NifEnv` for itself through
    /// this crate's public API, and the contract of `enif_alloc_env` is "the *host's* env", so the
    /// tests below pin the pointer **identity** the hook promised.
    fn with_alloc_env(mut self, ptr: *mut ErlNifEnv) -> EnvHost {
        self.env_ptr = Some(ptr);
        self
    }

    /// Claim ownership of the envs this host is handed: `free_env` then reports `true`.
    fn owning(mut self) -> EnvHost {
        self.owns_env = true;
        self
    }

    /// A handle on the `free_env` call log, taken **before** a `NifEnv` borrows this host.
    fn free_log(&self) -> Rc<RefCell<Vec<*mut ErlNifEnv>>> {
        Rc::clone(&self.freed_envs)
    }
}

impl NifHost for EnvHost {
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
        self.atoms
            .name(beam_term::prelude::atom_val(atom))
            .map(str::to_string)
    }

    /// A **pure lookup** — the crate's stand-in for `erts_atom_get` (it never interns).
    fn existing_atom(&mut self, name: &str) -> Option<Eterm> {
        self.atoms.lookup(name).map(make_atom)
    }

    fn priv_data(&mut self) -> *mut c_void {
        if self.with_priv {
            &PRIV_MARKER as *const u8 as *mut c_void
        } else {
            core::ptr::null_mut()
        }
    }

    fn alloc_env(&mut self) -> Option<*mut ErlNifEnv> {
        self.env_ptr
    }

    /// Record the delivered pointer and report ownership (the C `erts_free`s what `enif_alloc_env`
    /// allocated; here only a host that claims the env frees it).
    fn free_env(&mut self, env: *mut ErlNifEnv) -> bool {
        self.freed_envs.borrow_mut().push(env);
        self.owns_env
    }

    fn select(
        &mut self,
        _event: ErlNifEvent,
        _mode: ErlNifSelectFlags,
        _obj: *mut c_void,
        _pid: Option<Eterm>,
        _reference: Eterm,
    ) -> Option<c_int> {
        self.select_code
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
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

/// A host plus the fixture loaded against it (`None` only on a hard load failure).
fn loaded(
    path: &str,
    select_code: Option<c_int>,
    with_priv: bool,
) -> (EnvHost, beam_abi::nif::loader::NifLibrary) {
    let mut host = EnvHost::new(select_code, with_priv);
    let lib = load(path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    (host, lib)
}

/// Assert the term `got` is structurally equal to `want` (atoms compare by identity).
#[track_caller]
fn assert_same(got: Eterm, want: Eterm, ctx: &str) {
    assert!(
        compare::eq(got, want, &AtomTable::new()),
        "{ctx}: expected {want:?}, got {got:?}"
    );
}

/// The C's `INT_MIN | ERL_NIF_SELECT_INVALID_EVENT` as an Erlang small integer.
fn invalid_event_code() -> Eterm {
    make_small((c_int::MIN | ERL_NIF_SELECT_INVALID_EVENT) as i64)
}

// ---------------------------------------------------------------------------------------------
// enif_make_existing_atom / _len
// ---------------------------------------------------------------------------------------------

#[test]
fn existing_atom_hit_miss_and_len_through_a_real_c_nif() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let (mut host, lib) = loaded(&path, None, false);
    let mut env = NifEnv::new(&mut host);
    let hello = env.host().intern_atom("hello").expect("interns");

    // HIT: {1, hello}.
    let got = lib
        .call(&mut env, "existing_hit", 0, &[])
        .expect("existing_hit/0");
    let want = env
        .heap()
        .tuple(&[make_small(1), hello])
        .expect("a 2-tuple fits the test heap");
    assert_same(got, want, "existing_hit/0");

    // MISS: {0, 12345} — 0 *and* the out-pointer keeps the caller's sentinel.
    let got = lib
        .call(&mut env, "existing_miss", 0, &[])
        .expect("existing_miss/0");
    let want = env
        .heap()
        .tuple(&[make_small(0), make_small(12345)])
        .expect("a 2-tuple fits the test heap");
    assert_same(got, want, "existing_miss/0 (0 and *atom untouched)");

    // The name was not created by the miss.
    assert!(
        env.host().existing_atom("beam_abi_absent_atom").is_none(),
        "enif_make_existing_atom must never intern"
    );

    // `_len`: an explicit 5 bytes of "helloX" is the existing atom `hello` => {1, hello}.
    let got = lib
        .call(&mut env, "existing_len", 0, &[])
        .expect("existing_len/0");
    let want = env
        .heap()
        .tuple(&[make_small(1), hello])
        .expect("a 2-tuple fits the test heap");
    assert_same(got, want, "existing_len/0 (explicit length)");
}

// ---------------------------------------------------------------------------------------------
// enif_select / enif_select_x
// ---------------------------------------------------------------------------------------------

#[test]
fn select_reproduces_the_cs_invalid_event_and_failure_codes() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    // A host that cannot service select (the trait default) => every valid request is -1.
    let (mut host, lib) = loaded(&path, None, false);
    {
        let mut env = NifEnv::new(&mut host);
        assert_same(
            lib.call(&mut env, "select_bad", 0, &[]).expect("select_bad/0"),
            invalid_event_code(),
            "enif_select(-1) is INT_MIN | ERL_NIF_SELECT_INVALID_EVENT",
        );
        assert_same(
            lib.call(&mut env, "select_x_bad", 0, &[]).expect("select_x_bad/0"),
            invalid_event_code(),
            "enif_select_x(-1) takes the same invalid-event path",
        );
        assert_same(
            lib.call(&mut env, "select_noservice", 0, &[])
                .expect("select_noservice/0"),
            make_small(-1),
            "an unserviceable select is negative (the C's `retval < 0` idiom)",
        );
    }

    // A host that services the request passes its code through verbatim.
    let (mut host, lib) = loaded(&path, Some(0), false);
    {
        let mut env = NifEnv::new(&mut host);
        assert_same(
            lib.call(&mut env, "select_x_ok", 0, &[]).expect("select_x_ok/0"),
            make_small(0),
            "a serviced enif_select_x returns the host's code",
        );
        assert_same(
            lib.call(&mut env, "select_noservice", 0, &[])
                .expect("select_noservice/0"),
            make_small(0),
            "a serviced enif_select returns 0",
        );
    }
}

// ---------------------------------------------------------------------------------------------
// enif_priv_data / enif_make_copy
// ---------------------------------------------------------------------------------------------

#[test]
fn priv_data_is_the_library_pointer_and_the_host_is_the_no_call_fallback() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    // The fixture's `load` callback stored `&priv_marker`, so inside a call `enif_priv_data` answers
    // that pointer — `env->mod_nif->priv_data` — whatever the host hook says.
    let (mut host, lib) = loaded(&path, None, true);
    {
        let mut env = NifEnv::new(&mut host);
        assert_same(
            lib.call(&mut env, "priv_data", 0, &[]).expect("priv_data/0"),
            make_small(1),
            "enif_priv_data reports the library's stored private pointer",
        );
        assert_same(
            lib.call(&mut env, "make_copy", 0, &[]).expect("make_copy/0"),
            make_small(0),
            "with no copy engine the answer is THE_NON_VALUE",
        );
        // Outside a call the library is not in flight, so the host hook is the source: non-NULL on a
        // host that has private data of its own...
        // SAFETY: `env` is a live env.
        assert!(!unsafe { enif_priv_data(env.as_ptr()) }.is_null());
    }

    // ...and NULL on a host without private data.
    let (mut host, _lib) = loaded(&path, None, false);
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is a live env.
    assert!(
        unsafe { enif_priv_data(env.as_ptr()) }.is_null(),
        "a host without private data answers NULL outside a library call"
    );
}

// ---------------------------------------------------------------------------------------------
// enif_alloc_env — the host of the current NIF call, reached from a void entry point
// ---------------------------------------------------------------------------------------------

/// `enif_alloc_env(void)` reaches the host of the **current NIF call**: the pointer that host handed
/// out comes back through the C ABI, and a host without `alloc_env` support (the trait default)
/// yields `NULL`.
///
/// The returned pointer is a host-owned allocation this crate never dereferences (a test host cannot
/// produce a genuine `NifEnv` through the public API), so the assertion is **identity** — which is
/// exactly what `enif_alloc_env` promises: "the host's env", with the host's atom table behind it
/// (see `src/nif/enif/envs.rs`). Nothing is freed: the sentinel is not an allocation this crate or
/// the host made, and the C fixture never touches it either.
#[test]
fn alloc_env_reaches_the_current_host_through_a_real_c_nif() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    // A host with NO `alloc_env` support: `enif_alloc_env()` is NULL inside the call too.
    let mut host = EnvHost::new(None, false);
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    {
        let mut env = NifEnv::new(&mut host);
        assert_same(
            lib.call(&mut env, "alloc_env", 0, &[]).expect("alloc_env/0"),
            make_small(0),
            "a host with no alloc_env support answers NULL",
        );
    }

    // A host WITH support: the host's own pointer comes back verbatim.
    let want: *mut ErlNifEnv = 0x0000_1234_5678_9abcu64 as usize as *mut ErlNifEnv;
    let mut host = EnvHost::new(None, false).with_alloc_env(want);
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    {
        let mut env = NifEnv::new(&mut host);
        assert_same(
            lib.call(&mut env, "alloc_env", 0, &[]).expect("alloc_env/0"),
            make_small(want as usize as i64),
            "the host's env pointer comes back through the C ABI",
        );
    }

    // Outside the call (the env is dropped) there is no host any more, so the answer is NULL again —
    // the current-host stack never keeps a stale host alive.
    // SAFETY: no env is live on this thread here, which is exactly what the call reports.
    assert!(unsafe { enif_alloc_env() }.is_null());
}

// ---------------------------------------------------------------------------------------------
// enif_free_env — the symbol is real and never frees memory the host does not own
// ---------------------------------------------------------------------------------------------

/// `enif_free_env` is driven directly rather than from the fixture: a C `enif_free_env` call is only
/// meaningful on an env that came from `enif_alloc_env`, and the test hosts above hand out a bare
/// sentinel rather than a real allocation (`alloc_env/0` only reports that pointer's identity). The
/// symbol is `Some` in the table and exported, so what is asserted is its contract on the two
/// pointers that reach it today — null, and an env the host does not own.
#[test]
fn enif_free_env_is_null_safe_and_never_frees_a_foreign_env() {
    let mut host = EnvHost::new(None, false);
    // SAFETY: a null env is explicitly allowed by the contract.
    unsafe { enif_free_env(core::ptr::null_mut()) };

    let mut env = NifEnv::new(&mut host);
    assert!(!env.as_ptr().is_null());
    // The per-call env is not owned by the host (`free_env` reports `false`), so this is a no-op:
    // the C would `erts_free` it, which here would corrupt the test's own stack.
    // SAFETY: `env` is a live env for the rest of the test.
    unsafe { enif_free_env(env.as_ptr()) };
    let atom = env.host().intern_atom("still_usable").expect("interns");
    assert!(is_atom(atom), "the env is untouched by a no-op free");
    assert_eq!(NIL, NIL);
}

// ---------------------------------------------------------------------------------------------
// The message-env lifecycle: alloc -> use -> free, plus the no-op protocol
// ---------------------------------------------------------------------------------------------

/// The **message-env lifecycle** and its whole observable protocol, driven through the public API.
///
/// It needs no C toolchain (a host here can hand out a *real* `NifEnv` a test owns), so it always
/// runs — where the fixture-driven tests above skip without `cc`.
///
/// What it pins, in the order the C contract prescribes (`erl_nif.c:669-680`):
/// * `enif_alloc_env()` returns the **current NIF call's host's** env (here: the message env the call
///   host hands out), and that pointer is a genuinely usable env — its host's atom table included;
/// * `enif_free_env(p)` hands `p` back to the host that **owns** it — the env `p` designates, exactly
///   as the C frees in the allocator that made it — once, with the exact pointer;
/// * `enif_free_env(NULL)` is a no-op that reaches no host;
/// * `enif_free_env` on an env the host does not own is a refusal (`false`), so nothing is freed and
///   the env stays usable;
/// * outside a NIF call (no live env on this thread) `enif_alloc_env()` is `NULL`.
#[test]
fn message_env_lifecycle_alloc_use_free_and_the_no_op_protocol() {
    // The message env is a REAL env over its own host (a host cannot build a genuine `NifEnv` for
    // itself through the public API), so the "use" step below is a genuine atom intern.
    let mut msg_host = EnvHost::new(None, false).owning();
    let msg_freed = msg_host.free_log();
    let mut msg_env = NifEnv::new(&mut msg_host);
    let msg_ptr = msg_env.as_ptr();

    // The call env's host is the one `enif_alloc_env` reaches; it does NOT own message envs.
    let mut host = EnvHost::new(None, false).with_alloc_env(msg_ptr);
    let call_freed = host.free_log();
    let mut call_env = NifEnv::new(&mut host);
    let call_ptr = call_env.as_ptr();

    // --- alloc: the current call's host hands out its own env -------------------------------
    // SAFETY: `call_env` is live, so a host is current.
    let p = unsafe { enif_alloc_env() };
    assert_eq!(p, msg_ptr, "`enif_alloc_env` returns the host's own env");

    // --- use: the hand-out pointer is a real, usable env ------------------------------------
    {
        // SAFETY: `p` is `msg_env.as_ptr()`, and `msg_env` is alive here.
        let recovered = unsafe { NifEnv::from_opaque(p) }.expect("non-null");
        let atom = recovered
            .host()
            .intern_atom("message_env_atom")
            .expect("the message env's host interns");
        assert_eq!(
            recovered.host().atom_name(atom).as_deref(),
            Some("message_env_atom"),
            "the message env is usable, atoms included"
        );
    }

    // --- free: handed back to the owning host, once, with the exact pointer ------------------
    // SAFETY: `p` came from `enif_alloc_env` — the C contract for `enif_free_env`.
    unsafe { enif_free_env(p) };
    assert_eq!(
        *msg_freed.borrow(),
        vec![msg_ptr],
        "the owning host is asked exactly once, for the exact pointer"
    );

    // --- free(NULL) is a no-op: no host is consulted -----------------------------------------
    // SAFETY: a null env is explicitly allowed by the contract.
    unsafe { enif_free_env(core::ptr::null_mut()) };
    assert_eq!(msg_freed.borrow().len(), 1, "a NULL free reaches no host");

    // --- free on an env the host does NOT own: refused, and the env stays usable -------------
    // SAFETY: `call_env` is a live env.
    unsafe { enif_free_env(call_ptr) };
    assert_eq!(
        *call_freed.borrow(),
        vec![call_ptr],
        "the non-owning host saw the pointer and refused it"
    );
    let atom = call_env.host().intern_atom("still_usable").expect("interns");
    assert!(is_atom(atom), "a refused free leaves the env untouched");

    // --- outside a NIF call there is no current host, so the answer is NULL ------------------
    drop(call_env);
    drop(msg_env);
    // SAFETY: no env is live on this thread here, which is exactly what the call reports.
    assert!(unsafe { enif_alloc_env() }.is_null());
}
